//! Magic keywords (bd-cv653.3.6).
//!
//! Three standalone lowercase words opt a turn into specialized behavior:
//! `ultrathink` (highest supported thinking effort), `orchestrate`
//! (parallel subagents + per-phase verification), `workflowz`
//! (deterministic multi-subagent workflow). They trigger ONLY in prose —
//! never inside code spans, fenced blocks, XML/HTML sections, identifiers,
//! or paths.
//!
//! The tokenizer is the make-or-break correctness surface: it walks the
//! message with a small grammar-aware state machine (fences, inline code,
//! tag sections) and only considers tokens in PROSE state, bounded by
//! whitespace, string edges, or sentence punctuation.

use serde::{Deserialize, Serialize};

/// The keyword actions Pi supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeywordAction {
    /// Set the turn's thinking level to the active model's pre-clamped max.
    Ultrathink,
    /// Inject the parallel-orchestration directive.
    Orchestrate,
    /// Inject the deterministic-workflow directive.
    Workflowz,
}

impl KeywordAction {
    pub const fn word(self) -> &'static str {
        match self {
            Self::Ultrathink => "ultrathink",
            Self::Orchestrate => "orchestrate",
            Self::Workflowz => "workflowz",
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ultrathink => "ultrathink",
            Self::Orchestrate => "orchestrate",
            Self::Workflowz => "workflowz",
        }
    }
}

/// A custom keyword from settings: word → injected directive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomKeyword {
    pub word: String,
    pub directive: String,
}

/// Per-keyword enable flags plus future extensibility.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct KeywordSettings {
    /// Enable `ultrathink` (default true).
    pub ultrathink: Option<bool>,
    /// Enable `orchestrate` (default true).
    pub orchestrate: Option<bool>,
    /// Enable `workflowz` (default true).
    pub workflowz: Option<bool>,
    /// Future keywords: word + injected directive.
    pub extra: Option<Vec<CustomKeyword>>,
}

impl KeywordSettings {
    fn enabled(&self, action: KeywordAction) -> bool {
        match action {
            KeywordAction::Ultrathink => self.ultrathink.unwrap_or(true),
            KeywordAction::Orchestrate => self.orchestrate.unwrap_or(true),
            KeywordAction::Workflowz => self.workflowz.unwrap_or(true),
        }
    }
}

/// One activation recorded for session telemetry.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeywordActivation {
    pub word: String,
    pub action: String,
}

/// Stable schema carried by every session telemetry entry.
pub const KEYWORD_TELEMETRY_SCHEMA_V1: &str = "pi.magic_keyword.v1";

/// Append activation telemetry to the session's replayable custom-entry
/// stream. Both the SDK/RPC wrapper and the interactive TUI call this shared
/// function so audit behavior cannot drift between surfaces.
pub fn append_session_telemetry(
    session: &mut crate::session::Session,
    activations: &[KeywordActivation],
) {
    for activation in activations {
        session.append_custom_entry(
            "magic_keyword".to_string(),
            Some(serde_json::json!({
                "schema": KEYWORD_TELEMETRY_SCHEMA_V1,
                "word": activation.word,
                "action": activation.action,
            })),
        );
    }
}

/// Orchestration directive (omp parity: parallel task decomposition with
/// per-phase verification).
pub const ORCHESTRATE_DIRECTIVE: &str = "<system-reminder>\nThe user invoked `orchestrate` for this turn. Decompose the task into independent slices and run them as parallel subagents with a verification phase per slice; converge on a verified result rather than a single-pass answer.\n</system-reminder>";

/// Deterministic-workflow directive (omp parity: staged waves, named nodes,
/// barrier semantics matching our subagent chain/parallel shapes).
pub const WORKFLOWZ_DIRECTIVE: &str = "<system-reminder>\nThe user invoked `workflowz` for this turn. Execute as a deterministic multi-subagent workflow: name each node, wire dependencies explicitly, run independent nodes as parallel waves with barriers between stages, and verify each wave before proceeding.\n</system-reminder>";

/// Tokenizer state: only prose tokens are keyword-eligible. Delimiter lengths
/// are retained so a shorter or longer backtick run cannot escape a code span
/// or fence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanState {
    Prose,
    InlineCode { delimiter_len: usize },
    FencedCode { marker: u8, delimiter_len: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HtmlMarkup {
    Opening { name: String, self_closing: bool },
    Closing { name: String },
    Opaque,
}

fn is_void_html_tag(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

fn is_ascii_markup_name_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || matches!(byte, b'_' | b':')
}

/// Parse one HTML/XML construct starting at `start`, consuming attributes and
/// comments as one suppressed unit. Quoted `>` bytes inside attributes do not
/// end a tag. An unterminated comment consumes the rest of the message: the
/// safe interpretation is markup, never executable prose.
fn parse_html_markup(message: &str, start: usize) -> Option<(usize, HtmlMarkup)> {
    let bytes = message.as_bytes();
    if bytes.get(start) != Some(&b'<') {
        return None;
    }
    if bytes.get(start..start + 4) == Some(b"<!--") {
        let tail = message.get(start + 4..)?;
        let end = tail
            .find("-->")
            .map_or(bytes.len(), |relative| start + 4 + relative + 3);
        return Some((end, HtmlMarkup::Opaque));
    }

    let mut index = start + 1;
    if matches!(bytes.get(index), Some(b'!') | Some(b'?')) {
        let processing_instruction = bytes.get(index) == Some(&b'?');
        index += 1;
        let mut quote = None;
        let mut declaration_brackets = 0usize;
        while let Some(&byte) = bytes.get(index) {
            match (quote, byte) {
                (Some(expected), current) if current == expected => quote = None,
                (None, b'\'' | b'"') => quote = Some(byte),
                (None, b'[') if !processing_instruction => {
                    declaration_brackets = declaration_brackets.saturating_add(1);
                }
                (None, b']') if !processing_instruction => {
                    declaration_brackets = declaration_brackets.saturating_sub(1);
                }
                (None, b'?')
                    if processing_instruction && bytes.get(index + 1) == Some(&b'>') =>
                {
                    return Some((index + 2, HtmlMarkup::Opaque));
                }
                (None, b'>') if !processing_instruction && declaration_brackets == 0 => {
                    return Some((index + 1, HtmlMarkup::Opaque));
                }
                _ => {}
            }
            index += 1;
        }
        return Some((bytes.len(), HtmlMarkup::Opaque));
    }

    let closing = bytes.get(index) == Some(&b'/');
    if closing {
        index += 1;
    }
    let name_start = index;
    if !bytes.get(index).copied().is_some_and(is_ascii_markup_name_start) {
        return None;
    }
    index += 1;
    while bytes
        .get(index)
        .is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b':' | b'_'))
    {
        index += 1;
    }
    match bytes.get(index) {
        Some(b'>') | Some(b' ' | b'\t' | b'\r' | b'\n') => {}
        Some(b'/') if bytes.get(index + 1) == Some(&b'>') => {}
        _ => return None,
    }
    let name = message.get(name_start..index)?.to_ascii_lowercase();

    let mut quote = None;
    let mut end = None;
    while let Some(&byte) = bytes.get(index) {
        match (quote, byte) {
            (Some(expected), current) if current == expected => quote = None,
            (None, b'\'' | b'"') => quote = Some(byte),
            (None, b'>') => {
                end = Some(index + 1);
                break;
            }
            _ => {}
        }
        index += 1;
    }
    let Some(end) = end else {
        return Some((bytes.len(), HtmlMarkup::Opaque));
    };
    if closing {
        return Some((end, HtmlMarkup::Closing { name }));
    }
    let before_close = message.get(start..end - 1)?.trim_end();
    Some((
        end,
        HtmlMarkup::Opening {
            self_closing: before_close.ends_with('/') || is_void_html_tag(&name),
            name,
        },
    ))
}

fn run_len(bytes: &[u8], start: usize, marker: u8) -> usize {
    bytes
        .get(start..)
        .unwrap_or_default()
        .iter()
        .take_while(|byte| **byte == marker)
        .count()
}

fn fence_close_has_only_indent_after(message: &str, after_run: usize) -> bool {
    let tail = message.get(after_run..).unwrap_or_default();
    tail.split_once('\n')
        .map_or(tail, |(line_tail, _)| line_tail)
        .bytes()
        .all(|byte| matches!(byte, b' ' | b'\t' | b'\r'))
}

fn char_len_at(message: &str, index: usize) -> usize {
    message
        .get(index..)
        .and_then(|tail| tail.chars().next())
        .map_or(message.len().saturating_sub(index).max(1), char::len_utf8)
}

/// Detect enabled keywords in a user message. Returns each action at most
/// once (first hit wins), in message order.
#[allow(clippy::too_many_lines)]
#[must_use]
pub fn detect(message: &str, settings: Option<&KeywordSettings>) -> Vec<KeywordActivation> {
    let mut activations = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut state = ScanState::Prose;
    let mut token = String::new();
    let mut html_stack: Vec<String> = Vec::new();
    let mut line_start = true;
    let mut leading_spaces = 0usize;
    let mut line_indented_code = false;
    let mut index = 0usize;
    let bytes = message.as_bytes();

    let flush_token = |token: &mut String,
                       activations: &mut Vec<KeywordActivation>,
                       seen: &mut std::collections::HashSet<String>| {
        let word = token.as_str();
        for action in [
            KeywordAction::Ultrathink,
            KeywordAction::Orchestrate,
            KeywordAction::Workflowz,
        ] {
            if word == action.word()
                && settings.is_none_or(|s| s.enabled(action))
                && seen.insert(action.as_str().to_string())
            {
                activations.push(KeywordActivation {
                    word: action.word().to_string(),
                    action: action.as_str().to_string(),
                });
            }
        }
        if let Some(settings) = settings
            && let Some(extra) = &settings.extra
        {
            for custom in extra {
                if !custom.word.is_empty()
                    && word == custom.word
                    && seen.insert(custom.word.clone())
                {
                    activations.push(KeywordActivation {
                        word: custom.word.clone(),
                        action: "custom".to_string(),
                    });
                }
            }
        }
        token.clear();
    };

    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'\n' {
            flush_token(&mut token, &mut activations, &mut seen);
            line_start = true;
            leading_spaces = 0;
            line_indented_code = false;
            index += 1;
            continue;
        }
        if line_start && matches!(byte, b' ' | b'\t') {
            leading_spaces += if byte == b'\t' { 4 } else { 1 };
            if leading_spaces >= 4 {
                line_indented_code = true;
            }
            index += 1;
            continue;
        }

        if line_indented_code && html_stack.is_empty() {
            line_start = false;
            index += char_len_at(message, index);
            continue;
        }

        if let ScanState::FencedCode {
            marker,
            delimiter_len,
        } = state
        {
            if line_start && leading_spaces <= 3 && byte == marker {
                let count = run_len(bytes, index, marker);
                let after_run = index + count;
                if count >= delimiter_len && fence_close_has_only_indent_after(message, after_run) {
                    state = ScanState::Prose;
                    index = after_run;
                    line_start = false;
                    continue;
                }
            }
            line_start = false;
            index += char_len_at(message, index);
            continue;
        }

        if let ScanState::InlineCode { delimiter_len } = state {
            if byte == b'`' {
                let count = run_len(bytes, index, b'`');
                if count == delimiter_len {
                    state = ScanState::Prose;
                }
                index += count;
            } else {
                index += char_len_at(message, index);
            }
            line_start = false;
            continue;
        }

        if line_start && html_stack.is_empty() && matches!(byte, b'`' | b'~') {
            let count = run_len(bytes, index, byte);
            if count >= 3 {
                flush_token(&mut token, &mut activations, &mut seen);
                state = ScanState::FencedCode {
                    marker: byte,
                    delimiter_len: count,
                };
                index += count;
                line_start = false;
                continue;
            }
        }
        line_start = false;

        if byte == b'`' && html_stack.is_empty() {
            let count = run_len(bytes, index, b'`');
            flush_token(&mut token, &mut activations, &mut seen);
            state = ScanState::InlineCode {
                delimiter_len: count,
            };
            index += count;
            continue;
        }

        if byte == b'<'
            && let Some((end, markup)) = parse_html_markup(message, index)
        {
            flush_token(&mut token, &mut activations, &mut seen);
            match markup {
                HtmlMarkup::Opening { name, self_closing } => {
                    if !self_closing {
                        html_stack.push(name);
                    }
                }
                HtmlMarkup::Closing { name } => {
                    if html_stack.last() == Some(&name) {
                        html_stack.pop();
                    }
                }
                HtmlMarkup::Opaque => {}
            }
            index = end;
            continue;
        }

        let Some(ch) = message.get(index..).and_then(|tail| tail.chars().next()) else {
            break;
        };
        index += ch.len_utf8();
        if line_indented_code || !html_stack.is_empty() {
            flush_token(&mut token, &mut activations, &mut seen);
            continue;
        }

        let dot_continues_path = ch == '.'
            && message
                .get(index..)
                .and_then(|tail| tail.chars().next())
                .is_some_and(|next| next.is_alphanumeric() || matches!(next, '_' | '-'));
        let is_boundary = ch.is_whitespace()
            || (!dot_continues_path
                && matches!(
                    ch,
                    ',' | '.'
                        | '!'
                        | '?'
                        | ':'
                        | ';'
                        | '('
                        | ')'
                        | '"'
                        | '\''
                        | '['
                        | ']'
                        | '{'
                        | '}'
                        | '*'
                ));
        if is_boundary {
            flush_token(&mut token, &mut activations, &mut seen);
            continue;
        }
        token.push(ch);
    }
    if state == ScanState::Prose && html_stack.is_empty() && !line_indented_code {
        flush_token(&mut token, &mut activations, &mut seen);
    }
    activations
}

/// Map activations to their injected directives.
///
/// Path safety is structural: `/` is not a boundary, and a `.` followed by a
/// filename character is retained, so neither `/tmp/ultrathink` nor
/// `ultrathink.rs` equals a keyword token.
#[must_use]
pub fn directives_for(
    activations: &[KeywordActivation],
    settings: Option<&KeywordSettings>,
) -> Vec<String> {
    let mut directives = Vec::new();
    for activation in activations {
        match activation.action.as_str() {
            "orchestrate" => directives.push(ORCHESTRATE_DIRECTIVE.to_string()),
            "workflowz" => directives.push(WORKFLOWZ_DIRECTIVE.to_string()),
            "custom" => {
                if let Some(settings) = settings
                    && let Some(extra) = &settings.extra
                    && let Some(custom) = extra.iter().find(|c| c.word == activation.word)
                {
                    directives.push(custom.directive.clone());
                }
            }
            _ => {}
        }
    }
    directives
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(message: &str) -> Vec<String> {
        detect(message, None)
            .into_iter()
            .map(|activation| activation.word)
            .collect()
    }

    #[test]
    fn prose_triggers_each_keyword_once() {
        assert_eq!(words("please ultrathink this design"), ["ultrathink"]);
        assert_eq!(words("orchestrate the migration"), ["orchestrate"]);
        assert_eq!(words("workflowz please"), ["workflowz"]);
        // Idempotent per turn.
        assert_eq!(words("ultrathink then ultrathink again"), ["ultrathink"]);
    }

    #[test]
    fn code_spans_and_fences_never_trigger() {
        assert!(words("`ultrathink` in backticks").is_empty());
        assert!(words("```\nultrathink\n```").is_empty());
        assert!(words("some `code ultrathink code` here").is_empty());
        assert!(
            words("``code ` ultrathink still code``").is_empty(),
            "a shorter backtick run must not close a longer code span"
        );
        assert_eq!(
            words("``code ` ultrathink still code`` then orchestrate"),
            ["orchestrate"]
        );
        assert!(
            words("```rust\nultrathink\n``\nworkflowz\n```").is_empty(),
            "a shorter fence must not expose fenced content"
        );
        assert!(
            words("````rust\nultrathink\n```\nworkflowz\n````").is_empty(),
            "a shorter matching-marker fence must not close a longer fence"
        );
        assert!(
            words("prefix ``` ultrathink ``` suffix").is_empty(),
            "mid-line backtick runs are code spans, not fences"
        );
    }

    #[test]
    fn tilde_fences_never_trigger() {
        assert!(words("~~~\nultrathink\n~~~").is_empty());
        assert!(
            words("~~~\nultrathink\n~~~\nthen orchestrate here") == ["orchestrate"],
            "prose after a closed tilde fence still triggers"
        );
        // Fence chars don't cross-close: a ~~~ inside a ``` block is
        // content, and the ``` fence stays open past it.
        assert!(words("```\n~~~\nultrathink\n```").is_empty());
        // Strikethrough tildes mid-line are ordinary prose chars.
        assert_eq!(words("~~scratch that~~ ultrathink please"), ["ultrathink"]);
    }

    #[test]
    fn indented_code_lines_never_trigger() {
        assert!(words("look at this:\n\n    ultrathink(); // sample code").is_empty());
        assert!(words("\tultrathink in a tab-indented line").is_empty());
        // The suppression is per line: prose on the next line triggers.
        assert_eq!(
            words("    ultrathink as code\nbut ultrathink here is prose"),
            ["ultrathink"]
        );
        assert_eq!(
            words("    ```\nultrathink is prose on the next line"),
            ["ultrathink"],
            "an indented fence marker is code, not an opening fence"
        );
        // 1-3 leading spaces are still prose.
        assert_eq!(words("   ultrathink with three spaces"), ["ultrathink"]);
    }

    #[test]
    fn xml_sections_never_trigger() {
        assert!(words("<system-reminder>ultrathink</system-reminder>").is_empty());
        assert!(words("<think>ultrathink</think>").is_empty());
        assert!(
            words("<div data-mode=\"ultrathink\">workflowz</div>").is_empty(),
            "attributes and element bodies are both markup"
        );
        assert!(
            words("<!-- ultrathink and orchestrate -->").is_empty(),
            "HTML comments are not prose"
        );
        assert_eq!(
            words("<div><span>ultrathink</span></div> then workflowz"),
            ["workflowz"]
        );
        assert_eq!(
            words("<div>\n    ultrathink\n    </div>\nworkflowz"),
            ["workflowz"],
            "indentation inside an HTML section must not hide its closing tag"
        );
        assert_eq!(
            words("before <br data-mode='ultrathink'> orchestrate"),
            ["orchestrate"],
            "void tags must not suppress following prose"
        );
        assert_eq!(
            words("see <https://example.com/ultrathink> then orchestrate"),
            ["orchestrate"],
            "Markdown autolinks are not opening HTML tags"
        );
        assert!(
            words("<_guard>ultrathink</_guard>").is_empty(),
            "XML names may begin with an underscore"
        );
        assert!(
            words("<:guard>ultrathink</:guard>").is_empty(),
            "XML names may begin with a namespace separator"
        );
        assert!(
            words("<!DOCTYPE guard [ <!ENTITY mode 'ultrathink'> ]>").is_empty(),
            "keywords inside an internal declaration subset are markup"
        );
        assert!(
            words("<?guard mode='?> ultrathink'?>").is_empty(),
            "a quoted processing-instruction terminator must not expose markup"
        );
        assert!(
            words("<guard mode='unfinished ultrathink").is_empty(),
            "an unterminated tag consumes the remaining input fail-closed"
        );
    }

    #[test]
    fn identifiers_and_paths_never_trigger() {
        assert!(words("ultrathink_mode").is_empty());
        assert!(words("preultrathink").is_empty());
        assert!(words("/tmp/ultrathink").is_empty());
        assert!(words("see https://example.com/ultrathink docs").is_empty());
        assert!(
            words("edit ultrathink.rs next").is_empty(),
            "a bare filename is still a path, even without a slash"
        );
    }

    #[test]
    fn punctuation_boundaries_trigger() {
        assert_eq!(words("ultrathink,"), ["ultrathink"]);
        assert_eq!(words("(ultrathink)"), ["ultrathink"]);
        assert_eq!(words("ok. ultrathink."), ["ultrathink"]);
    }

    #[test]
    fn settings_disable_each_keyword() {
        let settings = KeywordSettings {
            ultrathink: Some(false),
            ..Default::default()
        };
        assert!(detect("ultrathink", Some(&settings)).is_empty());
        let settings = KeywordSettings {
            orchestrate: Some(false),
            workflowz: Some(false),
            ..Default::default()
        };
        let found = detect("orchestrate and workflowz but ultrathink", Some(&settings));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].word, "ultrathink");
    }

    #[test]
    fn custom_keywords_extend_the_set() {
        let settings = KeywordSettings {
            extra: Some(vec![CustomKeyword {
                word: "deepdive".to_string(),
                directive: "<sys>go deep</sys>".to_string(),
            }]),
            ..Default::default()
        };
        let found = detect("please deepdive this", Some(&settings));
        assert_eq!(found.len(), 1);
        let directives = directives_for(&found, Some(&settings));
        assert_eq!(directives, vec!["<sys>go deep</sys>".to_string()]);
    }

    #[test]
    fn empty_custom_keyword_never_activates() {
        let settings = KeywordSettings {
            extra: Some(vec![CustomKeyword {
                word: String::new(),
                directive: "must not inject".to_string(),
            }]),
            ..Default::default()
        };
        assert!(detect("ordinary prose", Some(&settings)).is_empty());
        assert!(detect("", Some(&settings)).is_empty());
    }
}
