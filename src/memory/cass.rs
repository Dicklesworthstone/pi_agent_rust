//! Optional project-history recall from the owner's installed CASS archive.
//!
//! The read-only CLI contract is verified in CASS's `docs/ROBOT_MODE.md`,
//! `tests/fixtures/cli_contract/introspect.json`, and the real CLI regression
//! `no_maintenance_lexical_search_is_byte_stable_across_the_real_cli_path`:
//! <https://github.com/Dicklesworthstone/coding_agent_session_search>.
//! `--no-maintenance` is mandatory: an older binary that rejects it is
//! unavailable, never retried with maintenance enabled. No index, repair,
//! installation, model inference, daemon, or source-file expansion is requested.
//!
//! The local memory bank remains authoritative and available independently.
//! Historical excerpts are derived from complete content screened with that
//! bank's privacy policy before shortening or rendering. Upstream snippets and
//! titles can omit credential context and are never used. Responses too large
//! to capture whole leave history unavailable instead of returning fragments.
//! Source paths are citations, never files to open or commands to run. Unix uses
//! owned, bounded nonblocking subprocess capture; other platforms explicitly
//! decline this optional bridge until they have an equivalent collector,
//! without introducing detached blocking reader threads.

use crate::agent_cx::AgentCx;
#[cfg(any(unix, test))]
use serde::Deserialize;
use serde::Serialize;
use std::path::Path;

#[cfg(unix)]
use crate::agent_cx::AgentCommand;
#[cfg(unix)]
use std::ffi::OsStr;
#[cfg(unix)]
use std::process::Stdio;
#[cfg(unix)]
use std::time::Duration;

#[cfg(any(unix, test))]
const MAX_HITS: usize = 20;
#[cfg(unix)]
const MAX_QUERY_BYTES: usize = 4096;
#[cfg(any(unix, test))]
const MAX_OUTPUT_BYTES: usize = 256 * 1024;
#[cfg(any(unix, test))]
const MAX_RESULT_BYTES: usize = 64 * 1024;
#[cfg(any(unix, test))]
const MAX_RAW_FIELD_BYTES: usize = 64 * 1024;
#[cfg(any(unix, test))]
const MAX_SNIPPET_BYTES: usize = 2048;
#[cfg(any(unix, test))]
const MAX_PATH_BYTES: usize = 4096;
#[cfg(any(unix, test))]
const MAX_LABEL_BYTES: usize = 512;
#[cfg(unix)]
const SEARCH_FIELDS: &str =
    "source_path,line_number,agent,workspace,content,source_id,origin_kind,origin_host";

type Screener<'a> = &'a (dyn Fn(&str) -> String + Send + Sync);

/// Completion and degradation are separate from an empty, completed search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CassStatus {
    #[cfg_attr(not(any(unix, test)), allow(dead_code))]
    Completed,
    #[cfg_attr(not(any(unix, test)), allow(dead_code))]
    Unavailable,
    #[cfg_attr(not(any(unix, test)), allow(dead_code))]
    Timeout,
    Cancelled,
    #[cfg_attr(unix, allow(dead_code))] // Retain one status schema on every platform.
    Unsupported,
}

/// Fixed diagnostic codes never contain a query, stdout, stderr, or OS error.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CassSearch {
    pub status: CassStatus,
    pub hits: Vec<CassHit>,
    pub diagnostic: Option<&'static str>,
    pub truncated: bool,
}

impl CassSearch {
    const fn unavailable(status: CassStatus, diagnostic: &'static str) -> Self {
        Self {
            status,
            hits: Vec::new(),
            diagnostic: Some(diagnostic),
            truncated: false,
        }
    }
}

/// A screened archive excerpt with its source/line citation.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CassHit {
    pub source_path: String,
    pub line_number: Option<u64>,
    pub agent: String,
    pub workspace: String,
    /// Reserved for a complete archive title; excerpt-derived titles are omitted.
    pub title: Option<String>,
    pub snippet: String,
    pub provenance: CassProvenance,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CassProvenance {
    pub source: &'static str,
    pub mode: &'static str,
    pub read_only: bool,
    pub source_id: Option<String>,
    pub origin_kind: Option<String>,
    pub origin_host: Option<String>,
}

/// Run one bounded, project-scoped lexical search. Every failure leaves the
/// caller free to return its local bank results; cancellation is distinguished
/// so a cancelled tool does not report successful partial recall.
#[cfg_attr(not(unix), allow(clippy::unused_async))]
pub(crate) async fn search(
    owner: &AgentCx,
    project_root: &Path,
    query: &str,
    limit: usize,
    screen: Screener<'_>,
) -> CassSearch {
    #[cfg(unix)]
    {
        search_with_program(
            owner,
            project_root,
            query,
            limit,
            screen,
            OsStr::new("cass"),
            Duration::from_secs(5),
        )
        .await
    }
    #[cfg(not(unix))]
    {
        let _ = (project_root, query, limit, screen);
        if owner.checkpoint().is_err() {
            return CassSearch::unavailable(CassStatus::Cancelled, "CASS_CANCELLED");
        }
        CassSearch::unavailable(CassStatus::Unsupported, "CASS_PLATFORM_UNSUPPORTED")
    }
}

#[cfg(unix)]
fn command(
    owner: &AgentCx,
    project_root: &Path,
    query: &str,
    limit: usize,
    program: &OsStr,
) -> AgentCommand {
    let mut command = owner.process().command(program);
    command
        .current_dir(project_root)
        .args([
            "search",
            "--robot",
            "--robot-format",
            "json",
            "--robot-meta",
            "--mode",
            "lexical",
            "--no-maintenance",
            "--no-daemon",
            "--workspace",
        ])
        .arg(project_root)
        .args(["--limit", &limit.clamp(1, MAX_HITS).to_string()])
        .args(["--fields", SEARCH_FIELDS])
        // Content must reach Pi's screener whole. CLI length/token budgets can
        // remove the assignment or key envelope that makes a value detectable.
        // The owned collector below imposes the hard limit without partial data.
        .args(["--timeout", "2000", "--"])
        .arg(query)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

#[cfg(unix)]
async fn search_with_program(
    owner: &AgentCx,
    project_root: &Path,
    query: &str,
    limit: usize,
    screen: Screener<'_>,
    program: &OsStr,
    timeout: Duration,
) -> CassSearch {
    if owner.checkpoint().is_err() {
        return CassSearch::unavailable(CassStatus::Cancelled, "CASS_CANCELLED");
    }
    let capabilities = owner.capabilities();
    if !capabilities.io || !capabilities.spawn || !capabilities.time {
        return CassSearch::unavailable(CassStatus::Unavailable, "CASS_CAPABILITY_UNAVAILABLE");
    }
    // Do not truncate a query into a different search or split a credential
    // before the caller's detector can screen it.
    if query.len() > MAX_QUERY_BYTES || query.contains('\0') {
        return CassSearch::unavailable(CassStatus::Unavailable, "CASS_QUERY_INVALID");
    }
    let query = screen(query);
    let query = query.trim();
    if query.is_empty() || query.len() > MAX_QUERY_BYTES || query.contains('\0') {
        return CassSearch::unavailable(CassStatus::Unavailable, "CASS_QUERY_INVALID");
    }
    let Ok(project_root) = project_root.canonicalize() else {
        return CassSearch::unavailable(CassStatus::Unavailable, "CASS_WORKSPACE_UNAVAILABLE");
    };
    if !project_root.is_dir() || project_root.to_str().is_none() {
        return CassSearch::unavailable(CassStatus::Unavailable, "CASS_WORKSPACE_UNAVAILABLE");
    }
    let child = match command(owner, &project_root, query, limit, program).spawn() {
        Ok(child) => child,
        Err(error) => {
            let (status, diagnostic) = match error.kind() {
                std::io::ErrorKind::Interrupted => (CassStatus::Cancelled, "CASS_CANCELLED"),
                std::io::ErrorKind::NotFound => (CassStatus::Unavailable, "CASS_NOT_INSTALLED"),
                _ => (CassStatus::Unavailable, "CASS_SPAWN_FAILED"),
            };
            return CassSearch::unavailable(status, diagnostic);
        }
    };
    let output = match child
        .wait_with_output_limited(MAX_OUTPUT_BYTES, timeout)
        .await
    {
        Ok(output) => output,
        Err(error) => {
            let (status, diagnostic) = match error.kind() {
                std::io::ErrorKind::Interrupted => (CassStatus::Cancelled, "CASS_CANCELLED"),
                std::io::ErrorKind::TimedOut => (CassStatus::Timeout, "CASS_TIMEOUT"),
                _ => (CassStatus::Unavailable, "CASS_CAPTURE_FAILED"),
            };
            return CassSearch::unavailable(status, diagnostic);
        }
    };
    if owner.checkpoint().is_err() {
        return CassSearch::unavailable(CassStatus::Cancelled, "CASS_CANCELLED");
    }
    if !output.status.success() {
        return CassSearch::unavailable(
            CassStatus::Unavailable,
            failure_diagnostic(&output.stderr),
        );
    }
    let result = parse_output(&output.stdout, &project_root, limit, screen);
    if owner.checkpoint().is_err() {
        return CassSearch::unavailable(CassStatus::Cancelled, "CASS_CANCELLED");
    }
    result
}

#[cfg(unix)]
fn failure_diagnostic(stderr: &[u8]) -> &'static str {
    let kind = serde_json::from_slice::<serde_json::Value>(stderr)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/kind")
                .or_else(|| value.pointer("/err/kind"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        });
    match kind.as_deref() {
        Some("maintenance-required") => "CASS_MAINTENANCE_REQUIRED",
        Some("index-busy") => "CASS_INDEX_BUSY",
        _ => "CASS_SEARCH_FAILED",
    }
}

#[cfg(any(unix, test))]
#[derive(Deserialize)]
struct WireSearch {
    hits: Vec<WireHit>,
    #[serde(default)]
    hits_clamped: bool,
    budget: Option<WireBudget>,
    #[serde(rename = "_meta")]
    meta: Option<WireMeta>,
    #[serde(rename = "_timeout")]
    timeout: Option<serde_json::Value>,
    error: Option<serde_json::Value>,
    err: Option<serde_json::Value>,
}

#[cfg(any(unix, test))]
#[derive(Deserialize)]
struct WireBudget {
    timed_out: bool,
}

#[cfg(any(unix, test))]
#[derive(Default, Deserialize)]
struct WireMeta {
    timed_out: Option<bool>,
    partial_results: Option<bool>,
    hits_clamped: Option<bool>,
}

#[cfg(any(unix, test))]
#[derive(Deserialize)]
struct WireHit {
    source_path: String,
    line_number: Option<u64>,
    agent: String,
    workspace: Option<String>,
    content: Option<String>,
    // CASS emits this marker only when content was shortened, including with
    // custom --fields selection (tests/cli_robot.rs). Any non-null marker
    // makes that source incomplete, irrespective of the marker's value/type.
    content_truncated: Option<serde_json::Value>,
    source_id: Option<String>,
    origin_kind: Option<String>,
    origin_host: Option<String>,
}

#[cfg(any(unix, test))]
fn parse_output(
    bytes: &[u8],
    project_root: &Path,
    limit: usize,
    screen: Screener<'_>,
) -> CassSearch {
    if bytes.len() > MAX_OUTPUT_BYTES {
        return CassSearch::unavailable(CassStatus::Unavailable, "CASS_OUTPUT_LIMIT");
    }
    let Ok(wire) = serde_json::from_slice::<WireSearch>(bytes) else {
        return CassSearch::unavailable(CassStatus::Unavailable, "CASS_OUTPUT_INVALID");
    };
    let meta = wire.meta.unwrap_or_default();
    if wire.budget.is_some_and(|budget| budget.timed_out)
        || meta.timed_out.unwrap_or(false)
        || wire.timeout.is_some()
    {
        return CassSearch::unavailable(CassStatus::Timeout, "CASS_TIMEOUT");
    }
    if wire.error.is_some() || wire.err.is_some() {
        return CassSearch::unavailable(CassStatus::Unavailable, "CASS_SEARCH_FAILED");
    }
    let limit = limit.clamp(1, MAX_HITS);
    let had_hits = !wire.hits.is_empty();
    let mut truncated =
        wire.hits_clamped || meta.hits_clamped.unwrap_or(false) || wire.hits.len() > limit;
    let mut filtered = false;
    let mut hits = Vec::new();
    let mut remaining = MAX_RESULT_BYTES;
    for raw in wire.hits {
        if hits.len() >= limit {
            truncated = true;
            break;
        }
        let Some((hit, shortened)) = screen_hit(raw, project_root, screen) else {
            filtered = true;
            continue;
        };
        let size = hit_text_bytes(&hit);
        if size > remaining {
            truncated = true;
            break;
        }
        remaining -= size;
        truncated |= shortened;
        hits.push(hit);
    }
    if had_hits && hits.is_empty() && filtered {
        return CassSearch::unavailable(CassStatus::Unavailable, "CASS_SCOPE_OR_OUTPUT_MISMATCH");
    }
    CassSearch {
        status: CassStatus::Completed,
        hits,
        diagnostic: if filtered {
            Some("CASS_RESULTS_FILTERED")
        } else if meta.partial_results.unwrap_or(false) {
            Some("CASS_PARTIAL_RESULTS")
        } else {
            None
        },
        truncated: truncated || meta.partial_results.unwrap_or(false),
    }
}

#[cfg(any(unix, test))]
fn screen_hit(
    raw: WireHit,
    project_root: &Path,
    screen: Screener<'_>,
) -> Option<(CassHit, bool)> {
    let workspace = raw.workspace?;
    // Validate identity on the raw path, before redaction can change it.
    // Do not canonicalize arbitrary paths supplied by another process. The
    // query already used the real canonical root; a different project, a
    // descendant directory, or an absent workspace must never leak through.
    if Path::new(&workspace) != project_root
        || raw.source_path.is_empty()
        || raw.source_path.chars().any(display_control)
        || raw.source_path.len() > MAX_PATH_BYTES
        || raw.agent.is_empty()
    {
        return None;
    }
    // A snippet or generated title may have dropped the `password=` or PEM
    // header that identifies a credential in the full message. Never fall back
    // to either one, even when the archive lacks complete content.
    if raw.content_truncated.is_some() {
        return None;
    }
    let content = raw.content.filter(|content| !content.is_empty())?;
    let fields = [
        raw.agent.as_str(),
        content.as_str(),
        raw.source_id.as_deref().unwrap_or_default(),
        raw.origin_kind.as_deref().unwrap_or_default(),
        raw.origin_host.as_deref().unwrap_or_default(),
    ];
    // Oversized fields are rejected whole instead of cutting a secret before
    // screening. The process output bound also covers unselected JSON fields.
    if fields.iter().any(|field| field.len() > MAX_RAW_FIELD_BYTES) {
        return None;
    }
    let source_path = screen(&raw.source_path);
    let workspace = screen(&workspace);
    if source_path.is_empty()
        || source_path.len() > MAX_PATH_BYTES
        || workspace.len() > MAX_PATH_BYTES
        || source_path.chars().any(display_control)
        || workspace.chars().any(display_control)
    {
        return None;
    }
    let mut shortened = false;
    let mut label = |value: &str| {
        let (value, was_shortened) = screened_excerpt(value, MAX_LABEL_BYTES, screen);
        shortened |= was_shortened;
        value.replace(['\n', '\t'], " ")
    };
    let agent = label(&raw.agent);
    let source_id = raw.source_id.as_deref().map(&mut label);
    let origin_kind = raw.origin_kind.as_deref().map(&mut label);
    let origin_host = raw.origin_host.as_deref().map(&mut label);
    let (snippet, snippet_shortened) = screened_excerpt(&content, MAX_SNIPPET_BYTES, screen);
    Some((
        CassHit {
            source_path,
            line_number: raw.line_number.filter(|line| *line > 0),
            agent,
            workspace,
            title: None,
            snippet,
            provenance: CassProvenance {
                source: "cass",
                mode: "lexical",
                read_only: true,
                source_id,
                origin_kind,
                origin_host,
            },
        },
        shortened || snippet_shortened,
    ))
}

#[cfg(any(unix, test))]
fn display_control(ch: char) -> bool {
    ch.is_control()
        || matches!(ch, '\u{200e}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

#[cfg(any(unix, test))]
fn screened_excerpt(raw: &str, maximum: usize, screen: Screener<'_>) -> (String, bool) {
    let screened = screen(raw);
    let mut safe: String = screened
        .chars()
        .filter(|ch| !display_control(*ch) || matches!(ch, '\n' | '\t'))
        .collect();
    if safe.len() <= maximum {
        return (safe, false);
    }
    if maximum < '…'.len_utf8() {
        safe.clear();
        return (safe, true);
    }
    let mut end = maximum - '…'.len_utf8();
    while !safe.is_char_boundary(end) {
        end -= 1;
    }
    safe.truncate(end);
    safe.push('…');
    (safe, true)
}

#[cfg(any(unix, test))]
fn hit_text_bytes(hit: &CassHit) -> usize {
    hit.source_path.len()
        + hit.workspace.len()
        + hit.agent.len()
        + hit.title.as_deref().map_or(0, str::len)
        + hit.snippet.len()
        + hit.provenance.source_id.as_deref().map_or(0, str::len)
        + hit.provenance.origin_kind.as_deref().map_or(0, str::len)
        + hit.provenance.origin_host.as_deref().map_or(0, str::len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn hit(root: &Path) -> Value {
        json!({
            "source_path": "/archive/agent-session.jsonl",
            "line_number": 42,
            "agent": "codex",
            "workspace": root,
            "title": "Earlier authentication fix",
            "content": "Keep the authentication cache scoped to the selected project.",
            "source_id": "local",
            "origin_kind": "local"
        })
    }

    fn parse(value: &Value, root: &Path, limit: usize) -> CassSearch {
        parse_output(
            &serde_json::to_vec(value).unwrap(),
            root,
            limit,
            &str::to_owned,
        )
    }

    #[test]
    fn history_recall_future_is_send_at_the_tool_boundary() {
        fn assert_send<T: Send>(_: T) {}

        let owner = AgentCx::for_request();
        assert_send(search(
            &owner,
            Path::new("/project"),
            "query",
            5,
            &str::to_owned,
        ));
    }

    #[test]
    fn robot_hits_preserve_citations_and_only_the_exact_project() {
        let root = Path::new("/projects/active");
        let mut foreign = hit(root);
        foreign["workspace"] = json!("/projects/active-other");
        foreign["content"] = json!("private foreign-project content");
        let mut child = hit(root);
        child["workspace"] = json!("/projects/active/nested");
        let mut unknown = hit(root);
        unknown["workspace"] = Value::Null;
        let result = parse(
            &json!({"hits": [foreign, hit(root), child, unknown]}),
            root,
            10,
        );
        assert_eq!(result.status, CassStatus::Completed);
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].source_path, "/archive/agent-session.jsonl");
        assert_eq!(result.hits[0].line_number, Some(42));
        assert_eq!(result.hits[0].provenance.source_id.as_deref(), Some("local"));
        assert_eq!(result.diagnostic, Some("CASS_RESULTS_FILTERED"));
        assert!(
            !serde_json::to_string(&result)
                .unwrap()
                .contains("foreign-project")
        );
        assert_eq!(
            parse(&json!({"hits": []}), root, 10).status,
            CassStatus::Completed
        );
        let mut foreign = hit(root);
        foreign["workspace"] = json!("/projects/private");
        let screen = |value: &str| value.replace("/projects/private", "/projects/active");
        let result = parse_output(
            &serde_json::to_vec(&json!({"hits": [foreign]})).unwrap(),
            root,
            10,
            &screen,
        );
        assert_eq!(result.status, CassStatus::Unavailable);
        assert_eq!(result.diagnostic, Some("CASS_SCOPE_OR_OUTPUT_MISMATCH"));
    }

    #[test]
    fn malformed_or_timed_out_output_is_never_an_empty_success() {
        let root = Path::new("/project");
        for bytes in [
            b"not json".as_slice(),
            b"{}",
            b"{\"hits\":[],\"hits\":[]}",
            b"{\"hits\":false}",
            b"{\"hits\":[],\"budget\":{}}",
        ] {
            let result = parse_output(bytes, root, 10, &str::to_owned);
            assert_eq!(result.status, CassStatus::Unavailable);
            assert!(result.hits.is_empty());
        }
        for value in [
            json!({"hits": [hit(root)], "budget": {"timed_out": true}}),
            json!({"hits": [], "_meta": {"timed_out": true}}),
            json!({"hits": [], "_timeout": {"message": "secret diagnostic"}}),
        ] {
            let result = parse(&value, root, 10);
            assert_eq!(result.status, CassStatus::Timeout);
            assert!(result.hits.is_empty());
            assert!(!serde_json::to_string(&result).unwrap().contains("secret"));
        }
        let error = parse(
            &json!({"hits": [], "error": {"message": "secret"}}),
            root,
            10,
        );
        assert_eq!(error.status, CassStatus::Unavailable);
        assert_eq!(
            parse(
                &json!({"hits": [], "_meta": {"timed_out": null, "partial_results": null}}),
                root,
                10,
            )
            .status,
            CassStatus::Completed
        );
    }

    #[test]
    fn privacy_screening_precedes_utf8_caps_and_covers_provenance() {
        let root = Path::new("/project");
        let secret = "secret-crosses-the-snippet-boundary";
        let mut raw = hit(root);
        raw["content"] = json!(format!("{}{}", "界".repeat(680), secret));
        raw["title"] = json!(secret);
        raw["source_path"] = json!(format!("/archive/{secret}.jsonl"));
        raw["agent"] = json!(secret);
        raw["origin_host"] = json!(secret);
        let screen = |value: &str| value.replace(secret, "[REDACTED]");
        let result = parse_output(
            &serde_json::to_vec(&json!({"hits": [raw]})).unwrap(),
            root,
            10,
            &screen,
        );
        assert_eq!(result.status, CassStatus::Completed);
        assert!(result.truncated);
        assert!(result.hits[0].snippet.len() <= MAX_SNIPPET_BYTES);
        assert!(result.hits[0].snippet.ends_with('…'));
        let serialized = serde_json::to_string(&result).unwrap();
        assert!(!serialized.contains("secret-"));
        assert!(serialized.contains("[REDACTED]"));
    }

    #[test]
    fn complete_content_preserves_assignment_context_lost_in_archive_excerpts() {
        let root = Path::new("/project");
        let secret = "opaqueArchivedCredential1234567";
        let mut raw = hit(root);
        raw["content"] = json!(format!("password={secret}"));
        // Neither excerpt contains enough context for the generic assignment
        // rule. Both must be ignored in favor of the complete source message.
        raw["snippet"] = json!(secret);
        raw["title"] = json!(secret);
        let result = parse_output(
            &serde_json::to_vec(&json!({"hits": [raw]})).unwrap(),
            root,
            10,
            &crate::memory::screen_secrets,
        );
        assert_eq!(result.status, CassStatus::Completed);
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].snippet, "password=[REDACTED_GENERIC_SECRET]");
        assert!(result.hits[0].title.is_none());
        assert!(!serde_json::to_string(&result).unwrap().contains(secret));
    }

    #[test]
    fn private_key_crossing_old_cli_limit_is_screened_as_complete_content() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let root = Path::new("/project");
        let canary = "PRIVATE_BODY_CANARY_1234567890";
        let content = format!(
            "before\n-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\nafter",
            canary.repeat(200)
        );
        assert!(content.len() > 4096);
        let mut raw = hit(root);
        raw["content"] = json!(&content);
        raw["snippet"] = json!(canary);
        raw["title"] = json!(canary);
        let saw_complete_content = AtomicBool::new(false);
        let screen = |value: &str| {
            if value == content.as_str() {
                saw_complete_content.store(true, Ordering::Relaxed);
            }
            crate::memory::screen_secrets(value)
        };
        let result = parse_output(
            &serde_json::to_vec(&json!({"hits": [raw]})).unwrap(),
            root,
            10,
            &screen,
        );
        assert!(saw_complete_content.load(Ordering::Relaxed));
        assert_eq!(result.status, CassStatus::Completed);
        assert_eq!(
            result.hits[0].snippet,
            "before\n[REDACTED_PRIVATE_KEY]\nafter"
        );
        assert!(!serde_json::to_string(&result).unwrap().contains(canary));
    }

    #[test]
    fn snippet_only_and_upstream_shortened_content_are_never_recalled() {
        let root = Path::new("/project");
        let secret = "opaqueArchivedCredential1234567";
        let mut snippet_only = hit(root);
        snippet_only.as_object_mut().unwrap().remove("content");
        snippet_only["snippet"] = json!(secret);
        for content in [None, Some(Value::Null), Some(json!(""))] {
            let mut raw = snippet_only.clone();
            if let Some(content) = content {
                raw["content"] = content;
            }
            let result = parse_output(
                &serde_json::to_vec(&json!({"hits": [raw]})).unwrap(),
                root,
                10,
                &crate::memory::screen_secrets,
            );
            assert_eq!(result.status, CassStatus::Unavailable);
            assert!(result.hits.is_empty());
            assert!(!serde_json::to_string(&result).unwrap().contains(secret));
        }
        // Marker shape is not part of the stable schema; its presence denotes
        // shortened content in the real CLI contract. Do not inspect fragments.
        for marker in [json!(true), json!(false), json!({"original_length": 5000})] {
            let mut raw = hit(root);
            raw["content"] = json!(secret);
            raw["content_truncated"] = marker;
            let result = parse_output(
                &serde_json::to_vec(&json!({"hits": [raw]})).unwrap(),
                root,
                10,
                &crate::memory::screen_secrets,
            );
            assert_eq!(result.status, CassStatus::Unavailable);
            assert!(result.hits.is_empty());
            assert!(!serde_json::to_string(&result).unwrap().contains(secret));
        }
    }

    #[test]
    fn output_and_result_limits_do_not_allow_unbounded_history() {
        let root = Path::new("/project");
        let hits = vec![hit(root); MAX_HITS + 3];
        let result = parse(&json!({"hits": hits}), root, usize::MAX);
        assert_eq!(result.hits.len(), MAX_HITS);
        assert!(result.truncated);
        assert!(result.hits.iter().map(hit_text_bytes).sum::<usize>() <= MAX_RESULT_BYTES);
        let mut wide = hit(root);
        wide["source_path"] = json!(format!("/{}", "p".repeat(MAX_PATH_BYTES - 1)));
        wide["content"] = json!("s".repeat(MAX_SNIPPET_BYTES));
        wide["title"] = json!("t".repeat(MAX_LABEL_BYTES));
        let result = parse(&json!({"hits": vec![wide; MAX_HITS]}), root, MAX_HITS);
        assert_eq!(result.status, CassStatus::Completed);
        assert!(!result.hits.is_empty());
        assert!(result.hits.len() < MAX_HITS);
        assert!(result.truncated);
        assert!(result.hits.iter().map(hit_text_bytes).sum::<usize>() <= MAX_RESULT_BYTES);
        let mut raw = hit(root);
        raw["content"] = json!("x".repeat(MAX_RAW_FIELD_BYTES + 1));
        let result = parse(&json!({"hits": [raw]}), root, 10);
        assert_eq!(result.status, CassStatus::Unavailable);
        let oversized = vec![b' '; MAX_OUTPUT_BYTES + 1];
        assert_eq!(
            parse_output(&oversized, root, 10, &str::to_owned).diagnostic,
            Some("CASS_OUTPUT_LIMIT")
        );
    }

    #[cfg(unix)]
    mod process_tests {
        use super::*;
        use std::fs;
        use std::os::unix::fs::PermissionsExt as _;

        fn fixture(script: &str) -> (tempfile::TempDir, std::path::PathBuf) {
            let directory = tempfile::tempdir().unwrap();
            let program = directory.path().join("cass-fixture");
            fs::write(&program, format!("#!/bin/sh\n{script}\n")).unwrap();
            fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
            (directory, program)
        }

        fn run<T>(future: impl std::future::Future<Output = T>) -> T {
            asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .unwrap()
                .block_on(future)
        }

        #[test]
        fn real_subprocess_receives_scoped_read_only_flags_and_literal_query() {
            let (directory, program) =
                fixture("printf '%s\\n' \"$@\" > argv.txt\ncat result.json");
            let root = directory.path().canonicalize().unwrap();
            fs::write(
                root.join("result.json"),
                json!({"hits": [hit(&root)]}).to_string(),
            )
            .unwrap();
            let query = "--refresh ; $(touch SHOULD_NOT_EXIST)";
            let result = run(search_with_program(
                &AgentCx::for_request(),
                directory.path(),
                query,
                5,
                &str::to_owned,
                program.as_os_str(),
                Duration::from_secs(5),
            ));
            assert_eq!(result.status, CassStatus::Completed);
            assert_eq!(result.hits.len(), 1);
            let arguments = fs::read_to_string(root.join("argv.txt")).unwrap();
            let arguments: Vec<&str> = arguments.lines().collect();
            assert_eq!(arguments[0], "search");
            assert!(arguments.contains(&"--robot"));
            assert!(arguments.contains(&"--no-maintenance"));
            assert!(arguments.contains(&"--no-daemon"));
            assert!(!arguments.contains(&"--max-content-length"));
            assert!(!arguments.contains(&"--max-tokens"));
            let fields = arguments
                .windows(2)
                .find(|pair| pair[0] == "--fields")
                .unwrap()[1];
            assert!(fields.split(',').any(|field| field == "content"));
            assert!(!fields.split(',').any(|field| matches!(field, "snippet" | "title")));
            assert!(
                arguments
                    .windows(2)
                    .any(|pair| pair == ["--mode", "lexical"])
            );
            assert!(
                arguments
                    .windows(2)
                    .any(|pair| pair == ["--workspace", root.to_str().unwrap()])
            );
            assert_eq!(&arguments[arguments.len() - 2..], &["--", query]);
            assert!(!root.join("SHOULD_NOT_EXIST").exists());
        }

        #[test]
        fn missing_binary_and_failed_read_only_search_have_safe_diagnostics() {
            let (directory, program) = fixture(
                "printf '%s' '{\"error\":{\"kind\":\"maintenance-required\",\"message\":\"secret stderr\"}}' >&2\nexit 7",
            );
            let owner = AgentCx::for_request();
            let result = run(search_with_program(
                &owner,
                directory.path(),
                "query",
                10,
                &str::to_owned,
                program.as_os_str(),
                Duration::from_secs(5),
            ));
            assert_eq!(result.status, CassStatus::Unavailable);
            assert_eq!(result.diagnostic, Some("CASS_MAINTENANCE_REQUIRED"));
            assert!(!serde_json::to_string(&result).unwrap().contains("secret"));
            let missing = directory.path().join("missing-cass");
            let result = run(search_with_program(
                &owner,
                directory.path(),
                "query",
                10,
                &str::to_owned,
                missing.as_os_str(),
                Duration::from_secs(5),
            ));
            assert_eq!(result.diagnostic, Some("CASS_NOT_INSTALLED"));
        }

        #[test]
        fn process_timeout_output_overflow_and_cancellation_do_not_return_partial_hits() {
            let (directory, program) = fixture("exec sleep 30");
            let owner = AgentCx::for_request();
            let result = run(search_with_program(
                &owner,
                directory.path(),
                "query",
                10,
                &str::to_owned,
                program.as_os_str(),
                Duration::ZERO,
            ));
            assert_eq!(result.status, CassStatus::Timeout);
            assert!(result.hits.is_empty());

            let (large_directory, large_program) = fixture("exec head -c 262145 /dev/zero");
            let result = run(search_with_program(
                &owner,
                large_directory.path(),
                "query",
                10,
                &str::to_owned,
                large_program.as_os_str(),
                Duration::from_secs(5),
            ));
            assert_eq!(result.status, CassStatus::Unavailable);
            assert_eq!(result.diagnostic, Some("CASS_CAPTURE_FAILED"));
            assert!(result.hits.is_empty());

            run(async {
                let owner = AgentCx::for_current_or_request();
                let mut search = Box::pin(search_with_program(
                    &owner,
                    directory.path(),
                    "query",
                    10,
                    &str::to_owned,
                    program.as_os_str(),
                    Duration::from_secs(5),
                ));
                assert!(futures::poll!(&mut search).is_pending());
                owner.cancel_with(
                    asupersync::types::CancelKind::User,
                    Some("cancel history recall"),
                );
                let result = search.await;
                assert_eq!(result.status, CassStatus::Cancelled);
                assert!(result.hits.is_empty());
            });
        }

        #[test]
        fn restricted_owner_cannot_dispatch_and_unavailable_workspace_never_broadens_scope() {
            let (directory, program) = fixture("touch DISPATCHED\nprintf '%s' '{\"hits\":[]}'");
            let restricted = asupersync::Cx::for_request().restrict::<asupersync::cx::cap::None>();
            let owner = {
                let _guard = restricted.set_current_restricted();
                AgentCx::for_current_or_request()
            };
            let result = run(search_with_program(
                &owner,
                directory.path(),
                "query",
                10,
                &str::to_owned,
                program.as_os_str(),
                Duration::from_secs(5),
            ));
            assert_eq!(result.diagnostic, Some("CASS_CAPABILITY_UNAVAILABLE"));
            let result = run(search_with_program(
                &AgentCx::for_request(),
                &directory.path().join("missing-project"),
                "query",
                10,
                &str::to_owned,
                program.as_os_str(),
                Duration::from_secs(5),
            ));
            assert_eq!(result.diagnostic, Some("CASS_WORKSPACE_UNAVAILABLE"));
            assert!(!directory.path().join("DISPATCHED").exists());
        }
    }
}
