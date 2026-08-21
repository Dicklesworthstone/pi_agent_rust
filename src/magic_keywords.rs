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
    /// Set the turn's thinking level to the model's max (clamped downstream).
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

/// Orchestration directive (omp parity: parallel task decomposition with
/// per-phase verification).
pub const ORCHESTRATE_DIRECTIVE: &str = "<system-reminder>\nThe user invoked `orchestrate` for this turn. Decompose the task into independent slices and run them as parallel subagents with a verification phase per slice; converge on a verified result rather than a single-pass answer.\n</system-reminder>";

/// Deterministic-workflow directive (omp parity: staged waves, named nodes,
/// barrier semantics matching our subagent chain/parallel shapes).
pub const WORKFLOWZ_DIRECTIVE: &str = "<system-reminder>\nThe user invoked `workflowz` for this turn. Execute as a deterministic multi-subagent workflow: name each node, wire dependencies explicitly, run independent nodes as parallel waves with barriers between stages, and verify each wave before proceeding.\n</system-reminder>";

/// Tokenizer state: only Prose tokens are keyword-eligible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanState {
    Prose,
    InlineCode,
    FencedCode,
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
    let mut chars = message.chars().peekable();
    // Track tag sections by counting open/close of the same tag name.
    let mut tag_depth: i32 = 0;

    let flush_token = |token: &mut String,
                       state: ScanState,
                       tag_depth: i32,
                       activations: &mut Vec<KeywordActivation>,
                       seen: &mut std::collections::HashSet<String>| {
        if state == ScanState::Prose && tag_depth == 0 {
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
                    if word == custom.word && seen.insert(custom.word.clone()) {
                        activations.push(KeywordActivation {
                            word: custom.word.clone(),
                            action: "custom".to_string(),
                        });
                    }
                }
            }
        }
        token.clear();
    };

    while let Some(ch) = chars.next() {
        // Fences: ``` toggles fenced state (prose only).
        if ch == '`' {
            let mut backticks = 1;
            while chars.peek() == Some(&'`') {
                chars.next();
                backticks += 1;
            }
            if backticks >= 3 {
                state = match state {
                    ScanState::FencedCode => ScanState::Prose,
                    _ => ScanState::FencedCode,
                };
            } else if state == ScanState::Prose {
                state = ScanState::InlineCode;
            } else if state == ScanState::InlineCode {
                state = ScanState::Prose;
            }
            flush_token(
                &mut token,
                ScanState::InlineCode,
                tag_depth,
                &mut activations,
                &mut seen,
            );
            continue;
        }

        // Tag sections: <tag> ... </tag> (prose only, any tag name).
        if state != ScanState::FencedCode && ch == '<' {
            let is_close = chars.peek() == Some(&'/');
            if is_close {
                chars.next();
            }
            let mut tag = String::new();
            let mut valid = false;
            while let Some(&next) = chars.peek() {
                if next == '>' {
                    chars.next();
                    valid = !tag.is_empty()
                        && tag
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == ':');
                    break;
                }
                if next.is_whitespace() {
                    break;
                }
                tag.push(next);
                chars.next();
            }
            flush_token(&mut token, state, tag_depth, &mut activations, &mut seen);
            if valid {
                tag_depth = if is_close {
                    (tag_depth - 1).max(0)
                } else {
                    tag_depth + 1
                };
            }
            // When the '<' wasn't a tag it's literal — the flush above is
            // the boundary behavior either way.
            continue;
        }

        // Word boundaries: whitespace, string edges, sentence punctuation.
        let is_boundary = ch.is_whitespace()
            || matches!(
                ch,
                ',' | '.' | '!' | '?' | ':' | ';' | '(' | ')' | '"' | '\''
            );
        if is_boundary {
            flush_token(&mut token, state, tag_depth, &mut activations, &mut seen);
            continue;
        }

        // Paths/URLs: a token containing '/' is never a keyword.
        token.push(ch);
    }
    flush_token(&mut token, state, tag_depth, &mut activations, &mut seen);

    // Tokens containing '/' (paths/URLs) are ineligible — post-filter.
    activations.retain(|activation| !activation.word.contains('/'));
    activations
}

/// Map activations to their injected directives.
///
/// Path safety is structural: `/` is not a boundary, so a path like
/// /tmp/ultrathink reads as one token and never equals a keyword; the
/// activation-level retain in `detect` is belt-and-braces for future
/// keyword shapes.
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
    }

    #[test]
    fn xml_sections_never_trigger() {
        assert!(words("<system-reminder>ultrathink</system-reminder>").is_empty());
        assert!(words("<think>ultrathink</think>").is_empty());
    }

    #[test]
    fn identifiers_and_paths_never_trigger() {
        assert!(words("ultrathink_mode").is_empty());
        assert!(words("preultrathink").is_empty());
        assert!(words("/tmp/ultrathink").is_empty());
        assert!(words("see https://example.com/ultrathink docs").is_empty());
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
}
