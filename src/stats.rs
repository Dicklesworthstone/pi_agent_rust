//! Local usage statistics over session files (`pi stats`, bd-cv653.7.7).
//!
//! Aggregates tokens/cost/tool-call/compaction counts by streaming session
//! JSONL line-by-line — bounded memory regardless of history size. All data
//! stays local: this module performs no network I/O (enforced by
//! [`tests::no_network_surface`]).
//!
//! JSON output conforms to `pi.stats.v1`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde::Serialize;

/// Schema tag for the JSON renderer.
pub const STATS_SCHEMA: &str = "pi.stats.v1";

/// Filters applied while aggregating.
///
/// `since`/`until` compare against the RFC 3339 entry timestamps
/// lexicographically (identical formatting across Pi-written sessions), so
/// day-level windows work without parsing.
#[derive(Debug, Clone, Default)]
pub struct StatsFilter {
    pub since: Option<String>,
    pub until: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
}

impl StatsFilter {
    fn admits_timestamp(&self, ts: Option<&str>) -> bool {
        let Some(ts) = ts else {
            return self.since.is_none() && self.until.is_none();
        };
        if let Some(since) = &self.since
            && ts < since.as_str()
        {
            return false;
        }
        if let Some(until) = &self.until {
            // A day-prefix bound (`--until 2026-08-21`) is inclusive of that
            // whole day: compare only the prefix so `2026-08-21T10:00` is not
            // lexicographically "greater" than its own day.
            let ts_prefix = ts.get(..until.len()).unwrap_or(ts);
            if ts_prefix > until.as_str() {
                return false;
            }
        }
        true
    }

    fn admits_model(&self, provider: Option<&str>, model: Option<&str>) -> bool {
        if let Some(want) = &self.provider
            && provider != Some(want.as_str())
        {
            return false;
        }
        if let Some(want) = &self.model
            && model != Some(want.as_str())
        {
            return false;
        }
        true
    }
}

/// Token totals in a bucket.
#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
pub struct TokenTotals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub total: u64,
}

/// Cost totals in dollars.
#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq)]
pub struct CostTotals {
    pub total: f64,
}

const fn add_tokens(totals: &mut TokenTotals, t: &TokenTotals) {
    totals.input += t.input;
    totals.output += t.output;
    totals.cache_read += t.cache_read;
    totals.cache_write += t.cache_write;
    totals.total += t.total;
}

/// Aggregate report over a set of session files.
#[derive(Debug, Clone, Default, Serialize)]
pub struct StatsReport {
    pub schema: &'static str,
    /// Number of session files contributing at least one admitted entry.
    pub sessions: u64,
    /// Admitted assistant messages.
    pub messages: u64,
    pub tokens: TokenTotals,
    pub cost: CostTotals,
    pub compactions: u64,
    pub tool_calls_total: u64,
    pub by_provider_model: Vec<ProviderModelRow>,
    pub by_day: Vec<DayRow>,
    pub tool_calls: Vec<ToolCallRow>,
}

/// Per provider/model aggregate row.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderModelRow {
    pub provider: String,
    pub model: String,
    pub messages: u64,
    pub tokens: TokenTotals,
    pub cost: CostTotals,
}

/// Per-day aggregate row (UTC day from the entry timestamp prefix).
#[derive(Debug, Clone, Serialize)]
pub struct DayRow {
    pub day: String,
    pub messages: u64,
    pub tokens: TokenTotals,
    pub cost: CostTotals,
}

/// Tool-call frequency row.
#[derive(Debug, Clone, Serialize)]
pub struct ToolCallRow {
    pub tool: String,
    pub count: u64,
}

#[derive(serde::Deserialize)]
struct LineProbe {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    timestamp: Option<String>,
    /// Message payload: session lines nest role/provider/model/usage under
    /// a `"message"` object (see `session::MessageEntry`), with camelCase
    /// role tags (`"toolResult"`).
    #[serde(default)]
    message: Option<MessageProbe>,
}

#[derive(serde::Deserialize, Default)]
struct MessageProbe {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: Option<UsageProbe>,
    #[serde(default, rename = "toolName")]
    tool_name: Option<String>,
}

#[derive(serde::Deserialize, Default)]
struct UsageProbe {
    #[serde(default)]
    input: u64,
    #[serde(default)]
    output: u64,
    #[serde(default, rename = "cacheRead")]
    cache_read: u64,
    #[serde(default, rename = "cacheWrite")]
    cache_write: u64,
    #[serde(default, rename = "totalTokens")]
    total_tokens: u64,
    #[serde(default)]
    cost: Option<CostProbe>,
}

#[derive(serde::Deserialize)]
struct CostProbe {
    #[serde(default)]
    total: f64,
}

/// Model pricing fallback ($/MTok in, out) applied only when a session's
/// recorded cost is zero but tokens exist (older sessions). Versioned
/// in-tree; longest-prefix match wins.
const PRICING: &[(&str, f64, f64)] = &[
    ("claude-opus-4", 15.0, 75.0),
    ("claude-opus-3", 15.0, 75.0),
    ("claude-sonnet-4", 3.0, 15.0),
    ("claude-sonnet-3", 3.0, 15.0),
    ("claude-haiku", 0.80, 4.0),
    ("gpt-4o-mini", 0.15, 0.60),
    ("gpt-4o", 2.50, 10.0),
    ("gpt-4.1-mini", 0.40, 1.60),
    ("gpt-4.1", 2.00, 8.00),
    ("o3-mini", 1.10, 4.40),
    ("o3", 2.00, 8.00),
];

#[allow(clippy::cast_precision_loss)] // token counts are far below 2^52
fn price_fallback(model: &str, tokens_in: u64, tokens_out: u64) -> f64 {
    let mut best: Option<(&&str, f64, f64)> = None;
    for (prefix, pin, pout) in PRICING {
        if model.starts_with(prefix) && best.is_none_or(|(b, _, _)| prefix.len() > b.len()) {
            best = Some((prefix, *pin, *pout));
        }
    }
    let Some((_, pin, pout)) = best else {
        return 0.0;
    };
    (tokens_in as f64 / 1_000_000.0).mul_add(pin, (tokens_out as f64 / 1_000_000.0) * pout)
}

/// Internal accumulator mirroring [`StatsReport`] buckets.
#[derive(Default)]
struct Accumulator {
    sessions: u64,
    messages: u64,
    tokens: TokenTotals,
    cost: f64,
    compactions: u64,
    pm_messages: BTreeMap<(String, String), u64>,
    pm_tokens: BTreeMap<(String, String), TokenTotals>,
    pm_cost: BTreeMap<(String, String), f64>,
    day_messages: BTreeMap<String, u64>,
    day_tokens: BTreeMap<String, TokenTotals>,
    day_cost: BTreeMap<String, f64>,
    tool_calls: BTreeMap<String, u64>,
}

impl Accumulator {
    fn ingest_line(&mut self, filter: &StatsFilter, line: &str, session_counted: &mut bool) {
        let Ok(probe) = serde_json::from_str::<LineProbe>(line) else {
            return;
        };
        if !filter.admits_timestamp(probe.timestamp.as_deref()) {
            return;
        }
        match probe.kind.as_str() {
            "compaction" => self.compactions += 1,
            "message" => {
                let msg = probe.message.unwrap_or_default();
                if msg.role.as_deref() == Some("toolResult")
                    && let Some(tool) = msg.tool_name.as_deref()
                {
                    *self.tool_calls.entry(tool.to_string()).or_insert(0) += 1;
                }
                if msg.role.as_deref() == Some("assistant") {
                    if !filter.admits_model(msg.provider.as_deref(), msg.model.as_deref()) {
                        return;
                    }
                    if !*session_counted {
                        self.sessions += 1;
                        *session_counted = true;
                    }
                    self.messages += 1;

                    let usage = msg.usage.unwrap_or_default();
                    let tokens = TokenTotals {
                        input: usage.input,
                        output: usage.output,
                        cache_read: usage.cache_read,
                        cache_write: usage.cache_write,
                        total: usage.total_tokens,
                    };
                    let recorded = usage.cost.as_ref().map_or(0.0, |c| c.total);
                    let cost = if recorded > 0.0 {
                        recorded
                    } else {
                        price_fallback(
                            msg.model.as_deref().unwrap_or_default(),
                            tokens.input,
                            tokens.output,
                        )
                    };

                    add_tokens(&mut self.tokens, &tokens);
                    self.cost += cost;

                    let key = (
                        msg.provider.unwrap_or_else(|| "unknown".into()),
                        msg.model.unwrap_or_else(|| "unknown".into()),
                    );
                    *self.pm_messages.entry(key.clone()).or_insert(0) += 1;
                    add_tokens(self.pm_tokens.entry(key.clone()).or_default(), &tokens);
                    *self.pm_cost.entry(key).or_default() += cost;

                    let day = probe
                        .timestamp
                        .as_deref()
                        .and_then(|ts| ts.get(..10))
                        .unwrap_or_default()
                        .to_string();
                    *self.day_messages.entry(day.clone()).or_insert(0) += 1;
                    add_tokens(self.day_tokens.entry(day.clone()).or_default(), &tokens);
                    *self.day_cost.entry(day).or_insert(0.0) += cost;
                }
            }
            _ => {}
        }
    }
}

/// Aggregate one session JSONL file into `acc`.
fn accumulate_file(acc: &mut Accumulator, path: &Path, filter: &StatsFilter) {
    let Ok(file) = std::fs::File::open(path) else {
        return;
    };
    let reader = BufReader::new(file);
    let mut counted = false;
    for line in reader.lines() {
        // A single bad line (invalid UTF-8, transient IO error) must not
        // discard the rest of the session file.
        let Ok(line) = line else { continue };
        acc.ingest_line(filter, &line, &mut counted);
    }
}

/// Collect session JSONL paths under `sessions_dir` (per-project encoded-cwd
/// subdirectories), optionally filtered by a project-name substring matched
/// against the directory name.
pub fn collect_session_files(sessions_dir: &Path, project: Option<&str>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(project_dirs) = std::fs::read_dir(sessions_dir) else {
        return out;
    };
    for dir in project_dirs.flatten() {
        let dir_path = dir.path();
        if !dir_path.is_dir() {
            continue;
        }
        if let Some(want) = project {
            let name = dir.file_name().to_string_lossy().to_lowercase();
            if !name.contains(&want.to_lowercase()) {
                continue;
            }
        }
        let Ok(files) = std::fs::read_dir(&dir_path) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().is_some_and(|ext| ext == "jsonl") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// Aggregate the given session files into a report.
pub fn aggregate(files: &[PathBuf], filter: &StatsFilter) -> StatsReport {
    let mut acc = Accumulator::default();
    for path in files {
        accumulate_file(&mut acc, path, filter);
    }
    finish(acc)
}

fn finish(acc: Accumulator) -> StatsReport {
    let tool_calls_total: u64 = acc.tool_calls.values().sum();
    let mut by_provider_model: Vec<ProviderModelRow> = acc
        .pm_messages
        .iter()
        .map(|((provider, model), messages)| ProviderModelRow {
            provider: provider.clone(),
            model: model.clone(),
            messages: *messages,
            tokens: acc
                .pm_tokens
                .get(&(provider.clone(), model.clone()))
                .copied()
                .unwrap_or_default(),
            cost: CostTotals {
                total: acc
                    .pm_cost
                    .get(&(provider.clone(), model.clone()))
                    .copied()
                    .unwrap_or(0.0),
            },
        })
        .collect();
    by_provider_model.sort_by(|a, b| {
        b.tokens
            .total
            .cmp(&a.tokens.total)
            .then_with(|| a.provider.cmp(&b.provider))
            .then_with(|| a.model.cmp(&b.model))
    });

    let mut by_day: Vec<DayRow> = acc
        .day_messages
        .iter()
        .map(|(day, messages)| DayRow {
            day: day.clone(),
            messages: *messages,
            tokens: acc.day_tokens.get(day).copied().unwrap_or_default(),
            cost: CostTotals {
                total: acc.day_cost.get(day).copied().unwrap_or(0.0),
            },
        })
        .collect();
    by_day.sort_by(|a, b| a.day.cmp(&b.day));

    let mut tool_calls: Vec<ToolCallRow> = acc
        .tool_calls
        .into_iter()
        .map(|(tool, count)| ToolCallRow { tool, count })
        .collect();
    tool_calls.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.tool.cmp(&b.tool)));

    StatsReport {
        schema: STATS_SCHEMA,
        sessions: acc.sessions,
        messages: acc.messages,
        tokens: acc.tokens,
        cost: CostTotals { total: acc.cost },
        compactions: acc.compactions,
        tool_calls_total,
        by_provider_model,
        by_day,
        tool_calls,
    }
}

/// Render the report as human-readable text.
#[must_use]
pub fn render_text(report: &StatsReport) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Sessions: {}  Messages: {}  Compactions: {}",
        report.sessions, report.messages, report.compactions
    );
    let _ = writeln!(
        out,
        "Tokens: in {}  out {}  cache-r {}  cache-w {}  total {}",
        report.tokens.input,
        report.tokens.output,
        report.tokens.cache_read,
        report.tokens.cache_write,
        report.tokens.total
    );
    let _ = writeln!(out, "Cost: ${:.4}", report.cost.total);
    if !report.by_provider_model.is_empty() {
        out.push_str("\nBy provider/model:\n");
        for row in &report.by_provider_model {
            let _ = writeln!(
                out,
                "  {:<12} {:<28} msgs {:<6} tok {}  ${:.4}",
                row.provider, row.model, row.messages, row.tokens.total, row.cost.total
            );
        }
    }
    if !report.by_day.is_empty() {
        out.push_str("\nBy day:\n");
        for row in &report.by_day {
            let _ = writeln!(
                out,
                "  {}  msgs {:<6} tok {}  ${:.4}",
                row.day, row.messages, row.tokens.total, row.cost.total
            );
        }
    }
    if !report.tool_calls.is_empty() {
        out.push_str("\nTop tools:\n");
        for row in report.tool_calls.iter().take(10) {
            let _ = writeln!(out, "  {:<20} {}", row.tool, row.count);
        }
    }
    out
}

/// Render the report as GitHub-flavored markdown.
#[must_use]
pub fn render_markdown(report: &StatsReport) -> String {
    let mut out = String::from("# pi stats\n\n");
    let _ = writeln!(
        out,
        "- Sessions: {}\n- Messages: {}\n- Tokens (in/out/total): {} / {} / {}\n- Cost: ${:.4}\n- Compactions: {}",
        report.sessions,
        report.messages,
        report.tokens.input,
        report.tokens.output,
        report.tokens.total,
        report.cost.total,
        report.compactions
    );
    if !report.by_provider_model.is_empty() {
        out.push_str("\n| provider | model | messages | tokens | cost |\n|---|---|---|---|---|\n");
        for row in &report.by_provider_model {
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} | ${:.4} |",
                row.provider, row.model, row.messages, row.tokens.total, row.cost.total
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant_line(
        provider: &str,
        model: &str,
        ts: &str,
        tin: u64,
        tout: u64,
        cost: f64,
    ) -> String {
        // Mirrors the real writer shape (session::MessageEntry): payload
        // fields nest under "message" with camelCase role tags.
        format!(
            r#"{{"type":"message","id":"m1","parentId":null,"timestamp":"{ts}","message":{{"role":"assistant","content":[],"api":"api","provider":"{provider}","model":"{model}","usage":{{"input":{tin},"output":{tout},"cacheRead":0,"cacheWrite":0,"totalTokens":{},"cost":{{"input":{cost},"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":{cost}}}}}}}}}"#,
            tin + tout
        )
    }

    fn write_session(dir: &Path, name: &str, lines: &[String]) -> PathBuf {
        std::fs::create_dir_all(dir).expect("mkdir");
        let path = dir.join(name);
        std::fs::write(&path, lines.join("\n")).expect("write session");
        path
    }

    #[test]
    fn aggregates_match_hand_computed_values() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("proj-a");
        let f1 = write_session(
            &proj,
            "s1.jsonl",
            &[
                assistant_line("anthropic", "claude-sonnet-4-5", "2026-08-20T10:00:00Z", 100, 200, 0.01),
                assistant_line("openai", "gpt-4o", "2026-08-21T11:00:00Z", 300, 400, 0.02),
                r#"{"type":"message","timestamp":"2026-08-21T11:05:00Z","message":{"role":"toolResult","toolName":"read","content":[]}}"#.into(),
                r#"{"type":"compaction","summary":"s","firstKeptEntryId":"x","tokensBefore":9}"#.into(),
            ],
        );
        let report = aggregate(&[f1], &StatsFilter::default());

        assert_eq!(report.sessions, 1);
        assert_eq!(report.messages, 2);
        assert_eq!(report.tokens.input, 400);
        assert_eq!(report.tokens.output, 600);
        assert_eq!(report.tokens.total, 1000);
        assert!((report.cost.total - 0.03).abs() < 1e-9);
        assert_eq!(report.compactions, 1);
        assert_eq!(report.tool_calls_total, 1);
        assert_eq!(report.by_provider_model.len(), 2);
        assert_eq!(report.by_day.len(), 2);
        let day1 = report
            .by_day
            .iter()
            .find(|d| d.day == "2026-08-20")
            .unwrap();
        assert_eq!(day1.tokens.input, 100);
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains(STATS_SCHEMA), "schema tag present");
    }

    #[test]
    fn probe_matches_real_session_writer_shape() {
        // Guard against writer/reader schema drift: build the line from the
        // REAL session serializer (SessionMessage), not a hand-written
        // fixture. This is exactly the drift that once made every real
        // session aggregate to zero.
        let assistant = crate::session::SessionMessage::Assistant {
            message: crate::model::AssistantMessage {
                content: Vec::new(),
                api: "api".into(),
                provider: "anthropic".into(),
                model: "claude-sonnet-4-5".into(),
                usage: crate::model::Usage {
                    input: 7,
                    output: 11,
                    cache_read: 0,
                    cache_write: 0,
                    total_tokens: 18,
                    cost: crate::model::Cost {
                        total: 0.5,
                        ..Default::default()
                    },
                },
                stop_reason: crate::model::StopReason::Stop,
                stop_details: None,
                error_message: None,
                timestamp: 0,
            },
        };
        let tool_result = crate::session::SessionMessage::ToolResult {
            tool_call_id: "t1".into(),
            tool_name: "read".into(),
            content: Vec::new(),
            details: None,
            is_error: false,
            timestamp: None,
        };
        let wrap = |msg: &crate::session::SessionMessage| {
            format!(
                r#"{{"type":"message","id":"m","timestamp":"2026-08-20T10:00:00.000Z","message":{}}}"#,
                serde_json::to_string(msg).unwrap()
            )
        };
        let tmp = tempfile::tempdir().unwrap();
        let f = write_session(
            &tmp.path().join("proj"),
            "s.jsonl",
            &[wrap(&assistant), wrap(&tool_result)],
        );
        let report = aggregate(&[f], &StatsFilter::default());
        assert_eq!(report.messages, 1, "assistant message admitted");
        assert_eq!(report.tokens.input, 7);
        assert_eq!(report.tokens.output, 11);
        assert!((report.cost.total - 0.5).abs() < 1e-9);
        assert_eq!(report.tool_calls_total, 1, "toolResult admitted");
    }

    #[test]
    fn filters_compose_day_and_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let f = write_session(
            tmp.path(),
            "s.jsonl",
            &[
                assistant_line(
                    "anthropic",
                    "claude-sonnet-4-5",
                    "2026-08-20T10:00:00Z",
                    10,
                    20,
                    0.001,
                ),
                assistant_line("openai", "gpt-4o", "2026-08-20T11:00:00Z", 30, 40, 0.002),
                assistant_line(
                    "anthropic",
                    "claude-sonnet-4-5",
                    "2026-08-21T09:00:00Z",
                    50,
                    60,
                    0.003,
                ),
            ],
        );
        let filter = StatsFilter {
            since: Some("2026-08-20".into()),
            until: Some("2026-08-20T23:59:59Z".into()),
            provider: Some("openai".into()),
            model: None,
        };
        let report = aggregate(&[f], &filter);
        assert_eq!(report.messages, 1);
        assert_eq!(report.tokens.input, 30);
        assert_eq!(report.tokens.output, 40);
    }

    #[test]
    fn until_day_prefix_is_inclusive() {
        let filter = StatsFilter {
            until: Some("2026-08-21".into()),
            ..Default::default()
        };
        assert!(filter.admits_timestamp(Some("2026-08-21T10:00:00Z")));
        assert!(filter.admits_timestamp(Some("2026-08-20T00:00:00Z")));
        assert!(!filter.admits_timestamp(Some("2026-08-22T00:00:00Z")));
    }

    #[test]
    fn pricing_fallback_applies_when_recorded_cost_zero() {
        let tmp = tempfile::tempdir().unwrap();
        // 1M in / 1M out on sonnet-4 => $3 + $15 = $18 via fallback.
        let line = assistant_line(
            "anthropic",
            "claude-sonnet-4-5",
            "2026-08-20T10:00:00Z",
            1_000_000,
            1_000_000,
            0.0,
        );
        let f = write_session(tmp.path(), "s.jsonl", &[line]);
        let report = aggregate(&[f], &StatsFilter::default());
        assert!(
            (report.cost.total - 18.0).abs() < 1e-6,
            "{}",
            report.cost.total
        );
    }

    #[test]
    fn collect_filters_by_project_substring() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("-users-jemanuel-proj-alpha");
        let b = tmp.path().join("-users-jemanuel-proj-beta");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("s.jsonl"), "").unwrap();
        std::fs::write(b.join("s.jsonl"), "").unwrap();

        let all = collect_session_files(tmp.path(), None);
        assert_eq!(all.len(), 2);
        let alpha = collect_session_files(tmp.path(), Some("alpha"));
        assert_eq!(alpha.len(), 1);
        assert!(alpha[0].starts_with(&a));
    }

    #[test]
    fn no_network_surface_static_audit() {
        // Privacy acceptance (bd-cv653.7.7 #4): the stats code path must not
        // construct any network client or reference the provider usage
        // readers. Static scan of this module's own source — the non-test
        // portion only, so this test's banned-word list cannot self-match.
        let source = include_str!("stats.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap_or_default();
        for banned in [
            "reqwest",
            "HttpClient",
            "http::client",
            "TcpStream",
            "usage::readers_from_auth",
        ] {
            assert!(
                !source.contains(banned),
                "stats module must not reference network surface: {banned}"
            );
        }
    }

    #[test]
    fn large_tree_aggregation_stays_bounded() {
        // Perf budget (acceptance #2, CI-scaled): 500 synthetic sessions x
        // 20 lines aggregate well under the 5s full-scale budget.
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("perf");
        let mut paths = Vec::new();
        for s in 0..500 {
            let mut lines = Vec::with_capacity(20);
            for m in 0..20 {
                lines.push(assistant_line(
                    "anthropic",
                    "claude-sonnet-4-5",
                    "2026-08-20T10:00:00Z",
                    10,
                    20,
                    0.0001,
                ));
            }
            paths.push(write_session(&proj, &format!("s{s}.jsonl"), &lines));
        }
        let started = std::time::Instant::now();
        let report = aggregate(&paths, &StatsFilter::default());
        let elapsed = started.elapsed();
        assert_eq!(report.messages, 10_000);
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "aggregation must stay under budget, took {elapsed:?}"
        );
    }
}
