//! GitHub tool (bd-cv653.2.3): structured `gh`-backed PR/issue/search/Actions
//! operations, replacing ad-hoc `bash` shell-outs with typed responses,
//! short-TTL caching, and a single named error taxonomy.
//!
//! Backend: the `gh` CLI (path from `config.ghPath`, default `gh`), always via
//! `--json`/`gh api` style typed output — never HTML scraping. Operations:
//! `pr_view`, `issue_view`, `pr_diff`, `search` (code/issues/prs),
//! `run_list`, `run_watch` (poll until a terminal conclusion, streaming
//! status through `on_update` like `bash` does).

use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::memory::screen_secrets;
use crate::model::{ContentBlock, TextContent};
use crate::tools::{
    ProcessCleanupMode, ProcessGuard, Tool, ToolEffects, ToolOutput, ToolUpdate,
    attach_child_job_discipline, command_with_default_sigpipe_in_dir,
    isolate_command_process_group, kill_process_group_tree, read_to_end_capped_and_drain,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Read-cache TTL: repeated PR/issue reads within a turn are common and gh is
/// slow (network); short enough that watch/refresh flows stay honest.
const CACHE_TTL: Duration = Duration::from_secs(30);
/// Default total budget for `run_watch`.
const DEFAULT_WATCH_TIMEOUT_SECS: u64 = 900;
/// Per-invocation budget for a single `gh` call.
const GH_CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// Implicit `git remote` lookup should never become a long-running tool call.
const GIT_REMOTE_TIMEOUT: Duration = Duration::from_secs(5);
/// Subprocess output is drained to EOF, but only these prefixes remain in RAM.
const MAX_GH_STDOUT_BYTES: u64 = 1_000_000;
const MAX_GH_STDERR_BYTES: u64 = 64 * 1024;
const MAX_GIT_REMOTE_BYTES: u64 = 16 * 1024;
/// Keep the short-lived cache itself proportional to a bounded working set.
const MAX_CACHE_ENTRIES: usize = 32;
const MAX_CACHE_KEY_BYTES: usize = 64 * 1024;
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(25);
/// Diff output bounds (standard truncation contract).
const MAX_DIFF_LINES: usize = 2000;
const MAX_DIFF_BYTES: usize = 1_000_000;

pub struct GithubTool {
    cwd: PathBuf,
    gh_path: String,
    cache: Mutex<HashMap<Vec<String>, (Instant, GhOutput)>>,
}

#[derive(Clone, Debug)]
struct GhOutput {
    text: String,
    truncated: bool,
}

#[derive(Debug)]
enum ProcessTermination {
    Exited(ExitStatus),
    TimedOut,
    Cancelled,
}

#[derive(Debug)]
struct BoundedProcessOutput {
    termination: ProcessTermination,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_truncated: bool,
    stderr_truncated: bool,
}

/// A process guard with the stricter GithubTool terminal-ordering contract.
///
/// The shared [`ProcessGuard`] performs best-effort cleanup in a background
/// thread on drop. A tool future can be dropped immediately before the agent
/// records its cancellation result, so this wrapper synchronously kills the
/// isolated tree and reaps the root before its drop returns.
struct GithubProcessGuard {
    process: ProcessGuard,
    pid: u32,
    active: bool,
}

impl GithubProcessGuard {
    const fn new(process: ProcessGuard, pid: u32) -> Self {
        Self {
            process,
            pid,
            active: true,
        }
    }

    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.process.try_wait_child()
    }

    fn finish_exited(&mut self) -> std::io::Result<ExitStatus> {
        let status = self.process.wait();
        // A subprocess can exit after leaving a descendant holding one of its
        // pipe descriptors. Always close the isolated group before joining the
        // pipe pumps so successful completion is bounded too.
        kill_process_group_tree(Some(self.pid));
        self.active = false;
        status
    }

    fn kill_and_reap(&mut self) -> std::io::Result<ExitStatus> {
        kill_process_group_tree(Some(self.pid));
        let status = self.process.wait();
        self.active = false;
        status
    }
}

impl Drop for GithubProcessGuard {
    fn drop(&mut self) {
        if self.active {
            kill_process_group_tree(Some(self.pid));
            let _ = self.process.wait();
        }
    }
}

async fn run_bounded_process(
    program: &OsStr,
    args: &[&str],
    cwd: &Path,
    timeout: Duration,
    stdout_limit: u64,
) -> std::io::Result<BoundedProcessOutput> {
    let cx = AgentCx::for_current_or_request();
    let started = Instant::now();
    if cx.checkpoint().is_err() {
        return Ok(BoundedProcessOutput {
            termination: ProcessTermination::Cancelled,
            stdout: Vec::new(),
            stderr: Vec::new(),
            stdout_truncated: false,
            stderr_truncated: false,
        });
    }
    let mut command = command_with_default_sigpipe_in_dir(program, cwd)?;
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    isolate_command_process_group(&mut command);

    let mut child = command.spawn()?;
    attach_child_job_discipline(&child);
    let pid = child.id();
    let stdout = child.stdout.take().ok_or_else(|| {
        kill_process_group_tree(Some(pid));
        let _ = child.wait();
        std::io::Error::other("missing stdout pipe")
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        kill_process_group_tree(Some(pid));
        let _ = child.wait();
        std::io::Error::other("missing stderr pipe")
    })?;
    let process = ProcessGuard::new(child, ProcessCleanupMode::ProcessGroupTree);
    let mut guard = GithubProcessGuard::new(process, pid);

    let stdout_thread = spawn_pipe_capture("github-stdout", stdout, stdout_limit)?;
    let stderr_thread = spawn_pipe_capture("github-stderr", stderr, MAX_GH_STDERR_BYTES)?;

    let termination = loop {
        if guard.try_wait()?.is_some() {
            break ProcessTermination::Exited(guard.finish_exited()?);
        }
        if started.elapsed() >= timeout {
            guard.kill_and_reap()?;
            break ProcessTermination::TimedOut;
        }
        if cx.checkpoint().is_err() {
            guard.kill_and_reap()?;
            break ProcessTermination::Cancelled;
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        asupersync::time::sleep(
            asupersync::time::wall_now(),
            PROCESS_POLL_INTERVAL.min(remaining),
        )
        .await;
    };

    let (stdout, stdout_truncated) = join_pipe_capture(stdout_thread, stdout_limit, "stdout")?;
    let (stderr, stderr_truncated) =
        join_pipe_capture(stderr_thread, MAX_GH_STDERR_BYTES, "stderr")?;
    Ok(BoundedProcessOutput {
        termination,
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
    })
}

fn spawn_pipe_capture<R>(
    name: &str,
    reader: R,
    limit: u64,
) -> std::io::Result<JoinHandle<std::result::Result<Vec<u8>, String>>>
where
    R: std::io::Read + Send + 'static,
{
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || read_to_end_capped_and_drain(reader, limit))
}

fn join_pipe_capture(
    thread: JoinHandle<std::result::Result<Vec<u8>, String>>,
    limit: u64,
    stream: &str,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut bytes = thread
        .join()
        .map_err(|_| std::io::Error::other(format!("{stream} capture thread panicked")))?
        .map_err(|err| std::io::Error::other(format!("{stream} capture failed: {err}")))?;
    let truncated = u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit;
    if truncated {
        bytes.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    }
    Ok((bytes, truncated))
}

fn stderr_evidence(output: &BoundedProcessOutput) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let brief = sanitize_process_diagnostic(&stderr.lines().take(4).collect::<Vec<_>>().join("\n"));
    let truncation = if output.stderr_truncated {
        format!("\n[stderr truncated at {MAX_GH_STDERR_BYTES} bytes]")
    } else {
        String::default()
    };
    if brief.is_empty() && truncation.is_empty() {
        String::new()
    } else {
        format!("\nstderr:\n{brief}{truncation}")
    }
}

/// Keep subprocess diagnostics useful without letting credentials or terminal
/// control bytes escape into a transcript. Bidi controls are escaped too: they
/// are not classified as `char::is_control`, but can visually reorder errors.
fn sanitize_process_diagnostic(raw: &str) -> String {
    let screened = screen_secrets(raw);
    let mut safe = String::with_capacity(screened.len());
    for character in screened.chars() {
        if matches!(character, '\n' | '\t') {
            safe.push(character);
        } else if character.is_control()
            || matches!(
                character,
                '\u{061c}'
                    | '\u{200e}'
                    | '\u{200f}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2066}'..='\u{2069}'
            )
        {
            safe.extend(character.escape_unicode());
        } else {
            safe.push(character);
        }
    }
    safe
}

impl GithubTool {
    pub fn new(cwd: &Path, gh_path: Option<&str>) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            gh_path: gh_path.unwrap_or("gh").to_string(),
            cache: Mutex::new(HashMap::new()),
        }
    }

    fn cache_get(&self, key: &[String]) -> Option<GhOutput> {
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.retain(|_, (at, _)| at.elapsed() < CACHE_TTL);
        cache.get(key).map(|(_, body)| body.clone())
    }

    fn cache_put(&self, key: Vec<String>, body: GhOutput) {
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.retain(|_, (at, _)| at.elapsed() < CACHE_TTL);
        if cache.len() >= MAX_CACHE_ENTRIES
            && !cache.contains_key(&key)
            && let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(key, _)| key.clone())
        {
            cache.remove(&oldest);
        }
        cache.insert(key, (Instant::now(), body));
    }

    fn cache_clear(&self) {
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    /// Run `gh` with a bounded wait; classifies missing-binary and
    /// unauthenticated states into the named error taxonomy.
    async fn run_gh_with_timeout(
        &self,
        args: &[&str],
        timeout: Duration,
        allow_truncated_stdout: bool,
    ) -> Result<GhOutput> {
        let output = run_bounded_process(
            OsStr::new(&self.gh_path),
            args,
            &self.cwd,
            timeout,
            MAX_GH_STDOUT_BYTES,
        )
        .await
        .map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                Error::tool(
                    "github",
                    "GH_MISSING: configured `gh` executable not found. Install the GitHub CLI \
                     (https://cli.github.com) or correct ghPath in settings.",
                )
            } else {
                Error::tool(
                    "github",
                    format!(
                        "GH_PROCESS: {}",
                        sanitize_process_diagnostic(&err.to_string())
                    ),
                )
            }
        })?;
        let evidence = stderr_evidence(&output);
        match &output.termination {
            ProcessTermination::TimedOut => {
                return Err(Error::tool(
                    "github",
                    format!(
                        "GH_TIMEOUT: `gh` invocation exceeded {}ms{evidence}",
                        timeout.as_millis(),
                    ),
                ));
            }
            ProcessTermination::Cancelled => {
                return Err(Error::tool(
                    "github",
                    format!("GH_CANCELLED: `gh` invocation was cancelled{evidence}"),
                ));
            }
            ProcessTermination::Exited(status) if !status.success() => {
                let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
                if stderr.contains("auth login") || stderr.contains("not logged in") {
                    return Err(Error::tool(
                        "github",
                        format!(
                            "GH_AUTH: `gh` is not authenticated. Run `gh auth login` and retry.{evidence}"
                        ),
                    ));
                }
                let code = status.code().unwrap_or(-1);
                return Err(Error::tool(
                    "github",
                    format!("GH_ERROR (exit {code}){evidence}"),
                ));
            }
            ProcessTermination::Exited(_) => {}
        }
        if output.stdout_truncated && !allow_truncated_stdout {
            return Err(Error::tool(
                "github",
                format!(
                    "GH_OUTPUT_LIMIT: `gh` invocation stdout exceeded {MAX_GH_STDOUT_BYTES} bytes{evidence}",
                ),
            ));
        }
        Ok(GhOutput {
            text: String::from_utf8_lossy(&output.stdout).into_owned(),
            truncated: output.stdout_truncated,
        })
    }

    /// Read op with the short-TTL cache.
    async fn run_gh_cached(&self, args: &[&str], allow_truncated_stdout: bool) -> Result<GhOutput> {
        let key_bytes = args
            .iter()
            .fold(0usize, |total, arg| total.saturating_add(arg.len()));
        let key = (key_bytes <= MAX_CACHE_KEY_BYTES).then(|| {
            args.iter()
                .map(|arg| (*arg).to_string())
                .collect::<Vec<_>>()
        });
        if let Some(key) = key.as_deref()
            && let Some(hit) = self.cache_get(key)
        {
            return Ok(hit);
        }
        let body = self
            .run_gh_with_timeout(args, GH_CALL_TIMEOUT, allow_truncated_stdout)
            .await?;
        if let Some(key) = key {
            self.cache_put(key, body.clone());
        }
        Ok(body)
    }

    /// Resolve `owner/repo`: explicit arg wins, else the cwd git remote.
    async fn resolve_repo(&self, explicit: Option<&str>) -> Result<String> {
        if let Some(repo) = explicit {
            let trimmed = repo.trim();
            if let Some((owner, name)) = trimmed.split_once('/')
                && !owner.is_empty()
                && !name.is_empty()
                && !name.contains('/')
            {
                return Ok(trimmed.to_string());
            }
            return Err(Error::tool(
                "github",
                "GH_REPO: expected a non-empty owner/repo slug",
            ));
        }
        let output = run_bounded_process(
            OsStr::new("git"),
            &["remote", "get-url", "origin"],
            &self.cwd,
            GIT_REMOTE_TIMEOUT,
            MAX_GIT_REMOTE_BYTES,
        )
        .await
        .map_err(|err| {
            Error::tool(
                "github",
                format!(
                    "GH_REPO: git remote failed: {}",
                    sanitize_process_diagnostic(&err.to_string())
                ),
            )
        })?;
        let evidence = stderr_evidence(&output);
        match &output.termination {
            ProcessTermination::TimedOut => {
                return Err(Error::tool(
                    "github",
                    format!("GH_REPO: git remote lookup timed out{evidence}"),
                ));
            }
            ProcessTermination::Cancelled => {
                return Err(Error::tool(
                    "github",
                    format!("GH_REPO: git remote lookup cancelled{evidence}"),
                ));
            }
            ProcessTermination::Exited(status) if !status.success() => {
                return Err(Error::tool(
                    "github",
                    format!(
                        "GH_REPO: no origin remote; pass repo: \"owner/name\" explicitly{evidence}"
                    ),
                ));
            }
            ProcessTermination::Exited(_) => {}
        }
        if output.stdout_truncated {
            return Err(Error::tool(
                "github",
                format!(
                    "GH_REPO: git remote output exceeded {MAX_GIT_REMOTE_BYTES} bytes{evidence}"
                ),
            ));
        }
        let url = String::from_utf8_lossy(&output.stdout);
        parse_repo_from_remote(url.trim()).ok_or_else(|| {
            Error::tool(
                "github",
                "GH_REPO: could not parse owner/repo from the origin remote",
            )
        })
    }
}

/// Parse `owner/repo` from ssh/https GitHub remote URL forms.
pub(crate) fn parse_repo_from_remote(url: &str) -> Option<String> {
    let stripped = url
        .strip_prefix("git@github.com:")
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))
        .or_else(|| url.strip_prefix("https://github.com/"))
        .or_else(|| url.strip_prefix("http://github.com/"))?;
    let stripped = stripped.strip_suffix(".git").unwrap_or(stripped);
    let mut parts = stripped.splitn(2, '/');
    let owner = parts.next()?.trim();
    let repo = parts.next()?.trim().trim_end_matches('/');
    if owner.is_empty() || repo.is_empty() || repo.contains('/') {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

/// Markdown card for a PR/issue `gh --json` payload.
fn format_item_card(kind: &str, item: &Value) -> String {
    use std::fmt::Write as _;
    let mut card = String::new();
    let title = item.get("title").and_then(Value::as_str).unwrap_or("?");
    let number = item.get("number").and_then(Value::as_u64).unwrap_or(0);
    let state = item.get("state").and_then(Value::as_str).unwrap_or("?");
    let author = item
        .get("author")
        .and_then(|a| a.get("login"))
        .and_then(Value::as_str)
        .unwrap_or("?");
    let _ = writeln!(card, "## {kind} #{number}: {title}");
    let _ = write!(card, "state: {state} · author: {author}");
    if let Some(labels) = item.get("labels").and_then(Value::as_array)
        && !labels.is_empty()
    {
        let names: Vec<&str> = labels
            .iter()
            .filter_map(|l| l.get("name").and_then(Value::as_str))
            .collect();
        let _ = write!(card, " · labels: {}", names.join(", "));
    }
    card.push('\n');
    if let Some(body) = item.get("body").and_then(Value::as_str)
        && !body.trim().is_empty()
    {
        let excerpt: String = body.chars().take(1200).collect();
        let _ = write!(card, "\n{excerpt}");
        if body.chars().count() > 1200 {
            card.push_str("\n… (body truncated)");
        }
        card.push('\n');
    }
    if let Some(comments) = item.get("comments").and_then(Value::as_array) {
        for comment in comments.iter().take(3) {
            let who = comment
                .get("author")
                .and_then(|a| a.get("login"))
                .and_then(Value::as_str)
                .unwrap_or("?");
            let text = comment.get("body").and_then(Value::as_str).unwrap_or("");
            let excerpt: String = text.chars().take(300).collect();
            let _ = write!(card, "\n> {who}: {excerpt}");
            card.push('\n');
        }
    }
    card
}

fn text_output(text: String, details: Value) -> ToolOutput {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(text))],
        details: Some(details),
        is_error: false,
    }
}

impl GithubTool {
    async fn op_view(&self, op: &str, input: &Value, repo_arg: Option<&str>) -> Result<ToolOutput> {
        let number = input
            .get("number")
            .and_then(Value::as_u64)
            .ok_or_else(|| Error::tool("github", "missing required field: number"))?
            .to_string();
        let repo = self.resolve_repo(repo_arg).await?;
        let (sub, fields, kind) = if op == "pr_view" {
            ("pr", PR_JSON_FIELDS, "PR")
        } else {
            ("issue", ISSUE_JSON_FIELDS, "Issue")
        };
        let raw = self
            .run_gh_cached(
                &[sub, "view", &number, "--repo", &repo, "--json", fields],
                false,
            )
            .await?;
        let item: Value = serde_json::from_str(&raw.text)
            .map_err(|err| Error::tool("github", format!("GH_PARSE: {err}")))?;
        Ok(text_output(format_item_card(kind, &item), item))
    }

    async fn op_pr_diff(&self, input: &Value, repo_arg: Option<&str>) -> Result<ToolOutput> {
        let number = input
            .get("number")
            .and_then(Value::as_u64)
            .ok_or_else(|| Error::tool("github", "missing required field: number"))?
            .to_string();
        let repo = self.resolve_repo(repo_arg).await?;
        let raw = self
            .run_gh_cached(&["pr", "diff", &number, "--repo", &repo], true)
            .await?;
        let (mut text, formatter_truncated) = truncate_diff(&raw.text);
        let truncated = raw.truncated || formatter_truncated;
        if raw.truncated && !formatter_truncated {
            text.push_str("… (diff truncated)\n");
        }
        Ok(text_output(
            text,
            json!({"repo": repo, "number": number, "truncated": truncated}),
        ))
    }

    async fn op_search(&self, input: &Value, limit: &str) -> Result<ToolOutput> {
        let query = input
            .get("query")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::tool("github", "missing required field: query"))?;
        let kind = input.get("kind").and_then(Value::as_str).unwrap_or("code");
        let raw = match kind {
            "code" => {
                self.run_gh_cached(
                    &[
                        "search",
                        "code",
                        query,
                        "--limit",
                        limit,
                        "--json",
                        "path,repository,textMatches",
                    ],
                    false,
                )
                .await?
            }
            "issues" => {
                self.run_gh_cached(
                    &[
                        "search",
                        "issues",
                        query,
                        "--limit",
                        limit,
                        "--json",
                        "number,title,state,repository,url",
                    ],
                    false,
                )
                .await?
            }
            "prs" => {
                self.run_gh_cached(
                    &[
                        "search",
                        "prs",
                        query,
                        "--limit",
                        limit,
                        "--json",
                        "number,title,state,repository,url",
                    ],
                    false,
                )
                .await?
            }
            other => {
                return Err(Error::tool(
                    "github",
                    format!("unknown search kind: {other} (code|issues|prs)"),
                ));
            }
        };
        let items: Value = serde_json::from_str(&raw.text)
            .map_err(|err| Error::tool("github", format!("GH_PARSE: {err}")))?;
        Ok(text_output(format_search_results(kind, &items), items))
    }

    async fn op_run_list(&self, repo_arg: Option<&str>, limit: &str) -> Result<ToolOutput> {
        let repo = self.resolve_repo(repo_arg).await?;
        let raw = self
            .run_gh_cached(
                &[
                    "run",
                    "list",
                    "--repo",
                    &repo,
                    "--limit",
                    limit,
                    "--json",
                    "databaseId,displayTitle,status,conclusion,workflowName,headBranch,createdAt",
                ],
                false,
            )
            .await?;
        let items: Value = serde_json::from_str(&raw.text)
            .map_err(|err| Error::tool("github", format!("GH_PARSE: {err}")))?;
        Ok(text_output(format_run_list(&items), items))
    }

    #[allow(clippy::too_many_lines)]
    async fn op_run_watch(
        &self,
        input: &Value,
        repo_arg: Option<&str>,
        on_update: Option<&(dyn Fn(ToolUpdate) + Send + Sync)>,
    ) -> Result<ToolOutput> {
        let run_id = parse_workflow_run_id(input)?;
        let repo = self.resolve_repo(repo_arg).await?;
        let budget = Duration::from_secs(
            input
                .get("timeout_secs")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_WATCH_TIMEOUT_SECS)
                .clamp(10, 6 * 3600),
        );
        let started = Instant::now();
        let mut poll = Duration::from_secs(2);
        loop {
            let elapsed = started.elapsed();
            let Some(remaining) = budget.checked_sub(elapsed) else {
                return Err(Error::tool(
                    "github",
                    format!(
                        "GH_WATCH_TIMEOUT: run {run_id} exceeded the {}s watch budget",
                        budget.as_secs()
                    ),
                ));
            };
            if remaining.is_zero() {
                return Err(Error::tool(
                    "github",
                    format!(
                        "GH_WATCH_TIMEOUT: run {run_id} exceeded the {}s watch budget",
                        budget.as_secs()
                    ),
                ));
            }
            // Watch reads bypass the cache (freshness IS the point).
            let result = self
                .run_gh_with_timeout(
                    &[
                        "run",
                        "view",
                        &run_id,
                        "--repo",
                        &repo,
                        "--json",
                        "status,conclusion,displayTitle,workflowName",
                    ],
                    remaining.min(GH_CALL_TIMEOUT),
                    false,
                )
                .await;
            let raw = match result {
                Ok(raw) => raw,
                Err(err) if started.elapsed() >= budget => {
                    return Err(Error::tool(
                        "github",
                        format!(
                            "GH_WATCH_TIMEOUT: run {run_id} exceeded the {}s watch budget; \
                             final poll: {err}",
                            budget.as_secs(),
                        ),
                    ));
                }
                Err(err) => return Err(err),
            };
            let state: Value = serde_json::from_str(&raw.text)
                .map_err(|err| Error::tool("github", format!("GH_PARSE: {err}")))?;
            let status = state.get("status").and_then(Value::as_str).unwrap_or("?");
            if status == "completed" {
                // A finished run changes PR check state: drop caches.
                self.cache_clear();
                let conclusion = state
                    .get("conclusion")
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                let title = state
                    .get("displayTitle")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                return Ok(text_output(
                    format!("Run {run_id} completed: {conclusion} — {title}"),
                    state,
                ));
            }
            if started.elapsed() >= budget {
                return Err(Error::tool(
                    "github",
                    format!(
                        "GH_WATCH_TIMEOUT: run {run_id} still `{status}` after {}s",
                        budget.as_secs()
                    ),
                ));
            }
            if let Some(update) = on_update {
                update(ToolUpdate {
                    content: vec![ContentBlock::Text(TextContent::new(format!(
                        "run {run_id}: {status} ({}s elapsed)",
                        started.elapsed().as_secs()
                    )))],
                    details: Some(state),
                });
            }
            let remaining = budget.saturating_sub(started.elapsed());
            sleep_with_cancellation(poll.min(remaining)).await?;
            poll = (poll * 2).min(Duration::from_secs(10));
        }
    }
}

fn parse_workflow_run_id(input: &Value) -> Result<String> {
    let id = match input.get("run_id") {
        Some(Value::String(raw)) => raw.parse::<u64>().ok(),
        Some(value) => value.as_u64(),
        None => return Err(Error::tool("github", "missing required field: run_id")),
    };
    id.filter(|id| *id > 0).map_or_else(
        || {
            Err(Error::tool(
                "github",
                "run_id must be a positive decimal integer",
            ))
        },
        |id| Ok(id.to_string()),
    )
}

const PR_JSON_FIELDS: &str = "number,title,state,author,labels,body,comments,url,headRefName,baseRefName,mergeable,reviewDecision";
const ISSUE_JSON_FIELDS: &str = "number,title,state,author,labels,body,comments,url";

#[async_trait::async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Tool for GithubTool {
    fn name(&self) -> &str {
        "github"
    }

    fn label(&self) -> &str {
        "GitHub"
    }

    fn description(&self) -> &str {
        "Structured GitHub operations via the gh CLI: pr_view, issue_view, pr_diff, \
         search (code/issues/prs), run_list, and run_watch (poll a workflow run \
         until it concludes). Repo defaults to the cwd origin remote; pass \
         repo: \"owner/name\" to override."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "op": {
                    "type": "string",
                    "enum": ["pr_view", "issue_view", "pr_diff", "search", "run_list", "run_watch"],
                    "description": "Operation to perform"
                },
                "number": {
                    "type": "integer",
                    "description": "PR/issue number (pr_view, issue_view, pr_diff)"
                },
                "query": {
                    "type": "string",
                    "description": "Search query (search)"
                },
                "kind": {
                    "type": "string",
                    "enum": ["code", "issues", "prs"],
                    "description": "What to search (search; default code)"
                },
                "run_id": {
                    "type": "string",
                    "description": "Workflow run id (run_watch)"
                },
                "repo": {
                    "type": "string",
                    "description": "owner/name override (default: cwd origin remote)"
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "run_watch total budget in seconds (default 900)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Max results for search/run_list (default 10)"
                }
            },
            "required": ["op"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: Value,
        on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let op = input
            .get("op")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::tool("github", "missing required field: op"))?;
        let repo_arg = input.get("repo").and_then(Value::as_str);
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(10)
            .clamp(1, 50)
            .to_string();

        match op {
            "pr_view" | "issue_view" => self.op_view(op, &input, repo_arg).await,
            "pr_diff" => self.op_pr_diff(&input, repo_arg).await,
            "search" => self.op_search(&input, &limit).await,
            "run_list" => self.op_run_list(repo_arg, &limit).await,
            "run_watch" => {
                self.op_run_watch(&input, repo_arg, on_update.as_deref())
                    .await
            }
            other => Err(Error::tool(
                "github",
                format!(
                    "unknown op: {other} (pr_view|issue_view|pr_diff|search|run_list|run_watch)"
                ),
            )),
        }
    }

    fn effects(&self) -> ToolEffects {
        // Every operation starts `gh`; implicit repo resolution also starts
        // `git` and reads cwd configuration. Process is a scheduling barrier.
        ToolEffects::read()
            .union(ToolEffects::network())
            .union(ToolEffects::process())
    }
}

async fn sleep_with_cancellation(duration: Duration) -> Result<()> {
    let cx = AgentCx::for_current_or_request();
    let started = Instant::now();
    while let Some(remaining) = duration.checked_sub(started.elapsed()) {
        if remaining.is_zero() {
            break;
        }
        if cx.checkpoint().is_err() {
            return Err(Error::tool(
                "github",
                "GH_CANCELLED: operation was cancelled",
            ));
        }
        asupersync::time::sleep(
            asupersync::time::wall_now(),
            remaining.min(PROCESS_POLL_INTERVAL),
        )
        .await;
    }
    Ok(())
}

fn truncate_diff(raw: &str) -> (String, bool) {
    let mut out = String::new();
    let mut truncated = false;
    for (index, line) in raw.lines().enumerate() {
        if index >= MAX_DIFF_LINES || out.len() + line.len() + 1 > MAX_DIFF_BYTES {
            truncated = true;
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    if truncated {
        out.push_str("… (diff truncated)\n");
    }
    (out, truncated)
}

fn format_search_results(kind: &str, items: &Value) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let empty = Vec::new();
    let list = items.as_array().unwrap_or(&empty);
    let _ = writeln!(out, "{} {kind} results:", list.len());
    for item in list {
        if kind == "code" {
            let repo = item
                .get("repository")
                .and_then(|r| {
                    r.get("nameWithOwner")
                        .or_else(|| r.get("fullName"))
                        .and_then(Value::as_str)
                })
                .unwrap_or("?");
            let path = item.get("path").and_then(Value::as_str).unwrap_or("?");
            let _ = writeln!(out, "- {repo}: {path}");
            if let Some(matches) = item.get("textMatches").and_then(Value::as_array) {
                for text_match in matches.iter().take(2) {
                    if let Some(fragment) = text_match.get("fragment").and_then(Value::as_str) {
                        let one_line = fragment.lines().next().unwrap_or("").trim();
                        let _ = writeln!(out, "    {one_line}");
                    }
                }
            }
        } else {
            let number = item.get("number").and_then(Value::as_u64).unwrap_or(0);
            let title = item.get("title").and_then(Value::as_str).unwrap_or("?");
            let state = item.get("state").and_then(Value::as_str).unwrap_or("?");
            let _ = writeln!(out, "- #{number} [{state}] {title}");
        }
    }
    out
}

fn format_run_list(items: &Value) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let empty = Vec::new();
    let list = items.as_array().unwrap_or(&empty);
    let _ = writeln!(out, "{} workflow runs:", list.len());
    for item in list {
        let id = item.get("databaseId").and_then(Value::as_u64).unwrap_or(0);
        let workflow = item
            .get("workflowName")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let status = item.get("status").and_then(Value::as_str).unwrap_or("?");
        let conclusion = item
            .get("conclusion")
            .and_then(Value::as_str)
            .unwrap_or("-");
        let branch = item
            .get("headBranch")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let _ = writeln!(out, "- {id} {workflow} [{branch}] {status}/{conclusion}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_repo_from_remote_forms() {
        for (url, expect) in [
            ("git@github.com:owner/repo.git", Some("owner/repo")),
            ("git@github.com:owner/repo", Some("owner/repo")),
            ("https://github.com/owner/repo.git", Some("owner/repo")),
            ("https://github.com/owner/repo", Some("owner/repo")),
            ("https://github.com/owner/repo/", Some("owner/repo")),
            ("ssh://git@github.com/owner/repo.git", Some("owner/repo")),
            ("https://gitlab.com/owner/repo.git", None),
            ("git@github.com:justowner", None),
        ] {
            assert_eq!(parse_repo_from_remote(url).as_deref(), expect, "url: {url}");
        }
    }

    #[test]
    fn subprocess_diagnostics_redact_secrets_and_escape_terminal_controls() {
        let diagnostic = sanitize_process_diagnostic(
            "failure: ghp_abcdefghijklmnopqrstuvwxyz\x1b]2;spoofed\u{202e}tail",
        );
        assert!(diagnostic.contains("[REDACTED_GITHUB_PAT]"));
        assert!(!diagnostic.contains("ghp_abcdefghijklmnopqrstuvwxyz"));
        assert!(!diagnostic.contains('\x1b'));
        assert!(!diagnostic.contains('\u{202e}'));
        assert!(diagnostic.contains("\\u{1b}"));
        assert!(diagnostic.contains("\\u{202e}"));
    }

    #[test]
    fn explicit_repo_requires_non_empty_owner_and_name() {
        asupersync::test_utils::run_test(|| async {
            let tool = GithubTool::new(Path::new("."), Some("/nonexistent/gh-binary"));
            assert_eq!(
                tool.resolve_repo(Some(" owner/repo "))
                    .await
                    .expect("valid repo"),
                "owner/repo"
            );
            for repo in ["/repo", "owner/", "owner/repo/extra"] {
                let err = tool
                    .resolve_repo(Some(repo))
                    .await
                    .expect_err("invalid repo slug");
                assert!(err.to_string().contains("GH_REPO"), "err: {err}");
            }
        });
    }

    #[test]
    fn workflow_run_id_is_positive_decimal_and_canonicalized() {
        assert_eq!(
            parse_workflow_run_id(&json!({"run_id": "00042"})).expect("string id"),
            "42"
        );
        assert_eq!(
            parse_workflow_run_id(&json!({"run_id": 42})).expect("numeric id"),
            "42"
        );
        for input in [
            json!({}),
            json!({"run_id": 0}),
            json!({"run_id": "run-42"}),
            json!({"run_id": "42\nGH_WATCH_TIMEOUT: spoofed"}),
        ] {
            assert!(parse_workflow_run_id(&input).is_err(), "input: {input}");
        }
    }

    #[test]
    fn invalid_workflow_run_id_is_rejected_before_spawn() {
        asupersync::test_utils::run_test(|| async {
            let tool = GithubTool::new(Path::new("."), Some("/nonexistent/gh-binary"));
            let err = tool
                .execute(
                    "t1",
                    json!({
                        "op": "run_watch",
                        "run_id": "123; ghp_abcdefghijklmnopqrstuvwxyz",
                        "repo": "owner/repo"
                    }),
                    None,
                )
                .await
                .expect_err("non-decimal workflow id must fail before gh spawn");
            assert!(
                err.to_string()
                    .contains("run_id must be a positive decimal integer"),
                "err: {err}"
            );
            assert!(!err.to_string().contains("GH_MISSING"), "err: {err}");
            assert!(
                !err.to_string().contains("ghp_abcdefghijklmnopqrstuvwxyz"),
                "err: {err}"
            );
        });
    }

    #[test]
    fn card_formats_title_state_labels_and_truncates_body() {
        let item = serde_json::json!({
            "number": 42,
            "title": "Fix the flux capacitor",
            "state": "OPEN",
            "author": {"login": "doc"},
            "labels": [{"name": "bug"}, {"name": "p1"}],
            "body": "b".repeat(2000),
            "comments": [
                {"author": {"login": "marty"}, "body": "works for me"}
            ]
        });
        let card = format_item_card("PR", &item);
        assert!(card.contains("PR #42: Fix the flux capacitor"));
        assert!(card.contains("state: OPEN · author: doc"));
        assert!(card.contains("labels: bug, p1"));
        assert!(card.contains("… (body truncated)"));
        assert!(card.contains("> marty: works for me"));
    }

    #[test]
    fn diff_truncation_bounds_lines() {
        let raw = "line\n".repeat(MAX_DIFF_LINES + 100);
        let (text, truncated) = truncate_diff(&raw);
        assert!(truncated);
        assert!(text.lines().count() <= MAX_DIFF_LINES + 1);
        assert!(text.ends_with("… (diff truncated)\n"));
    }

    #[test]
    fn cache_ttl_and_clear() {
        let tool = GithubTool::new(Path::new("."), None);
        let key = vec![String::from("k")];
        tool.cache_put(
            key.clone(),
            GhOutput {
                text: String::from("v"),
                truncated: false,
            },
        );
        assert_eq!(
            tool.cache_get(&key).map(|value| value.text).as_deref(),
            Some("v")
        );
        tool.cache_clear();
        assert!(tool.cache_get(&key).is_none());
    }

    #[test]
    fn cache_has_a_hard_entry_bound_and_collision_free_keys() {
        let tool = GithubTool::new(Path::new("."), None);
        for index in 0..(MAX_CACHE_ENTRIES + 8) {
            tool.cache_put(
                vec![format!("key-{index}")],
                GhOutput {
                    text: format!("value-{index}"),
                    truncated: false,
                },
            );
        }
        let cache = tool
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(cache.len(), MAX_CACHE_ENTRIES);
        drop(cache);

        let collision_tool = GithubTool::new(Path::new("."), None);
        let delimiter = "\u{1f}";
        collision_tool.cache_put(
            vec![format!("a{delimiter}b"), String::from("c")],
            GhOutput {
                text: String::from("first"),
                truncated: false,
            },
        );
        collision_tool.cache_put(
            vec![String::from("a"), format!("b{delimiter}c")],
            GhOutput {
                text: String::from("second"),
                truncated: false,
            },
        );
        assert_eq!(
            collision_tool
                .cache_get(&[format!("a{delimiter}b"), String::from("c")])
                .map(|value| value.text)
                .as_deref(),
            Some("first")
        );
        assert_eq!(
            collision_tool
                .cache_get(&[String::from("a"), format!("b{delimiter}c")])
                .map(|value| value.text)
                .as_deref(),
            Some("second")
        );
    }

    #[test]
    fn effects_truthfully_block_process_execution_in_plan_mode() {
        let tool = GithubTool::new(Path::new("."), None);
        let effects = tool.effects();
        assert!(effects.reads());
        assert!(effects.networks());
        assert!(effects.processes());
        assert!(!effects.parallel_safe());

        let plan = crate::plan::PlanState::new();
        plan.enter_planning();
        assert!(!plan.allows_effects(effects));
    }

    #[test]
    fn search_results_format_code_and_issues() {
        let code = serde_json::json!([{
            "repository": {"nameWithOwner": "o/r"},
            "path": "src/lib.rs",
            "textMatches": [{"fragment": "fn main() {}\nmore"}]
        }]);
        let out = format_search_results("code", &code);
        assert!(out.contains("o/r: src/lib.rs"));
        assert!(out.contains("fn main() {}"));

        let issues = serde_json::json!([{
            "number": 7, "title": "Broken", "state": "open"
        }]);
        let out = format_search_results("issues", &issues);
        assert!(out.contains("#7 [open] Broken"));
    }

    #[cfg(unix)]
    fn wait_for_pid_exit(pid: u32, timeout: Duration) -> bool {
        let pid = sysinfo::Pid::from_u32(pid);
        let started = Instant::now();
        loop {
            let mut system = sysinfo::System::new();
            system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
            if system.process(pid).is_none() {
                return true;
            }
            if started.elapsed() >= timeout {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(unix)]
    #[test]
    // The capture limits are small compile-time constants (<= 1MB), far below
    // usize::MAX on any supported target, so the casts cannot truncate.
    #[allow(clippy::cast_possible_truncation)]
    fn production_capture_is_bounded_and_reports_json_overflow() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tool = GithubTool::new(dir.path(), Some("/bin/sh"));
        let script = "i=0; while [ \"$i\" -lt 16000 ]; do printf '0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef'; i=$((i + 1)); done";
        let both_streams_script = "i=0; while [ \"$i\" -lt 16000 ]; do printf '0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef'; printf 'fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210' >&2; i=$((i + 1)); done";
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let bounded = run_bounded_process(
                OsStr::new("/bin/sh"),
                &["-c", both_streams_script],
                dir.path(),
                Duration::from_secs(5),
                1024,
            )
            .await
            .expect("bounded subprocess capture");
            assert!(matches!(
                &bounded.termination,
                ProcessTermination::Exited(status) if status.success()
            ));
            assert!(bounded.stdout_truncated);
            assert!(bounded.stderr_truncated);
            assert_eq!(bounded.stdout.len(), 1024);
            assert_eq!(bounded.stderr.len(), MAX_GH_STDERR_BYTES as usize);

            let allowed = tool
                .run_gh_with_timeout(&["-c", script], Duration::from_secs(5), true)
                .await
                .expect("bounded diff-style capture");
            assert!(allowed.truncated);
            assert_eq!(allowed.text.len(), MAX_GH_STDOUT_BYTES as usize);

            let err = tool
                .run_gh_with_timeout(&["-c", script], Duration::from_secs(5), false)
                .await
                .expect_err("JSON-style output must reject an incomplete document");
            let message = err.to_string();
            assert!(message.contains("GH_OUTPUT_LIMIT"), "err: {message}");
            assert!(
                message.contains(&MAX_GH_STDOUT_BYTES.to_string()),
                "err: {message}"
            );
            assert!(
                !message.contains(script),
                "output-limit error leaked the command payload: {message}"
            );

            let err = tool
                .run_gh_with_timeout(
                    &[
                        "-c",
                        "printf 'evidence ghp_abcdefghijklmnopqrstuvwxyz \\033]2;spoofed' >&2; exit 7",
                    ],
                    Duration::from_secs(5),
                    false,
                )
                .await
                .expect_err("non-zero exit must retain stderr evidence");
            let message = err.to_string();
            assert!(message.contains("GH_ERROR (exit 7)"), "err: {message}");
            assert!(message.contains("evidence [REDACTED_GITHUB_PAT]"), "err: {message}");
            assert!(!message.contains("ghp_abcdefghijklmnopqrstuvwxyz"), "err: {message}");
            assert!(!message.contains('\x1b'), "err: {message}");
            assert!(message.contains("\\u{1b}"), "err: {message}");
        });
    }

    #[cfg(unix)]
    #[test]
    fn timeout_kills_descendants_and_reaps_root_before_return() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root_pid_path = dir.path().join("root.pid");
        let leak_path = dir.path().join("leaked");
        let script = format!(
            "echo $$ > '{}'; (sleep 1; printf leaked > '{}') & wait",
            root_pid_path.display(),
            leak_path.display()
        );
        let tool = GithubTool::new(dir.path(), Some("/bin/sh"));
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .build()
            .expect("runtime build");
        let err = runtime.block_on(async {
            tool.run_gh_with_timeout(&["-c", &script], Duration::from_millis(100), false)
                .await
                .expect_err("blocking process must time out")
        });
        assert!(err.to_string().contains("GH_TIMEOUT"), "err: {err}");
        assert!(
            !err.to_string().contains(&script),
            "timeout error leaked the command payload: {err}"
        );

        let root_pid = std::fs::read_to_string(&root_pid_path)
            .expect("root pid")
            .trim()
            .parse::<u32>()
            .expect("numeric root pid");
        assert!(
            wait_for_pid_exit(root_pid, Duration::from_secs(1)),
            "root process {root_pid} was not reaped"
        );
        std::thread::sleep(Duration::from_millis(1100));
        assert!(
            !leak_path.exists(),
            "descendant survived timeout and performed a delayed side effect"
        );
    }

    #[cfg(unix)]
    #[test]
    fn dropping_production_future_kills_tree_before_drop_returns() {
        use futures::future::{Either, select};

        let dir = tempfile::tempdir().expect("tempdir");
        let root_pid_path = dir.path().join("root.pid");
        let leak_path = dir.path().join("leaked");
        let script = format!(
            "echo $$ > '{}'; (sleep 1; printf leaked > '{}') & wait",
            root_pid_path.display(),
            leak_path.display()
        );
        let tool = GithubTool::new(dir.path(), Some("/bin/sh"));
        // Bind the argv outside the async block: `&["-c", &script]` would
        // create a temporary that dies while the future still borrows it.
        let args = ["-c", script.as_str()];
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let process = Box::pin(tool.run_gh_with_timeout(&args, Duration::from_secs(5), false));
            let cancellation = Box::pin(asupersync::time::sleep(
                asupersync::time::wall_now(),
                Duration::from_millis(100),
            ));
            match select(process, cancellation).await {
                Either::Left((result, _)) => {
                    panic!("process exited before cancellation: {result:?}")
                }
                Either::Right(((), pending_process)) => drop(pending_process),
            }
        });

        let root_pid = std::fs::read_to_string(&root_pid_path)
            .expect("root pid")
            .trim()
            .parse::<u32>()
            .expect("numeric root pid");
        assert!(
            wait_for_pid_exit(root_pid, Duration::from_millis(100)),
            "future drop returned before root process {root_pid} was reaped"
        );
        std::thread::sleep(Duration::from_millis(1100));
        assert!(
            !leak_path.exists(),
            "descendant survived future cancellation and performed a delayed side effect"
        );
    }

    /// Hermetic execute() test with a canned `gh` stub on ghPath.
    #[cfg(unix)]
    #[test]
    fn execute_pr_view_via_stub_gh() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let stub = dir.path().join("gh");
        std::fs::write(
            &stub,
            "#!/bin/sh\necho '{\"number\": 5, \"title\": \"Stubbed\", \"state\": \"MERGED\", \"author\": {\"login\": \"bot\"}, \"labels\": [], \"body\": \"ok\", \"comments\": []}'\n",
        )
        .expect("write stub");
        let mut perms = std::fs::metadata(&stub).expect("stat").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&stub, perms).expect("chmod");

        // Some endpoint-security setups stall exec() of freshly written
        // unsigned scripts from monitored processes (observed on darwin:
        // com.apple.provenance + EDR). Real `gh` is a signed binary and is
        // unaffected; probe and skip rather than hang the suite.
        {
            use std::process::{Command, Stdio};
            let probe = Command::new(&stub)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn();
            let Ok(mut probe) = probe else {
                eprintln!("Skipping: cannot spawn stub");
                return;
            };
            let started = std::time::Instant::now();
            loop {
                match probe.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if started.elapsed() > Duration::from_secs(2) => {
                        let _ = probe.kill();
                        let _ = probe.wait();
                        eprintln!("Skipping: host stalls exec of fresh scripts (security tooling)");
                        return;
                    }
                    Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                    Err(_) => break,
                }
            }
        }

        // Use the real runtime for wall-clock polling and real subprocess I/O.
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .build()
            .expect("runtime build");
        let dir_path = dir.path().to_path_buf();
        runtime.block_on(async move {
            let tool = GithubTool::new(&dir_path, Some(stub.to_str().expect("utf8 path")));
            let output = tool
                .execute(
                    "t1",
                    serde_json::json!({"op": "pr_view", "number": 5, "repo": "o/r"}),
                    None,
                )
                .await
                .expect("execute");
            assert!(!output.is_error);
            let text = match &output.content[0] {
                // ubs:ignore test index — single-block output is the assertion
                ContentBlock::Text(text) => &text.text,
                other => panic!("unexpected block: {other:?}"), // ubs:ignore test assertion panic
            };
            assert!(text.contains("PR #5: Stubbed"), "card: {text}");
        });
    }

    /// Missing binary maps to the named GH_MISSING error.
    #[test]
    fn missing_gh_is_named_error() {
        asupersync::test_utils::run_test(|| async {
            let malicious_path =
                "/nonexistent/ghp_abcdefghijklmnopqrstuvwxyz\x1b]2;spoofed\u{202e}gh-binary";
            let tool = GithubTool::new(Path::new("."), Some(malicious_path));
            let err = tool
                .execute(
                    "t1",
                    serde_json::json!({"op": "pr_view", "number": 1, "repo": "o/r"}),
                    None,
                )
                .await
                .expect_err("should fail");
            let message = err.to_string();
            assert!(message.contains("GH_MISSING"), "err: {message}");
            assert!(
                !message.contains("ghp_abcdefghijklmnopqrstuvwxyz"),
                "err: {message}"
            );
            assert!(!message.contains('\x1b'), "err: {message}");
            assert!(!message.contains('\u{202e}'), "err: {message}");
        });
    }
}
