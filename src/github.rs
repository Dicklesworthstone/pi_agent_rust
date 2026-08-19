//! GitHub tool (bd-cv653.2.3): structured `gh`-backed PR/issue/search/Actions
//! operations, replacing ad-hoc `bash` shell-outs with typed responses,
//! short-TTL caching, and a single named error taxonomy.
//!
//! Backend: the `gh` CLI (path from `config.ghPath`, default `gh`), always via
//! `--json`/`gh api` style typed output — never HTML scraping. Operations:
//! `pr_view`, `issue_view`, `pr_diff`, `search` (code/issues/prs),
//! `run_list`, `run_watch` (poll until a terminal conclusion, streaming
//! status through `on_update` like `bash` does).

use crate::error::{Error, Result};
use crate::model::{ContentBlock, TextContent};
use crate::tools::{Tool, ToolEffects, ToolOutput, ToolUpdate};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Read-cache TTL: repeated PR/issue reads within a turn are common and gh is
/// slow (network); short enough that watch/refresh flows stay honest.
const CACHE_TTL: Duration = Duration::from_secs(30);
/// Default total budget for `run_watch`.
const DEFAULT_WATCH_TIMEOUT_SECS: u64 = 900;
/// Per-invocation budget for a single `gh` call.
const GH_CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// Diff output bounds (standard truncation contract).
const MAX_DIFF_LINES: usize = 2000;
const MAX_DIFF_BYTES: usize = 1_000_000;

pub struct GithubTool {
    cwd: PathBuf,
    gh_path: String,
    cache: Mutex<HashMap<String, (Instant, String)>>,
}

impl GithubTool {
    pub fn new(cwd: &Path, gh_path: Option<&str>) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            gh_path: gh_path.unwrap_or("gh").to_string(),
            cache: Mutex::new(HashMap::new()),
        }
    }

    fn cache_get(&self, key: &str) -> Option<String> {
        let cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache
            .get(key)
            .filter(|(at, _)| at.elapsed() < CACHE_TTL)
            .map(|(_, body)| body.clone())
    }

    fn cache_put(&self, key: String, body: String) {
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.retain(|_, (at, _)| at.elapsed() < CACHE_TTL);
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
    async fn run_gh(&self, args: &[&str]) -> Result<String> {
        use std::process::{Command, Stdio};

        let mut cmd = Command::new(&self.gh_path);
        cmd.args(args)
            .current_dir(&self.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = match cmd.spawn() {
            Ok(child) => child,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::tool(
                    "github",
                    format!(
                        "GH_MISSING: `{}` not found. Install the GitHub CLI \
                         (https://cli.github.com) or set ghPath in settings.",
                        self.gh_path
                    ),
                ));
            }
            Err(err) => return Err(Error::tool("github", format!("GH_SPAWN: {err}"))),
        };

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("gh-wait".into())
            .spawn(move || {
                let _ = tx.send(child.wait_with_output());
            })
            .map_err(|err| Error::tool("github", format!("GH_SPAWN: {err}")))?;

        let started = Instant::now();
        let output = loop {
            match rx.try_recv() {
                Ok(result) => {
                    break result
                        .map_err(|err| Error::tool("github", format!("GH_WAIT: {err}")))?;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    if started.elapsed() > GH_CALL_TIMEOUT {
                        return Err(Error::tool(
                            "github",
                            format!("GH_TIMEOUT: `gh {}` exceeded 60s", args.join(" ")),
                        ));
                    }
                    asupersync::time::sleep(
                        asupersync::time::wall_now(),
                        Duration::from_millis(50),
                    )
                    .await;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    return Err(Error::tool("github", "GH_WAIT: worker vanished"));
                }
            }
        };

        let stderr = String::from_utf8_lossy(&output.stderr);
        if !output.status.success() {
            let lower = stderr.to_lowercase();
            if lower.contains("auth login") || lower.contains("not logged in") {
                return Err(Error::tool(
                    "github",
                    "GH_AUTH: `gh` is not authenticated. Run `gh auth login` and retry.",
                ));
            }
            let code = output.status.code().unwrap_or(-1);
            let brief: String = stderr.lines().take(4).collect::<Vec<_>>().join("\n");
            return Err(Error::tool(
                "github",
                format!("GH_ERROR (exit {code}): {brief}"),
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Read op with the short-TTL cache.
    async fn run_gh_cached(&self, args: &[&str]) -> Result<String> {
        let key = args.join("\u{1f}");
        if let Some(hit) = self.cache_get(&key) {
            return Ok(hit);
        }
        let body = self.run_gh(args).await?;
        self.cache_put(key, body.clone());
        Ok(body)
    }

    /// Resolve `owner/repo`: explicit arg wins, else the cwd git remote.
    async fn resolve_repo(&self, explicit: Option<&str>) -> Result<String> {
        if let Some(repo) = explicit {
            let trimmed = repo.trim();
            if trimmed.split('/').count() == 2 && !trimmed.ends_with('/') {
                return Ok(trimmed.to_string());
            }
            return Err(Error::tool(
                "github",
                format!("GH_REPO: expected owner/repo, got {trimmed:?}"),
            ));
        }
        let output = std::process::Command::new("git")
            .args(["remote", "get-url", "origin"])
            .current_dir(&self.cwd)
            .output()
            .map_err(|err| Error::tool("github", format!("GH_REPO: git remote failed: {err}")))?;
        if !output.status.success() {
            return Err(Error::tool(
                "github",
                "GH_REPO: no origin remote; pass repo: \"owner/name\" explicitly",
            ));
        }
        let url = String::from_utf8_lossy(&output.stdout);
        parse_repo_from_remote(url.trim()).ok_or_else(|| {
            Error::tool(
                "github",
                format!("GH_REPO: could not parse owner/repo from remote {url:?}"),
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
            .run_gh_cached(&[sub, "view", &number, "--repo", &repo, "--json", fields])
            .await?;
        let item: Value = serde_json::from_str(&raw)
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
            .run_gh_cached(&["pr", "diff", &number, "--repo", &repo])
            .await?;
        let (text, truncated) = truncate_diff(&raw);
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
                self.run_gh_cached(&[
                    "search",
                    "code",
                    query,
                    "--limit",
                    limit,
                    "--json",
                    "path,repository,textMatches",
                ])
                .await?
            }
            "issues" => {
                self.run_gh_cached(&[
                    "search",
                    "issues",
                    query,
                    "--limit",
                    limit,
                    "--json",
                    "number,title,state,repository,url",
                ])
                .await?
            }
            "prs" => {
                self.run_gh_cached(&[
                    "search",
                    "prs",
                    query,
                    "--limit",
                    limit,
                    "--json",
                    "number,title,state,repository,url",
                ])
                .await?
            }
            other => {
                return Err(Error::tool(
                    "github",
                    format!("unknown search kind: {other} (code|issues|prs)"),
                ));
            }
        };
        let items: Value = serde_json::from_str(&raw)
            .map_err(|err| Error::tool("github", format!("GH_PARSE: {err}")))?;
        Ok(text_output(format_search_results(kind, &items), items))
    }

    async fn op_run_list(&self, repo_arg: Option<&str>, limit: &str) -> Result<ToolOutput> {
        let repo = self.resolve_repo(repo_arg).await?;
        let raw = self
            .run_gh_cached(&[
                "run",
                "list",
                "--repo",
                &repo,
                "--limit",
                limit,
                "--json",
                "databaseId,displayTitle,status,conclusion,workflowName,headBranch,createdAt",
            ])
            .await?;
        let items: Value = serde_json::from_str(&raw)
            .map_err(|err| Error::tool("github", format!("GH_PARSE: {err}")))?;
        Ok(text_output(format_run_list(&items), items))
    }

    async fn op_run_watch(
        &self,
        input: &Value,
        repo_arg: Option<&str>,
        on_update: Option<&(dyn Fn(ToolUpdate) + Send + Sync)>,
    ) -> Result<ToolOutput> {
        let run_id = input
            .get("run_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                input
                    .get("run_id")
                    .and_then(Value::as_u64)
                    .map(|id| id.to_string())
            })
            .ok_or_else(|| Error::tool("github", "missing required field: run_id"))?;
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
            // Watch reads bypass the cache (freshness IS the point).
            let raw = self
                .run_gh(&[
                    "run",
                    "view",
                    &run_id,
                    "--repo",
                    &repo,
                    "--json",
                    "status,conclusion,displayTitle,workflowName",
                ])
                .await?;
            let state: Value = serde_json::from_str(&raw)
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
            if started.elapsed() > budget {
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
            asupersync::time::sleep(asupersync::time::wall_now(), poll).await;
            poll = (poll * 2).min(Duration::from_secs(10));
        }
    }
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
        // Network reads only in v1 (no checkout/write operations).
        ToolEffects::network()
    }
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

// Keep OsString referenced for future arg-building parity with share.rs.
#[allow(dead_code)]
type ArgList = Vec<OsString>;

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
        tool.cache_put(String::from("k"), String::from("v"));
        assert_eq!(tool.cache_get("k").as_deref(), Some("v"));
        tool.cache_clear();
        assert!(tool.cache_get("k").is_none());
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
                        eprintln!("Skipping: host stalls exec of fresh scripts (security tooling)");
                        return;
                    }
                    Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                    Err(_) => break,
                }
            }
        }

        // Real runtime: run_gh polls a std channel between wall-clock
        // sleeps, which the deterministic test runtime never advances.
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
            let tool = GithubTool::new(Path::new("."), Some("/nonexistent/gh-binary"));
            let err = tool
                .execute(
                    "t1",
                    serde_json::json!({"op": "pr_view", "number": 1, "repo": "o/r"}),
                    None,
                )
                .await
                .expect_err("should fail");
            assert!(err.to_string().contains("GH_MISSING"), "err: {err}");
        });
    }
}
