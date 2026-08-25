//! Per-model tool-call dialect converters (bd-cv653.7.8).
//!
//! Weak models on OpenAI-compatible endpoints sometimes emit tool calls as
//! TEXT (fenced JSON, XML-ish tags, or a bare JSON object) instead of the
//! structured `tool_calls` field. The dialect layer recognizes and repairs
//! those emissions into real tool calls so the turn continues instead of
//! ending on prose.
//!
//! Guards against false positives (the whole point of the layer):
//! - Extraction only runs when the model catalog explicitly selects the
//!   repairable Xmlish dialect; Native and Harmony never see it.
//! - A candidate's `name` must be a currently-registered tool.
//! - `arguments` must be a JSON object.
//! - Bare-JSON extraction only fires when the ENTIRE trimmed content is the
//!   candidate object (prose can never half-match).
//! - At most one repair per assistant message (callers bound the turn).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Tool-call dialect families (v1).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dialect {
    /// Provider-native structured tool calls (no repair).
    #[default]
    Native,
    /// XML-ish / fenced-JSON text emissions (qwen3, kimi-k2, glm-4.5,
    /// minimax, deepseek-reasoner families on OpenAI-compatible transports).
    Xmlish,
    /// GPT-5 harmony channel conventions (no text repair; prompt notes only).
    Harmony,
}

impl Dialect {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Xmlish => "xmlish",
            Self::Harmony => "harmony",
        }
    }
}

/// Heuristically classify a model for offline benchmarking and migration suggestions (bd-cv653.7.8).
///
/// Runtime response repair does not call this: only an explicit model-catalog `dialect` opt-in
/// can enable Xmlish repair. Callers must never treat Harmony as a text-repair dialect.
#[must_use]
pub fn dialect_for_model(provider: &str, model_id: &str) -> Dialect {
    const XMLISH_MARKERS: &[&str] = &[
        "qwen3",
        "kimi-k2",
        "glm-4",
        "glm4",
        "minimax",
        "deepseek-r1",
        "deepseek-reasoner",
        "hermes",
        "nous",
        "dolphin",
    ];
    let id = model_id.to_ascii_lowercase();
    let provider = provider.to_ascii_lowercase();
    if provider.contains("openai")
        && (id.starts_with("gpt-5")
            || id.starts_with("o1")
            || id.starts_with("o3")
            || id.starts_with("o4"))
    {
        return Dialect::Harmony;
    }
    if XMLISH_MARKERS.iter().any(|marker| id.contains(marker)) {
        return Dialect::Xmlish;
    }
    Dialect::Native
}

/// A tool call extracted from text.
#[derive(Debug, Clone)]
pub struct RepairCandidate {
    pub name: String,
    pub arguments: Value,
    /// Byte span in the source text (for stripping).
    pub start: usize,
    pub end: usize,
}

/// One recorded repair (the ledger entry).
#[derive(Debug, Clone)]
pub struct RepairEntry {
    pub tool: String,
    pub stripped_bytes: usize,
    pub remaining_text_bytes: usize,
}

/// In-memory repair ledger for one session (bd-cv653.7.8).
#[derive(Debug, Default)]
pub struct RepairLedger {
    pub entries: Vec<RepairEntry>,
}

impl RepairLedger {
    pub fn record(&mut self, tool: &str, stripped: usize, remaining: usize) {
        self.entries.push(RepairEntry {
            tool: tool.to_string(),
            stripped_bytes: stripped,
            remaining_text_bytes: remaining,
        });
    }
}

/// Extract tool-call candidates from assistant text. `is_known_tool` gates
/// names against the live registry. Conservative by construction.
#[must_use]
pub fn extract_text_tool_calls(
    text: &str,
    is_known_tool: &dyn Fn(&str) -> bool,
) -> Vec<RepairCandidate> {
    let mut out = Vec::new();
    extract_bare_json(text, is_known_tool, &mut out);
    if out.is_empty() {
        extract_fenced(text, is_known_tool, &mut out);
    }
    if out.is_empty() {
        extract_xmlish(text, is_known_tool, &mut out);
    }
    out
}

fn parse_candidate_payload(
    payload: &str,
    is_known_tool: &dyn Fn(&str) -> bool,
) -> Option<(String, Value)> {
    let value: Value = serde_json::from_str(payload.trim()).ok()?;
    let name = value.get("name").and_then(Value::as_str)?;
    if !is_known_tool(name) {
        return None;
    }
    let arguments = value
        .get("arguments")
        .or_else(|| value.get("args"))
        .or_else(|| value.get("parameters"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    if !arguments.is_object() {
        return None;
    }
    Some((name.to_string(), arguments))
}

/// Bare JSON: only when the whole trimmed content is one candidate object.
fn extract_bare_json(
    text: &str,
    is_known_tool: &dyn Fn(&str) -> bool,
    out: &mut Vec<RepairCandidate>,
) {
    let trimmed = text.trim();
    if !trimmed.starts_with('{') || !trimmed.ends_with('}') {
        return;
    }
    let start = text.find('{').unwrap_or(0);
    let end = text.rfind('}').map_or(text.len(), |i| i + 1);
    if let Some((name, arguments)) = parse_candidate_payload(&text[start..end], is_known_tool) {
        out.push(RepairCandidate {
            name,
            arguments,
            start,
            end,
        });
    }
}

/// Fenced blocks: ```json / ```tool_call / ```tool blocks whose payload is a
/// single candidate object.
fn extract_fenced(
    text: &str,
    is_known_tool: &dyn Fn(&str) -> bool,
    out: &mut Vec<RepairCandidate>,
) {
    let mut cursor = 0;
    while let Some(open) = text[cursor..].find("```") {
        let fence_start = cursor + open;
        let after_open = &text[fence_start + 3..];
        let Some(newline) = after_open.find('\n') else {
            break;
        };
        let lang = after_open[..newline].trim();
        let content_start = fence_start + 3 + newline + 1;
        let Some(close_rel) = text[content_start..].find("```") else {
            break;
        };
        let content_end = content_start + close_rel;
        let payload = &text[content_start..content_end];
        if matches!(
            lang,
            "json" | "tool_call" | "tool" | "tool_use" | "toolcall"
        ) && let Some((name, arguments)) = parse_candidate_payload(payload, is_known_tool)
        {
            out.push(RepairCandidate {
                name,
                arguments,
                start: fence_start,
                end: content_end + 3,
            });
            break; // one repair per message
        }
        cursor = content_end + 3;
    }
}

/// XML-ish emissions: `<tool_call>{json}</tool_call>` and
/// `<tool name="x">{json-args}</tool>`.
fn extract_xmlish(
    text: &str,
    is_known_tool: &dyn Fn(&str) -> bool,
    out: &mut Vec<RepairCandidate>,
) {
    for (open_tag, close_tag) in [
        ("<tool_call>", "</tool_call>"),
        ("<tool_use>", "</tool_use>"),
    ] {
        if let Some(open) = text.find(open_tag)
            && let Some(close_rel) = text[open + open_tag.len()..].find(close_tag)
        {
            let start = open + open_tag.len();
            let end = start + close_rel;
            if let Some((name, arguments)) =
                parse_candidate_payload(&text[start..end], is_known_tool)
            {
                out.push(RepairCandidate {
                    name,
                    arguments,
                    start: open,
                    end: end + close_tag.len(),
                });
                return;
            }
        }
    }
    // <tool name="x">{args}</tool>
    if let Some(open) = text.find("<tool name=\"")
        && let Some(name_end_rel) = text[open + 12..].find('"')
    {
        let name = &text[open + 12..open + 12 + name_end_rel];
        if is_known_tool(name)
            && let Some(tag_end_rel) = text[open + 12 + name_end_rel..].find('>')
        {
            let content_start = open + 12 + name_end_rel + tag_end_rel + 1;
            if let Some(close_rel) = text[content_start..].find("</tool>") {
                let payload = text[content_start..content_start + close_rel].trim();
                let arguments: Option<Value> = serde_json::from_str(payload).ok();
                if let Some(arguments) = arguments
                    && arguments.is_object()
                {
                    out.push(RepairCandidate {
                        name: name.to_string(),
                        arguments,
                        start: open,
                        end: content_start + close_rel + "</tool>".len(),
                    });
                }
            }
        }
    }
}

/// Strip extracted spans from the text (descending order), returning the
/// remaining prose.
#[must_use]
pub fn strip_candidates(text: &str, candidates: &[RepairCandidate]) -> String {
    let mut out = text.to_string();
    let mut spans: Vec<(usize, usize)> = candidates.iter().map(|c| (c.start, c.end)).collect();
    spans.sort_by_key(|span| std::cmp::Reverse(span.0));
    for (start, end) in spans {
        if end <= out.len() && start <= end {
            out.replace_range(start..end, "");
        }
    }
    out.trim().to_string()
}

use serde_json::json;

#[cfg(test)]
mod tests {
    use super::*;

    fn known(name: &str) -> bool {
        matches!(name, "bash" | "read" | "write" | "grep")
    }

    #[test]
    fn bare_json_extracts_when_whole_content_is_call() {
        let text = r#"{"name": "bash", "arguments": {"command": "ls"}}"#;
        let found = extract_text_tool_calls(text, &known);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "bash");
        assert_eq!(found[0].arguments["command"], "ls");
    }

    #[test]
    fn bare_json_rejected_when_prose_wraps_it() {
        // Planted negative: prose around the object must not extract.
        let text = "I would run this:\n{\"name\": \"bash\", \"arguments\": {\"command\": \"ls\"}}\nWant me to?";
        assert!(extract_text_tool_calls(text, &known).is_empty());
    }

    #[test]
    fn fenced_json_extracts() {
        let text = "Let me check.\n\n```json\n{\"name\": \"read\", \"arguments\": {\"path\": \"src/main.rs\"}}\n```\n";
        let found = extract_text_tool_calls(text, &known);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "read");
    }

    #[test]
    fn fenced_non_tool_lang_never_extracts() {
        // Planted negative: a rust fence showing an example stays prose.
        let text = "Example:\n\n```rust\n{\"name\": \"read\", \"arguments\": {}}\n```\n";
        assert!(extract_text_tool_calls(text, &known).is_empty());
    }

    #[test]
    fn xmlish_tool_call_tag_extracts() {
        let text = r#"Checking. <tool_call>{"name": "grep", "arguments": {"pattern": "TODO"}}</tool_call>"#;
        let found = extract_text_tool_calls(text, &known);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "grep");
    }

    #[test]
    fn unknown_tool_name_rejected() {
        let text = r#"<tool_call>{"name": "nuclear_launch", "arguments": {}}</tool_call>"#;
        assert!(extract_text_tool_calls(text, &known).is_empty());
    }

    #[test]
    fn non_object_arguments_rejected() {
        let text = r#"{"name": "bash", "arguments": "ls -la"}"#;
        assert!(extract_text_tool_calls(text, &known).is_empty());
    }

    #[test]
    fn strip_candidates_removes_span() {
        let text = "before ```json\n{\"name\": \"read\", \"arguments\": {}}\n``` after";
        let found = extract_text_tool_calls(text, &known);
        assert_eq!(found.len(), 1);
        let stripped = strip_candidates(text, &found);
        assert!(!stripped.contains("```json"));
        assert!(stripped.contains("before"));
        assert!(stripped.contains("after"));
    }

    #[test]
    fn dialect_mapping_is_conservative() {
        assert_eq!(dialect_for_model("openai", "gpt-5.5"), Dialect::Harmony);
        assert_eq!(dialect_for_model("ollama", "qwen3:32b"), Dialect::Xmlish);
        assert_eq!(
            dialect_for_model("openrouter", "kimi-k2-0711"),
            Dialect::Xmlish
        );
        assert_eq!(
            dialect_for_model("anthropic", "claude-opus-4-7"),
            Dialect::Native
        );
        assert_eq!(dialect_for_model("openai", "gpt-4o"), Dialect::Native);
    }

    #[test]
    fn ledger_records_entries() {
        let mut ledger = RepairLedger::default();
        ledger.record("read", 120, 40);
        assert_eq!(ledger.entries.len(), 1);
        assert_eq!(ledger.entries[0].tool, "read");
    }
}
