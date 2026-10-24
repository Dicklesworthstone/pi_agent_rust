//! Session-owned Bash state without replaying earlier commands.
//!
//! Each invocation runs in the existing process supervisor. A private, bounded
//! snapshot transfers native Bash declarations between invocations; the
//! transaction remains leased to the supervisor when a call is backgrounded.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use crate::error::{Error, Result};

const MAX_SNAPSHOT_BYTES: usize = 1024 * 1024;
const MAX_RETAINED_SESSIONS: usize = 32;
const SNAPSHOT_MAGIC: &str = "pi.bash_state.v1";
const SNAPSHOT_END: &str = "--pi-bash-state-complete--";

pub(super) const BUSY_NOTICE: &str = "The session shell is busy. This command ran in a temporary shell: session variables, functions, aliases, options, and working-directory changes were unavailable, and this command's shell-state changes will not persist.";
pub(super) const CAPACITY_NOTICE: &str = "The session shell state pool is full. This command ran in a temporary shell, and its shell-state changes will not persist.";

#[derive(Clone, PartialEq, Eq, Hash)]
struct SessionKey {
    session_id: String,
    shell: String,
    prefix: Option<String>,
    initial_cwd: PathBuf,
}

#[derive(Default)]
struct SessionState {
    snapshot: Option<Arc<Snapshot>>,
    busy: bool,
}

struct PoolEntry {
    state: Arc<Mutex<SessionState>>,
    last_used: Instant,
}

fn pool() -> &'static Mutex<HashMap<SessionKey, PoolEntry>> {
    static POOL: OnceLock<Mutex<HashMap<SessionKey, PoolEntry>>> = OnceLock::new();
    POOL.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Default)]
struct Snapshot {
    variables: BTreeMap<String, String>,
    functions: String,
    aliases: String,
    shell_options: Vec<String>,
    bash_options: Vec<String>,
    cwd: PathBuf,
    umask: String,
}

/// The transaction and its temporary files outlive a promoted tool future.
/// Only the process owner calls `finish`, after the process has been reaped.
#[derive(Clone)]
pub(crate) struct ShellTransaction(Arc<TransactionInner>);

struct TransactionInner {
    state: Arc<Mutex<SessionState>>,
    completion: Mutex<TransactionCompletion>,
    capture: Mutex<tempfile::NamedTempFile>,
    // Keep the source file alive through spawn, execution, and settlement.
    _restore: tempfile::NamedTempFile,
    command: String,
    cwd: PathBuf,
    proxy_variables: Vec<String>,
}

#[derive(Default)]
struct TransactionCompletion {
    requested: Option<bool>,
    active_processes: usize,
    finished: bool,
}

/// A direct runner may hand its child to an asynchronous reaper on Drop or
/// cancellation. Retain both the lease and private files until that owner has
/// actually reaped the child, even if the outer tool already requested finish.
pub(crate) struct ShellProcessLease(ShellTransaction);

pub(super) enum Acquisition {
    Persistent(ShellTransaction),
    Temporary(&'static str),
}

/// Acquire before spawn. Busy calls do not wait on a long-running command and
/// never import or publish that command's in-flight session state.
pub(super) fn acquire(
    session_id: &str,
    initial_cwd: &Path,
    shell: &str,
    prefix: Option<&str>,
    command: &str,
) -> Result<Acquisition> {
    let key = SessionKey {
        session_id: session_id.to_owned(),
        shell: shell.to_owned(),
        prefix: prefix.map(str::to_owned),
        initial_cwd: initial_cwd.to_path_buf(),
    };
    let (state, snapshot) = {
        let mut sessions = pool()
            .lock()
            .map_err(|_| Error::tool("bash", "session shell pool poisoned"))?;
        if !sessions.contains_key(&key) && sessions.len() >= MAX_RETAINED_SESSIONS {
            let oldest_idle = sessions
                .iter()
                .filter(|(_, entry)| entry.state.try_lock().is_ok_and(|state| !state.busy))
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone());
            if let Some(oldest_idle) = oldest_idle {
                sessions.remove(&oldest_idle);
            } else {
                return Ok(Acquisition::Temporary(CAPACITY_NOTICE));
            }
        }
        let entry = sessions.entry(key).or_insert_with(|| PoolEntry {
            state: Arc::new(Mutex::new(SessionState::default())),
            last_used: Instant::now(),
        });
        entry.last_used = Instant::now();
        let state = Arc::clone(&entry.state);
        let mut leased = state
            .lock()
            .map_err(|_| Error::tool("bash", "session shell state poisoned"))?;
        if leased.busy {
            return Ok(Acquisition::Temporary(BUSY_NOTICE));
        }
        // Publish the lease while holding the pool lock too, so eviction
        // cannot remove an apparently idle entry during this handoff.
        leased.busy = true;
        let snapshot = leased.snapshot.clone();
        drop(leased);
        (state, snapshot)
    };
    // Fallible file setup must release the lease too. No process can have
    // executed at this point, so the old snapshot remains authoritative.
    match prepare_transaction(
        Arc::clone(&state),
        snapshot.as_deref(),
        prefix,
        command,
        initial_cwd,
    ) {
        Ok(transaction) => Ok(Acquisition::Persistent(transaction)),
        Err(error) => {
            state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .busy = false;
            Err(error)
        }
    }
}

fn prepare_transaction(
    state: Arc<Mutex<SessionState>>,
    snapshot: Option<&Snapshot>,
    prefix: Option<&str>,
    command: &str,
    initial_cwd: &Path,
) -> Result<ShellTransaction> {
    // NamedTempFile creates a private file, including mode 0600 on Unix. State
    // never enters the public tool-output artifact store or job descriptors.
    let mut restore = tempfile::Builder::new()
        .prefix("pi-shell-restore-")
        .tempfile()
        .map_err(|error| {
            Error::tool(
                "bash",
                format!("Cannot prepare private shell state: {error}"),
            )
        })?;
    let capture = tempfile::Builder::new()
        .prefix("pi-shell-capture-")
        .tempfile()
        .map_err(|error| {
            Error::tool(
                "bash",
                format!("Cannot prepare private shell state: {error}"),
            )
        })?;
    let proxy_env = crate::http::proxy::child_process_env();
    let proxy_variables: Vec<String> = proxy_env.iter().map(|(key, _)| key.clone()).collect();
    // A removed working directory must not make every future `cd` fail before
    // the command can run. Keep variables and fall back to the tool's root.
    let cwd = snapshot
        .filter(|snapshot| snapshot.cwd.is_dir())
        .map_or_else(
            || initial_cwd.to_path_buf(),
            |snapshot| snapshot.cwd.clone(),
        );
    let restore_source = snapshot.map_or_else(String::new, |snapshot| {
        snapshot.restore_source(&proxy_variables, &cwd)
    });
    restore
        .write_all(restore_source.as_bytes())
        .map_err(|error| {
            Error::tool("bash", format!("Cannot write private shell state: {error}"))
        })?;
    let user_command = prefix
        .filter(|prefix| !prefix.trim().is_empty())
        .map_or_else(
            || command.to_owned(),
            |prefix| format!("{prefix}\n{command}"),
        );
    let command = wrapper(
        &restore.path().to_string_lossy(),
        &capture.path().to_string_lossy(),
        &user_command,
    );
    Ok(ShellTransaction(Arc::new(TransactionInner {
        state,
        completion: Mutex::new(TransactionCompletion::default()),
        capture: Mutex::new(capture),
        _restore: restore,
        command,
        cwd,
        proxy_variables,
    })))
}

impl ShellTransaction {
    pub(crate) fn command(&self) -> &str {
        &self.0.command
    }

    pub(crate) fn cwd(&self) -> &Path {
        &self.0.cwd
    }

    pub(crate) fn retain_process(&self) -> ShellProcessLease {
        self.0
            .completion
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active_processes += 1;
        ShellProcessLease(self.clone())
    }

    /// Publish before the supervisor publishes terminal job state. A failed
    /// spawn, cancellation, timeout, incomplete capture, or oversized capture
    /// leaves the last committed snapshot untouched.
    pub(crate) fn finish(&self, commit: bool) {
        let mut completion = self
            .0
            .completion
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if completion.finished {
            return;
        }
        let commit = completion.requested.unwrap_or(true) && commit;
        completion.requested = Some(commit);
        if completion.active_processes == 0 {
            self.0.publish(commit);
            completion.finished = true;
        }
    }
}

impl Drop for ShellProcessLease {
    fn drop(&mut self) {
        let mut completion = self
            .0
            .0
            .completion
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        completion.active_processes -= 1;
        if completion.active_processes == 0
            && !completion.finished
            && let Some(commit) = completion.requested
        {
            self.0.0.publish(commit);
            completion.finished = true;
        }
    }
}

impl TransactionInner {
    fn publish(&self, commit: bool) {
        let snapshot = if commit {
            self.read_snapshot().map(Arc::new)
        } else {
            None
        };
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(snapshot) = snapshot {
            state.snapshot = Some(snapshot);
        }
        state.busy = false;
    }

    fn read_snapshot(&self) -> Option<Snapshot> {
        let mut capture = self
            .capture
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Read the already-open private inode, not an attacker-replaced path.
        capture.as_file_mut().seek(SeekFrom::Start(0)).ok()?;
        let mut bytes = Vec::new();
        capture
            .as_file_mut()
            .take(u64::try_from(MAX_SNAPSHOT_BYTES + 1).ok()?)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() > MAX_SNAPSHOT_BYTES {
            tracing::warn!(
                event = "pi.bash.state",
                "shell state exceeded its 1 MiB bound; previous state retained"
            );
            return None;
        }
        let text = String::from_utf8(bytes).ok()?;
        Snapshot::parse(&text, &self.proxy_variables)
    }
}

impl Drop for TransactionInner {
    fn drop(&mut self) {
        if !self
            .completion
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .finished
        {
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .busy = false;
        }
    }
}

fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// Parse the whole block before evaluating user code so newly defined aliases
/// cannot rewrite the snapshot commands. Native declarations handle literal
/// quotes, newlines, sparse arrays, and associative-array keys without an
/// ad hoc shell-value encoder. Capture runs in a subshell so turning off
/// tracing/traps and limiting its file size cannot alter the user's state.
fn wrapper(restore: &str, capture: &str, command: &str) -> String {
    format!(
        r#"{{
\builtin set +e +u +x +v
\builtin shopt -u expand_aliases
\builtin unalias -a
\builtin source -- {restore} || \builtin exit 125
\builtin eval -- {command}
__PI_SHELL_STATUS=$?
__PI_SHELL_OPTIONS=$SHELLOPTS
(
    \builtin trap - DEBUG RETURN ERR EXIT
    \builtin set +e +u +x +v
    \builtin ulimit -c 0
    \builtin ulimit -f 2048
    {{
        __PI_SHELL_BASH_OPTIONS=
        while IFS= \builtin read -r __PI_SHELL_OPTION; do
            if \builtin shopt -q "$__PI_SHELL_OPTION"; then
                __PI_SHELL_BASH_OPTIONS="$__PI_SHELL_BASH_OPTIONS$__PI_SHELL_OPTION:"
            fi
        done < <(\builtin compgen -A shopt)
        \builtin printf '%s\0' '{magic}' "$__PI_SHELL_OPTIONS" "$__PI_SHELL_BASH_OPTIONS"
        \builtin pwd -L
        \builtin printf '\0'
        \builtin umask
        \builtin printf '\0'
        \builtin declare -pf
        \builtin printf '\0'
        \builtin alias -p
        \builtin printf '\0'
        while IFS= \builtin read -r __PI_SHELL_NAME; do
            case "$__PI_SHELL_NAME" in
                __PI_SHELL_*|BASH*|SHELLOPTS|EUID|UID|PPID|PWD|OLDPWD|SHLVL|_|RANDOM|SRANDOM|SECONDS|EPOCHREALTIME|EPOCHSECONDS|LINENO|FUNCNAME|GROUPS|DIRSTACK|PIPESTATUS|HISTCMD) continue ;;
            esac
            \builtin printf '%s\0' "$__PI_SHELL_NAME"
            \builtin declare -p "$__PI_SHELL_NAME"
            \builtin printf '\0'
        done < <(\builtin compgen -v)
        \builtin printf '%s\0' '{end}'
    }} >| {capture}
) 2>/dev/null || \builtin :
\builtin exit "$__PI_SHELL_STATUS"
}}"#,
        restore = quote(restore),
        capture = quote(capture),
        command = quote(command),
        magic = SNAPSHOT_MAGIC,
        end = SNAPSHOT_END,
    )
}

fn ordinary_variable(name: &str) -> bool {
    let mut chars = name.bytes();
    chars
        .next()
        .is_some_and(|first| first == b'_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == b'_' || ch.is_ascii_alphanumeric())
        && !name.starts_with("__PI_SHELL_")
        && !name.starts_with("BASH")
        && !matches!(
            name,
            "SHELLOPTS"
                | "EUID"
                | "UID"
                | "PPID"
                | "PWD"
                | "OLDPWD"
                | "SHLVL"
                | "_"
                | "RANDOM"
                | "SRANDOM"
                | "SECONDS"
                | "EPOCHREALTIME"
                | "EPOCHSECONDS"
                | "LINENO"
                | "FUNCNAME"
                | "GROUPS"
                | "DIRSTACK"
                | "PIPESTATUS"
                | "HISTCMD"
        )
}

fn options(text: &str) -> Option<Vec<String>> {
    text.split(':')
        .filter(|option| !option.is_empty())
        .map(|option| {
            option
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
                .then(|| option.to_owned())
        })
        .collect()
}

impl Snapshot {
    fn parse(text: &str, proxy_variables: &[String]) -> Option<Self> {
        let mut fields = text.split('\0');
        if fields.next()? != SNAPSHOT_MAGIC {
            return None;
        }
        let shell_options = options(fields.next()?)?;
        let bash_options = options(fields.next()?)?;
        // pwd and umask add exactly one newline. Preserve newlines belonging
        // to the directory itself instead of trimming arbitrary whitespace.
        let cwd = PathBuf::from(fields.next()?.strip_suffix('\n')?);
        if !cwd.is_absolute() {
            return None;
        }
        let umask = fields.next()?.strip_suffix('\n')?.to_owned();
        if umask.is_empty() || !umask.bytes().all(|byte| (b'0'..=b'7').contains(&byte)) {
            return None;
        }
        let functions = fields.next()?.to_owned();
        let aliases = fields.next()?.to_owned();
        let mut variables = BTreeMap::new();
        loop {
            let name = fields.next()?;
            if name == SNAPSHOT_END {
                if fields.next() != Some("") || fields.next().is_some() {
                    return None;
                }
                break;
            }
            let declaration = fields.next()?;
            if !ordinary_variable(name) || !declaration.starts_with("declare -") {
                return None;
            }
            if !proxy_variables.iter().any(|key| key == name)
                && variables
                    .insert(name.to_owned(), declaration.to_owned())
                    .is_some()
            {
                return None;
            }
        }
        Some(Self {
            variables,
            functions,
            aliases,
            shell_options,
            bash_options,
            cwd,
            umask,
        })
    }

    fn restore_source(&self, proxy_variables: &[String], cwd: &Path) -> String {
        let mut source = String::new();
        // Restore parse-affecting options before function definitions; extglob
        // patterns are otherwise syntax errors even inside an uncalled body.
        // Alias expansion remains off until all state has been restored.
        source.push_str("while IFS= \\builtin read -r __PI_SHELL_OPTION; do \\builtin shopt -u \"$__PI_SHELL_OPTION\" 2>/dev/null; done < <(\\builtin compgen -A shopt)\n");
        for option in &self.bash_options {
            if option != "expand_aliases" {
                let _ = writeln!(source, "\\builtin shopt -s {option} 2>/dev/null");
            }
        }
        source.push_str("while IFS= \\builtin read -r __PI_SHELL_FUNCTION; do \\builtin unset -f -- \"$__PI_SHELL_FUNCTION\"; done < <(\\builtin compgen -A function)\n");
        // Fresh processes inherit the host environment. Honor variables the
        // previous call unset, while configured proxy routing stays per-call.
        for (name, _) in std::env::vars_os() {
            let Some(name) = name.to_str().filter(|name| ordinary_variable(name)) else {
                continue;
            };
            if !self.variables.contains_key(name) && !proxy_variables.iter().any(|key| key == name)
            {
                let _ = writeln!(source, "\\builtin unset -v -- {}", quote(name));
            }
        }
        for (name, declaration) in &self.variables {
            if !proxy_variables.iter().any(|key| key == name) {
                // Bare declare preserves an inherited export attribute. Clear
                // it first so `export -n` remains effective on the next call.
                let _ = writeln!(source, "\\builtin unset -v -- {}", quote(name));
                // Bash recognizes native array assignments only when declare
                // is the command word, without a preceding builtin command.
                // Aliases are disabled and inherited functions were removed.
                source.push_str(declaration);
            }
        }
        // Parse aliases as one block before enabling expansion. Splitting at
        // newlines would corrupt valid multi-line alias values.
        if !self.aliases.is_empty() {
            source.push_str("{\n");
            source.push_str(&self.aliases);
            source.push_str("}\n");
        }
        source.push_str(&self.functions);
        let _ = writeln!(
            source,
            "\\builtin cd -- {} || \\builtin exit 125",
            quote(&cwd.to_string_lossy())
        );
        let _ = writeln!(source, "\\builtin umask {}", self.umask);
        source.push_str("while IFS= \\builtin read -r __PI_SHELL_OPTION; do \\builtin set +o \"$__PI_SHELL_OPTION\"; done < <(\\builtin compgen -A setopt)\n");
        if self
            .bash_options
            .iter()
            .any(|option| option == "expand_aliases")
        {
            source.push_str("\\builtin shopt -s expand_aliases\n");
        }
        // xtrace/verbose are restored only after private declarations were
        // read, so neither option can print the saved secret-bearing values.
        for option in &self.shell_options {
            let _ = writeln!(source, "\\builtin set -o {option}");
        }
        source
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn snapshot_framing_preserves_native_quoting_and_requires_the_complete_record() {
        let declaration = "declare -- PI_VALUE=$'line one\\nquoted\\047 value'\n";
        let complete = format!(
            "{SNAPSHOT_MAGIC}\0braceexpand:hashall:interactive-comments\0extglob\0/tmp/line\nend\n\00077\n\0\0\0PI_VALUE\0{declaration}\0{SNAPSHOT_END}\0"
        );
        let snapshot = Snapshot::parse(&complete, &[]).expect("complete native snapshot");
        assert_eq!(snapshot.cwd, Path::new("/tmp/line\nend"));
        assert_eq!(snapshot.umask, "0077");
        assert_eq!(snapshot.variables["PI_VALUE"], declaration);
        assert!(
            snapshot
                .shell_options
                .iter()
                .any(|option| option == "interactive-comments")
        );
        for end in [0, complete.len() / 2, complete.len() - 1] {
            assert!(Snapshot::parse(&complete[..end], &[]).is_none());
        }
        assert!(Snapshot::parse(&format!("{complete}unexpected"), &[]).is_none());
    }

    #[test]
    fn shell_state_files_are_private_and_oversized_capture_preserves_prior_state() {
        let root = tempfile::TempDir::new().expect("temporary workspace");
        let original = Arc::new(Snapshot {
            cwd: root.path().to_path_buf(),
            umask: "0077".to_owned(),
            variables: BTreeMap::from([(
                "PI_SECRET".to_owned(),
                "declare -- PI_SECRET=\"private-value\"\n".to_owned(),
            )]),
            ..Snapshot::default()
        });
        let state = Arc::new(Mutex::new(SessionState {
            snapshot: Some(Arc::clone(&original)),
            busy: true,
        }));
        let transaction = prepare_transaction(
            Arc::clone(&state),
            Some(&original),
            None,
            "printf done",
            root.path(),
        )
        .expect("prepare private state");
        assert_eq!(
            transaction
                .0
                ._restore
                .as_file()
                .metadata()
                .expect("restore metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        {
            let mut capture = transaction.0.capture.lock().expect("capture");
            assert_eq!(
                capture
                    .as_file()
                    .metadata()
                    .expect("capture metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            capture
                .write_all(&vec![b'x'; MAX_SNAPSHOT_BYTES + 1])
                .expect("oversized producer");
        }
        transaction.finish(true);
        let retained = state.lock().expect("state");
        assert!(!retained.busy);
        assert!(Arc::ptr_eq(
            retained.snapshot.as_ref().expect("prior snapshot"),
            &original
        ));
        assert!(
            !transaction.command().contains("private-value"),
            "state values must not enter process arguments"
        );
    }
}
