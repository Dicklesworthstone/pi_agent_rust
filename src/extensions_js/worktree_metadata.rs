//! Host-owned worktree identity for context-mode's documented suffix override.
//!
//! Only a provenance-validated context-mode package reaches `prepare`. These
//! fixed metadata queries do not expose another guest process API or change
//! the opt-in boundary for general synchronous child-process execution.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use sha2::{Digest as _, Sha256};

use super::PiJsRuntimeConfig;
use crate::agent_cx::AgentCx;
use crate::extensions::{
    ExecMediationResult, ExtensionPolicy, PolicyDecision, evaluate_exec_mediation, strip_unc_prefix,
};
use crate::tools::{ProcessCleanupMode, ProcessGuard};

const SUFFIX_KEY: &str = "CONTEXT_MODE_SESSION_SUFFIX";
const QUERY_BYTES: usize = 64 * 1024;
const QUERY_TIMEOUT: Duration = Duration::from_secs(2);
type Reader = JoinHandle<Result<Vec<u8>, String>>;

/// Supply a single identity to the extension and its inherited MCP environment.
/// Permission/refusal skips preserve loading; query failures never invent an
/// empty override, which would explicitly disable upstream worktree isolation.
pub(super) fn prepare(
    config: &mut PiJsRuntimeConfig,
    policy: &ExtensionPolicy,
    extension_id: &str,
    root_deadline: Instant,
) -> Result<(), String> {
    let windows = super::uses_windows_environment(&config.env);
    if super::configured_environment_value(&config.env, SUFFIX_KEY, windows).is_some()
        || config.deny_env
        || policy.secret_broker.is_secret(SUFFIX_KEY)
        || ["env", "read", "exec"].iter().any(|capability| {
            policy.evaluate_for(capability, Some(extension_id)).decision != PolicyDecision::Allow
        })
    {
        return Ok(());
    }

    let budget = QueryBudget::new(root_deadline)?;
    let requested_workspace = super::effective_workspace_cwd(config, Some(policy));
    let workspace = Path::new(requested_workspace)
        .canonicalize()
        .map(strip_unc_prefix)
        .map_err(|error| format!("Cannot resolve context-mode workspace: {error}"))?;
    if !workspace.is_dir() {
        return Err("Context-mode workspace is not a directory".to_string());
    }
    budget.check()?;
    let executable = trusted_git_executable()
        .ok_or("No trusted system Git executable is available for worktree metadata")?;
    let current = budget.git(
        &executable,
        &workspace,
        requested_workspace,
        GitQuery::CurrentRoot,
        policy,
    )?;
    let main = budget.git(
        &executable,
        &workspace,
        requested_workspace,
        GitQuery::Worktrees,
        policy,
    )?;
    let current = canonical_hash_path(current_root(&current)?)?;
    let main = canonical_hash_path(main_root(&main)?)?;
    budget.check()?;
    let suffix = if current == main {
        String::new()
    } else {
        super::hex_lower(&Sha256::digest(current.as_bytes()))[..8].to_string()
    };
    // No guest code runs during preparation, but keep the Windows snapshot
    // invariant explicit: insertion must never introduce a case alias.
    if windows {
        config
            .env
            .retain(|name, _| !name.eq_ignore_ascii_case(SUFFIX_KEY));
    }
    config.env.insert(SUFFIX_KEY.to_string(), suffix);
    Ok(())
}

#[derive(Clone, Copy)]
enum GitQuery {
    CurrentRoot,
    Worktrees,
}

impl GitQuery {
    const fn arguments(self) -> &'static [&'static str] {
        match self {
            Self::CurrentRoot => &["rev-parse", "--show-toplevel"],
            Self::Worktrees => &["worktree", "list", "--porcelain"],
        }
    }
}

struct QueryBudget {
    owner: AgentCx,
    deadline: Instant,
}

impl QueryBudget {
    fn new(root_deadline: Instant) -> Result<Self, String> {
        let owner = AgentCx::for_current_or_request();
        let capabilities = owner.capabilities();
        if !capabilities.io || !capabilities.spawn || !capabilities.time {
            return Err(
                "Worktree metadata requires host I/O, process and timer authority".to_string(),
            );
        }
        let now = owner
            .timer_driver()
            .map_or_else(asupersync::time::wall_now, |timer| timer.now());
        let inherited = owner.budget().deadline.map_or(QUERY_TIMEOUT, |deadline| {
            Duration::from_nanos(deadline.as_nanos().saturating_sub(now.as_nanos()))
        });
        let budget = Self {
            owner,
            deadline: root_deadline.min(Instant::now() + inherited.min(QUERY_TIMEOUT)),
        };
        budget.check()?;
        Ok(budget)
    }

    fn check(&self) -> Result<(), String> {
        self.owner
            .checkpoint()
            .map_err(|_| "Worktree metadata query cancelled".to_string())?;
        if Instant::now() >= self.deadline {
            return Err("Worktree metadata query exceeded its shared deadline".to_string());
        }
        Ok(())
    }

    fn git(
        &self,
        executable: &Path,
        workspace: &Path,
        requested_workspace: &str,
        query: GitQuery,
        policy: &ExtensionPolicy,
    ) -> Result<Vec<u8>, String> {
        self.check()?;
        let workspace = workspace
            .to_str()
            .ok_or("Worktree metadata workspace is not UTF-8")?;
        let executable = executable
            .to_str()
            .ok_or("Worktree metadata executable is not UTF-8")?;
        let operation_args: Vec<_> = query
            .arguments()
            .iter()
            .map(|arg| (*arg).to_string())
            .collect();
        mediate(policy, "git", &operation_args)?;
        let mut visible_args = vec!["-C".to_string(), requested_workspace.to_string()];
        visible_args.extend(operation_args);
        mediate(policy, "git", &visible_args)?;
        visible_args[1] = workspace.to_string();
        mediate(policy, "git", &visible_args)?;

        let mut args = vec![
            "--no-pager".to_string(),
            "-c".to_string(),
            "core.fsmonitor=false".to_string(),
            "-c".to_string(),
            "core.untrackedCache=false".to_string(),
        ];
        args.extend(visible_args);
        if matches!(query, GitQuery::Worktrees) {
            // The host parses NUL records so path newlines cannot be mistaken
            // for another worktree. The visible upstream form was checked too.
            args.push("-z".to_string());
        }
        mediate(policy, executable, &args)?;
        let mut command =
            crate::tools::command_with_default_sigpipe_in_dir(executable, Path::new(workspace))
                .map_err(|error| format!("Cannot construct worktree metadata query: {error}"))?;
        command.args(args).current_dir(workspace);
        configure_environment(&mut command);
        self.capture(command)
    }

    fn capture(&self, mut command: Command) -> Result<Vec<u8>, String> {
        self.check()?;
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        crate::tools::isolate_command_process_group(&mut command);
        let child = self
            .owner
            .process()
            .spawn_checked(&mut command)
            .map_err(|error| format!("Cannot start worktree metadata query: {error}"))?;
        let mut child = ProcessGuard::new(child, ProcessCleanupMode::ProcessGroupTree);
        let stdout = child
            .take_stdout()
            .ok_or("Git metadata stdout unavailable")?;
        let stderr = child
            .take_stderr()
            .ok_or("Git metadata stderr unavailable")?;
        let mut stdout_reader = match spawn_reader(stdout) {
            Ok(reader) => Some(reader),
            Err(error) => {
                let _ = child.terminate_and_wait();
                return Err(error);
            }
        };
        let mut stderr_reader = match spawn_reader(stderr) {
            Ok(reader) => Some(reader),
            Err(error) => {
                let _ = child.terminate_and_wait();
                let _ = join_reader(stdout_reader.take().expect("stdout reader"));
                return Err(error);
            }
        };
        let outcome = (|| {
            let mut stdout = None;
            let mut stderr = None;
            let mut status = None;
            loop {
                self.check()?;
                if stdout_reader.as_ref().is_some_and(JoinHandle::is_finished) {
                    stdout = Some(join_reader(stdout_reader.take().expect("stdout reader"))?);
                }
                if stderr_reader.as_ref().is_some_and(JoinHandle::is_finished) {
                    stderr = Some(join_reader(stderr_reader.take().expect("stderr reader"))?);
                }
                if status.is_none()
                    && let Some(exited) = child
                        .try_wait_child()
                        .map_err(|error| format!("Cannot poll worktree metadata query: {error}"))?
                {
                    status = Some(exited);
                    // Retire descendants even when the root exits first; they
                    // must not keep a pipe reader or Windows Job alive.
                    child
                        .terminate_and_wait()
                        .map_err(|error| format!("Cannot reap worktree metadata query: {error}"))?;
                }
                if let Some(status) = status
                    && stdout.is_some()
                    && stderr.is_some()
                {
                    if !status.success() {
                        // Do not disclose arbitrary local Git configuration
                        // diagnostics through an environment metadata binding.
                        return Err("Git could not inspect the selected worktree".to_string());
                    }
                    return Ok(stdout.expect("completed stdout"));
                }
                std::thread::sleep(
                    Duration::from_millis(5)
                        .min(self.deadline.saturating_duration_since(Instant::now())),
                );
            }
        })();
        let _ = child.terminate_and_wait();
        // Readers retain at most QUERY_BYTES + 1 output bytes. Cleanup owns both workers
        // until pipe EOF, including timeout, cancellation and overflow exits.
        for reader in [stdout_reader, stderr_reader].into_iter().flatten() {
            let _ = join_reader(reader);
        }
        outcome
    }
}

fn spawn_reader(reader: impl std::io::Read + Send + 'static) -> Result<Reader, String> {
    std::thread::Builder::new()
        .name("context-mode-git-output".to_string())
        .spawn(move || {
            let mut bytes = Vec::with_capacity(QUERY_BYTES + 1);
            reader
                .take((QUERY_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
                .map_err(|error| format!("Cannot read worktree metadata output: {error}"))?;
            if bytes.len() > QUERY_BYTES {
                return Err("Worktree metadata output exceeds 64 KiB".to_string());
            }
            Ok(bytes)
        })
        .map_err(|error| format!("Cannot start worktree metadata output reader: {error}"))
}

fn join_reader(reader: Reader) -> Result<Vec<u8>, String> {
    reader
        .join()
        .map_err(|_| "Worktree metadata output reader failed".to_string())?
}

fn mediate(policy: &ExtensionPolicy, program: &str, args: &[String]) -> Result<(), String> {
    match evaluate_exec_mediation(&policy.exec_mediation, program, args) {
        ExecMediationResult::Deny { reason, .. } => Err(format!(
            "Worktree metadata denied by exec mediation: {reason}"
        )),
        ExecMediationResult::AllowWithAudit { class, reason } => {
            tracing::info!(
                event = "pijs.worktree_metadata.mediation_audit",
                class = class.label(),
                reason = %reason,
                "Host worktree metadata query admitted with audit"
            );
            Ok(())
        }
        ExecMediationResult::Allow => Ok(()),
    }
}

fn configure_environment(command: &mut Command) {
    command.env_clear().envs([
        ("LANG", "C"),
        ("LC_ALL", "C"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
        (
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        ),
        ("GIT_OPTIONAL_LOCKS", "0"),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_NO_REPLACE_OBJECTS", "1"),
        ("GIT_LITERAL_PATHSPECS", "1"),
    ]);
    #[cfg(windows)]
    if let Some(root) = std::env::var_os("SystemRoot") {
        // This OS binding is taken from the host, never the extension snapshot.
        command.env("SystemRoot", &root).env("WINDIR", root);
    }
}

#[cfg(unix)]
fn trusted_git_executable() -> Option<PathBuf> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    [
        "/usr/bin/git",
        "/usr/local/bin/git",
        "/opt/homebrew/bin/git",
    ]
    .into_iter()
    .find_map(|candidate| {
        let canonical = std::fs::canonicalize(candidate).ok()?;
        let metadata = std::fs::symlink_metadata(&canonical).ok()?;
        if !metadata.is_file()
            || metadata.uid() != 0
            || metadata.permissions().mode() & 0o111 == 0
            || metadata.permissions().mode() & 0o022 != 0
        {
            return None;
        }
        let trusted_ancestors = canonical.ancestors().skip(1).all(|ancestor| {
            std::fs::symlink_metadata(ancestor).is_ok_and(|metadata| {
                metadata.is_dir()
                    && !metadata.file_type().is_symlink()
                    && metadata.uid() == 0
                    && metadata.permissions().mode() & 0o022 == 0
            })
        });
        trusted_ancestors.then_some(canonical)
    })
}

#[cfg(windows)]
fn trusted_git_executable() -> Option<PathBuf> {
    use std::os::windows::fs::MetadataExt as _;

    [
        r"C:\Program Files\Git\cmd\git.exe",
        r"C:\Program Files\Git\bin\git.exe",
    ]
    .into_iter()
    .find_map(|candidate| {
        let path = Path::new(candidate);
        if !path.ancestors().all(|ancestor| {
            std::fs::symlink_metadata(ancestor)
                .is_ok_and(|metadata| metadata.file_attributes() & 0x400 == 0)
        }) {
            return None;
        }
        let canonical = std::fs::canonicalize(path).ok()?;
        let metadata = std::fs::symlink_metadata(&canonical).ok()?;
        (metadata.is_file() && metadata.file_attributes() & 0x400 == 0).then_some(canonical)
    })
}

#[cfg(not(any(unix, windows)))]
fn trusted_git_executable() -> Option<PathBuf> {
    None
}

fn current_root(output: &[u8]) -> Result<&str, String> {
    let output = std::str::from_utf8(output).map_err(|_| "Git worktree root is not UTF-8")?;
    let path = output.strip_suffix('\n').unwrap_or(output);
    let path = if cfg!(windows) {
        path.strip_suffix('\r').unwrap_or(path)
    } else {
        path
    };
    validate_root(path)
}

fn main_root(output: &[u8]) -> Result<&str, String> {
    let first = output
        .split(|byte| *byte == 0)
        .next()
        .filter(|_| output.last() == Some(&0))
        .ok_or("Git worktree inventory is not NUL-terminated")?;
    let first = std::str::from_utf8(first).map_err(|_| "Git main worktree root is not UTF-8")?;
    let path = first
        .strip_prefix("worktree ")
        .ok_or("Git worktree inventory has no initial worktree record")?;
    validate_root(path)
}

fn validate_root(path: &str) -> Result<&str, String> {
    if path.is_empty()
        || path.trim() != path
        || path.contains(['\0', '\r', '\n'])
        || !Path::new(path).is_absolute()
    {
        return Err("Git returned an ambiguous worktree path".to_string());
    }
    Ok(path)
}

fn canonical_hash_path(path: &str) -> Result<String, String> {
    let canonical = Path::new(path)
        .canonicalize()
        .map(strip_unc_prefix)
        .map_err(|error| format!("Cannot canonicalize Git worktree identity: {error}"))?;
    if !canonical.is_dir() {
        return Err("Git worktree identity is not a directory".to_string());
    }
    let canonical = canonical
        .to_str()
        .ok_or("Canonical Git worktree identity is not UTF-8")?;
    Ok(normalize_hash_path(
        canonical,
        cfg!(windows) || cfg!(target_os = "macos"),
    ))
}

fn normalize_hash_path(path: &str, fold_case: bool) -> String {
    let normalized = path.replace('\\', "/");
    let path = if !normalized.is_empty() && normalized.bytes().all(|byte| byte == b'/') {
        "/".to_string()
    } else if normalized.as_bytes().get(1) == Some(&b':')
        && normalized.as_bytes()[0].is_ascii_alphabetic()
        && normalized[2..].bytes().all(|byte| byte == b'/')
        && normalized.len() > 2
    {
        format!("{}/", &normalized[..2])
    } else {
        normalized.trim_end_matches('/').to_string()
    };
    if fold_case { path.to_lowercase() } else { path }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::{ExtensionOverride, ExtensionPolicyMode};

    fn allowed_policy() -> ExtensionPolicy {
        ExtensionPolicy {
            mode: ExtensionPolicyMode::Strict,
            default_caps: ["read", "env", "exec"].map(str::to_string).to_vec(),
            deny_caps: Vec::new(),
            ..ExtensionPolicy::default()
        }
    }

    fn config(cwd: &Path) -> PiJsRuntimeConfig {
        PiJsRuntimeConfig {
            cwd: cwd.to_string_lossy().into_owned(),
            disk_cache_dir: None,
            allow_unsafe_sync_exec: false,
            deny_env: false,
            ..PiJsRuntimeConfig::default()
        }
    }

    #[test]
    fn explicit_suffixes_and_windows_aliases_are_preserved_without_io() {
        for (key, value, platform) in [
            (SUFFIX_KEY, "", "linux"),
            (SUFFIX_KEY, "operator-selected", "linux"),
            ("context_mode_session_suffix", "", "win32"),
            ("Context_Mode_Session_Suffix", "selected", "win32"),
        ] {
            let mut config = config(Path::new("missing-worktree-must-not-be-probed"));
            config.env.insert(key.to_string(), value.to_string());
            config
                .env
                .insert("PI_PLATFORM".to_string(), platform.to_string());
            let before = config.env.clone();
            prepare(
                &mut config,
                &allowed_policy(),
                "context-mode",
                Instant::now(),
            )
            .expect("explicit override bypasses metadata query");
            assert_eq!(config.env, before);
        }
    }

    #[test]
    fn metadata_admission_respects_owner_caps_env_denial_and_secret_broker() {
        for capability in ["read", "env", "exec"] {
            let mut policy = allowed_policy();
            policy.per_extension.insert(
                "denied-owner".to_string(),
                ExtensionOverride {
                    deny: vec![capability.to_string()],
                    ..ExtensionOverride::default()
                },
            );
            let mut config = config(Path::new("missing-worktree-must-not-be-probed"));
            prepare(&mut config, &policy, "denied-owner", Instant::now())
                .expect("owner capability denial is a no-I/O skip");
            assert!(!config.env.contains_key(SUFFIX_KEY));
            policy.deny_caps.push(capability.to_string());
            policy
                .per_extension
                .get_mut("denied-owner")
                .unwrap()
                .deny
                .clear();
            policy.per_extension.get_mut("denied-owner").unwrap().allow =
                vec![capability.to_string()];
            prepare(&mut config, &policy, "denied-owner", Instant::now())
                .expect("global denial defeats owner allow");
            assert!(!config.env.contains_key(SUFFIX_KEY));
        }
        let mut denied_env = config(Path::new("missing-worktree-must-not-be-probed"));
        denied_env.deny_env = true;
        prepare(&mut denied_env, &allowed_policy(), "owner", Instant::now()).unwrap();
        let mut policy = allowed_policy();
        policy
            .secret_broker
            .secret_exact
            .push(SUFFIX_KEY.to_string());
        let mut broker_hidden = config(Path::new("missing-worktree-must-not-be-probed"));
        prepare(&mut broker_hidden, &policy, "owner", Instant::now()).unwrap();
        assert!(!broker_hidden.env.contains_key(SUFFIX_KEY));
        policy = allowed_policy();
        policy.mode = ExtensionPolicyMode::Prompt;
        policy
            .default_caps
            .retain(|capability| capability != "exec");
        prepare(&mut broker_hidden, &policy, "owner", Instant::now())
            .expect("Prompt is not implicit permission");
    }

    #[test]
    fn root_deadline_and_restricted_host_authority_stop_before_filesystem_queries() {
        let mut config = config(Path::new("missing-worktree-must-not-be-probed"));
        let error = prepare(&mut config, &allowed_policy(), "owner", Instant::now())
            .expect_err("expired root deadline");
        assert!(error.contains("deadline"), "{error}");
        assert!(!config.env.contains_key(SUFFIX_KEY));

        let owner = asupersync::Cx::for_request();
        let _guard = owner
            .restrict::<asupersync::cx::cap::None>()
            .set_current_restricted();
        let error = prepare(
            &mut config,
            &allowed_policy(),
            "owner",
            Instant::now() + Duration::from_secs(5),
        )
        .expect_err("restricted host context");
        assert!(error.contains("authority"), "{error}");
        assert!(!config.env.contains_key(SUFFIX_KEY));
    }

    #[test]
    fn bounded_readers_accept_the_limit_and_reject_the_first_extra_byte() {
        let exact = spawn_reader(std::io::Cursor::new(vec![b'x'; QUERY_BYTES])).unwrap();
        assert_eq!(join_reader(exact).unwrap().len(), QUERY_BYTES);
        let oversized = spawn_reader(std::io::Cursor::new(vec![b'x'; QUERY_BYTES + 1])).unwrap();
        assert!(join_reader(oversized).unwrap_err().contains("64 KiB"));
    }

    #[test]
    fn worktree_hash_path_normalization_matches_windows_and_posix_roots() {
        for (input, fold_case, expected) in [
            ("", false, ""),
            ("////", false, "/"),
            (r"C:\\", true, "c:/"),
            (r"C:\Work\Ünicode\", true, "c:/work/ünicode"),
            (r"\\SERVER\SHARE\", true, "//server/share"),
            ("/Work/Case///", false, "/Work/Case"),
            ("/Work/Case/", true, "/work/case"),
        ] {
            assert_eq!(normalize_hash_path(input, fold_case), expected, "{input}");
        }
        #[cfg(windows)]
        for (input, expected) in [
            (r"\\?\C:\Work\Tree", "c:/work/tree"),
            (r"\\?\UNC\SERVER\Share\Tree", "//server/share/tree"),
        ] {
            let stripped = strip_unc_prefix(PathBuf::from(input));
            assert_eq!(
                normalize_hash_path(stripped.to_str().unwrap(), true),
                expected
            );
        }
    }

    #[test]
    fn inventory_parser_requires_an_initial_nul_terminated_worktree_record() {
        let root = if cfg!(windows) {
            "C:/main tree"
        } else {
            "/main tree"
        };
        let inventory = format!("worktree {root}\0HEAD abc\0\0worktree ignored\0\0");
        assert_eq!(main_root(inventory.as_bytes()).unwrap(), root);
        assert!(main_root(b"HEAD abc\0worktree /main\0").is_err());
        assert!(main_root(b"worktree /main\nHEAD abc\n").is_err());
        assert!(main_root(b"worktree /misleading\nworktree /other\0").is_err());
        assert!(current_root(b"/ambiguous\nsecond\n").is_err());
        assert!(current_root(b"relative\n").is_err());
    }

    fn fixture_git(executable: &Path, cwd: &Path, args: &[&str]) {
        let mut command = Command::new(executable);
        command.arg("-C").arg(cwd).args(args).current_dir(cwd);
        configure_environment(&mut command);
        QueryBudget::new(Instant::now() + Duration::from_secs(10))
            .unwrap()
            .capture(command)
            .expect("create real Git fixture");
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn real_git_main_linked_and_nested_workspaces_get_stable_scoped_identities() {
        let Some(git) = trusted_git_executable() else {
            eprintln!("skipping real Git worktree fixture: trusted system Git unavailable");
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let main = temp.path().join("main tree");
        let linked = temp.path().join("linked tree");
        std::fs::create_dir_all(&main).unwrap();
        fixture_git(&git, &main, &["init", "--quiet"]);
        fixture_git(
            &git,
            &main,
            &[
                "-c",
                "user.name=Pi Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgSign=false",
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "fixture",
            ],
        );
        fixture_git(
            &git,
            &main,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                linked.to_str().unwrap(),
                "HEAD",
            ],
        );
        let nested = linked.join("nested");
        std::fs::create_dir(&nested).unwrap();
        let policy = allowed_policy();
        let deadline = || Instant::now() + Duration::from_secs(5);
        let mut main_config = config(&main);
        // Ambient/configured PATH and Git routing do not select a program or
        // redirect these fixed host queries away from the admitted workspace.
        main_config
            .env
            .insert("PATH".to_string(), linked.display().to_string());
        main_config
            .env
            .insert("GIT_DIR".to_string(), linked.display().to_string());
        prepare(&mut main_config, &policy, "context-mode", deadline()).unwrap();
        assert_eq!(
            main_config.env.get(SUFFIX_KEY).map(String::as_str),
            Some("")
        );
        assert!(!main_config.allow_unsafe_sync_exec);

        let expected_path = canonical_hash_path(linked.to_str().unwrap()).unwrap();
        let expected =
            super::super::hex_lower(&Sha256::digest(expected_path.as_bytes()))[..8].to_string();
        for cwd in [&linked, &nested, &linked] {
            let mut config = config(cwd);
            prepare(&mut config, &policy, "context-mode", deadline()).unwrap();
            assert_eq!(config.env.get(SUFFIX_KEY), Some(&expected));
            assert!(!config.allow_unsafe_sync_exec);
        }

        for denied in [
            "git worktree".to_string(),
            git.to_string_lossy().into_owned(),
        ] {
            let mut denied_policy = policy.clone();
            denied_policy.exec_mediation.deny_patterns = vec![denied];
            let mut config = config(&linked);
            let error =
                prepare(&mut config, &denied_policy, "context-mode", deadline()).unwrap_err();
            assert!(error.contains("mediation"), "{error}");
            assert!(!config.env.contains_key(SUFFIX_KEY));
        }
        let outside = temp.path().join("non-repository");
        std::fs::create_dir(&outside).unwrap();
        let mut not_git = config(&outside);
        assert!(prepare(&mut not_git, &policy, "context-mode", deadline()).is_err());
        assert!(
            !not_git.env.contains_key(SUFFIX_KEY),
            "unknown is not main-worktree identity"
        );
    }

    #[cfg(unix)]
    #[test]
    fn timed_out_or_oversized_capture_retires_the_child_and_pipe_readers() {
        for (script, limit, expected_error) in [
            ("/bin/sleep 10", Duration::from_millis(50), "deadline"),
            (
                "while :; do printf '0123456789abcdef'; done",
                QUERY_TIMEOUT,
                "64 KiB",
            ),
        ] {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", script]);
            configure_environment(&mut command);
            let start = Instant::now();
            let budget = QueryBudget {
                owner: AgentCx::for_current_or_request(),
                deadline: start + limit,
            };
            let error = budget.capture(command).unwrap_err();
            assert!(error.contains(expected_error), "{error}");
            assert!(
                start.elapsed() < Duration::from_secs(3),
                "child/reader cleanup blocked"
            );
        }
    }
}
