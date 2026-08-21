//! Secrets obfuscation vault (bd-cv653.7.9).
//!
//! Credential-shaped values are replaced with stable placeholders before
//! provider calls and restored on the way back: read .env → tool result →
//! provider payload never flows raw. The vault map lives in memory for the
//! session and dies with it — session files and compaction summaries carry
//! placeholders only.
//!
//! Detection: versioned pattern rules (sk-ant-*, sk-*, ghp_*, AKIA*,
//! xox[bap]-*, private-key headers, DSN/connection strings, generic
//! KEY=value high-entropy assignments) with a measured false-positive
//! budget. This module is the SINGLE detector for the whole program
//! (bd-cv653.4.1's memory screener composes with it).
//!
//! Modes (`secrets.mode`): `off` | `obfuscate` (default) | `block`
//! (refuse to send, loud named error). Audit events per transform are
//! redacted (counts only, never values).

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Tool-result schema tag for secrets operations.
pub const SECRETS_SCHEMA: &str = "pi.secrets.v1";

/// Secrets mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SecretsMode {
    /// No transform (pre-vault behavior).
    Off,
    /// Replace credential shapes with placeholders outbound; restore
    /// inbound (default).
    #[default]
    Obfuscate,
    /// Refuse to send a message containing a credential shape (loud named
    /// error).
    Block,
}

impl SecretsMode {
    #[must_use]
    pub fn from_setting(raw: Option<&str>) -> Self {
        match raw
            .unwrap_or("obfuscate")
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "off" | "false" | "disabled" => Self::Off,
            "block" => Self::Block,
            _ => Self::Obfuscate,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Obfuscate => "obfuscate",
            Self::Block => "block",
        }
    }
}

/// `secrets` settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SecretsSettings {
    /// off | obfuscate | block (default obfuscate).
    pub mode: Option<String>,
    /// User-added regex patterns (each match becomes a placeholder).
    pub extra_patterns: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// Detector (single source of truth for the whole program)
// ---------------------------------------------------------------------------

/// One detector rule: name + regex + placeholder label.
struct Rule {
    name: &'static str,
    regex: regex::Regex,
    label: &'static str,
}

/// Versioned in-tree rules (bump SECRETS_RULESET_VERSION when editing).
pub const SECRETS_RULESET_VERSION: u32 = 1;

fn rules() -> &'static Vec<Rule> {
    static RULES: std::sync::LazyLock<Vec<Rule>> = std::sync::LazyLock::new(|| {
        vec![
            Rule {
                name: "openai-key",
                regex: regex::Regex::new(r"sk-[A-Za-z0-9_\-]{16,}").expect("rule"),
                label: "openai_key",
            },
            Rule {
                name: "anthropic-key",
                regex: regex::Regex::new(r"sk-ant-[A-Za-z0-9_\-]{16,}").expect("rule"),
                label: "anthropic_key",
            },
            Rule {
                name: "github-pat",
                regex: regex::Regex::new(r"ghp_[A-Za-z0-9]{20,}").expect("rule"),
                label: "github_pat",
            },
            Rule {
                name: "github-pat-fine",
                regex: regex::Regex::new(r"github_pat_[A-Za-z0-9_]{20,}").expect("rule"),
                label: "github_pat",
            },
            Rule {
                name: "aws-access-key",
                regex: regex::Regex::new(r"AKIA[0-9A-Z]{16}").expect("rule"),
                label: "aws_access_key",
            },
            Rule {
                name: "private-key",
                regex: regex::Regex::new(r"-----BEGIN [A-Z ]*PRIVATE KEY-----").expect("rule"),
                label: "private_key",
            },
            Rule {
                name: "google-api-key",
                regex: regex::Regex::new(r"AIza[0-9A-Za-z_\-]{20,}").expect("rule"),
                label: "google_api_key",
            },
            Rule {
                name: "slack-token",
                regex: regex::Regex::new(r"xox[baprs]-[A-Za-z0-9\-]{10,}").expect("rule"),
                label: "slack_token",
            },
            Rule {
                name: "dsn",
                regex: regex::Regex::new(
                    r"(?i)(postgres|mysql|mongodb|redis|amqp)://[^\s/:@]+:[^\s/@]+@",
                )
                .expect("rule"),
                label: "dsn",
            },
            // Generic KEY=value assignments with a high-entropy value:
            // 16+ chars with mixed classes, no spaces.
            Rule {
                name: "generic-assignment",
                regex: regex::Regex::new(
                    r#"(?i)(?:api[_-]?key|secret|token|password|passwd|pwd)\s*[=:]\s*['"]?([A-Za-z0-9+/=_\-]{16,})['"]?"#,
                )
                .expect("rule"),
                label: "generic_secret",
            },
        ]
    });
    &RULES
}

/// One detection: byte range + rule label (never carries the value).
#[derive(Debug, Clone)]
pub struct Detection {
    pub start: usize,
    pub end: usize,
    pub rule: &'static str,
    pub label: &'static str,
}

/// Scan text for credential shapes. User patterns compose on top.
#[must_use]
pub fn scan(text: &str, extra_patterns: &[regex::Regex]) -> Vec<Detection> {
    let mut out = Vec::new();
    for rule in rules() {
        for m in rule.regex.find_iter(text) {
            out.push(Detection {
                start: m.start(),
                end: m.end(),
                rule: rule.name,
                label: rule.label,
            });
        }
    }
    for (index, pattern) in extra_patterns.iter().enumerate() {
        for m in pattern.find_iter(text) {
            out.push(Detection {
                start: m.start(),
                end: m.end(),
                rule: "user",
                label: USER_LABELS[index % USER_LABELS.len()],
            });
        }
    }
    out.sort_by_key(|d| d.start);
    out
}

const USER_LABELS: &[&str] = &["user_pattern"];

/// Does the text contain any credential shape?
#[must_use]
pub fn contains_secret(text: &str, extra_patterns: &[regex::Regex]) -> bool {
    !scan(text, extra_patterns).is_empty()
}

// ---------------------------------------------------------------------------
// Session vault
// ---------------------------------------------------------------------------

/// Session-scoped placeholder map. Lives in memory; dies with the session.
#[derive(Debug, Default)]
pub struct SecretVault {
    /// real value → placeholder (stable per session).
    by_value: std::collections::HashMap<String, String>,
    /// placeholder → real value.
    by_placeholder: std::collections::HashMap<String, String>,
    /// Placeholder sequence.
    next: u64,
}

impl SecretVault {
    fn placeholder_for(&mut self, value: &str, label: &str) -> String {
        if let Some(existing) = self.by_value.get(value) {
            return existing.clone();
        }
        self.next = self.next.saturating_add(1);
        let placeholder = format!("<pi-secret:{:06x}>", self.next);
        self.by_value.insert(value.to_string(), placeholder.clone());
        self.by_placeholder
            .insert(placeholder.clone(), value.to_string());
        let _ = label;
        placeholder
    }

    /// Restore any placeholders in `text` to their real values.
    #[must_use]
    pub fn restore(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (placeholder, real) in &self.by_placeholder {
            if out.contains(placeholder.as_str()) {
                out = out.replace(placeholder.as_str(), real);
            }
        }
        out
    }

    /// Mask any real values in `text` back to placeholders (echo hygiene).
    #[must_use]
    pub fn mask(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (placeholder, real) in &self.by_placeholder {
            if out.contains(real.as_str()) {
                out = out.replace(real.as_str(), placeholder);
            }
        }
        out
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.by_value.len()
    }
}

/// Per-transform audit record (redacted: counts and rule labels only).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransformAudit {
    pub schema: String,
    pub direction: String,
    pub detections: usize,
    pub rules: Vec<String>,
}

/// The outbound transform: replace detections with stable placeholders.
/// Returns the transformed text + audit.
pub fn obfuscate(
    text: &str,
    vault: &mut SecretVault,
    extra_patterns: &[regex::Regex],
) -> (String, TransformAudit) {
    let detections = scan(text, extra_patterns);
    if detections.is_empty() {
        return (
            text.to_string(),
            TransformAudit {
                schema: SECRETS_SCHEMA.to_string(),
                direction: "outbound".to_string(),
                detections: 0,
                rules: Vec::new(),
            },
        );
    }
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    let mut rules_hit: Vec<String> = Vec::new();
    for detection in &detections {
        if detection.start < cursor {
            continue; // overlapping rule hit
        }
        out.push_str(&text[cursor..detection.start]);
        let value = &text[detection.start..detection.end];
        let placeholder = vault.placeholder_for(value, detection.label);
        out.push_str(&placeholder);
        cursor = detection.end;
        if !rules_hit.iter().any(|r| r == detection.label) {
            rules_hit.push(detection.label.to_string());
        }
    }
    out.push_str(&text[cursor..]);
    (
        out,
        TransformAudit {
            schema: SECRETS_SCHEMA.to_string(),
            direction: "outbound".to_string(),
            detections: detections.len(),
            rules: rules_hit,
        },
    )
}

/// The inbound restore: placeholders → real values before tool execution.
#[must_use]
pub fn restore(text: &str, vault: &SecretVault) -> String {
    vault.restore(text)
}

/// Mode gate for the outbound send path.
///
/// # Errors
/// Named `PI_SECRET_BLOCK` in block mode when detections exist.
pub fn gate_outbound(text: &str, mode: SecretsMode, extra_patterns: &[regex::Regex]) -> Result<()> {
    if mode == SecretsMode::Block && contains_secret(text, extra_patterns) {
        return Err(Error::validation(
            "PI_SECRET_BLOCK: message contains credential-shaped content and secrets.mode=block \
             — refusing to send. Remove the secret or switch secrets.mode to obfuscate."
                .to_string(),
        ));
    }
    Ok(())
}

/// Compile user extra patterns (settings) into regexes.
#[must_use]
pub fn compile_extra_patterns(patterns: &[String]) -> Vec<regex::Regex> {
    patterns
        .iter()
        .filter_map(|pattern| regex::Regex::new(pattern).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detector_finds_known_shapes() {
        assert!(contains_secret("key = sk-abcdefghijklmnopqrstuvwxyz", &[]));
        assert!(contains_secret("sk-ant-api03-aaaaaaaaaaaaaaaaaaaa", &[]));
        assert!(contains_secret("ghp_abcdefghijklmnopqrstuvwxyz0123", &[]));
        assert!(contains_secret(concat!("AKIA", "IOSFODNN7EXAMPLE"), &[])); // AWS docs example
        assert!(contains_secret(
            concat!("-----BEGIN ", "OPENSSH PRIVATE KEY-----"),
            &[]
        ));
        assert!(contains_secret(
            "postgres://user:hunter2secret@db.internal/prod",
            &[]
        ));
        assert!(contains_secret("api_key = sk_live_51abcdefghijklmnop", &[]));
    }

    #[test]
    fn detector_passes_clean_code() {
        assert!(!contains_secret("fn main() { println!(\"hello\"); }", &[]));
        assert!(!contains_secret("let timeout = 30;", &[]));
        assert!(!contains_secret("use std::collections::HashMap;", &[]));
        assert!(!contains_secret("const MAX: usize = 1024;", &[]));
    }

    #[test]
    fn vault_placeholder_stable_and_restorable() {
        let mut vault = SecretVault::default();
        let (first, _) = obfuscate("the key is sk-aaaaaaaaaaaaaaaaaaaaaaaa", &mut vault, &[]);
        let (second, _) = obfuscate("again: sk-aaaaaaaaaaaaaaaaaaaaaaaa", &mut vault, &[]);
        let placeholder = "<pi-secret:000001>";
        assert!(first.contains(placeholder), "{first}");
        assert!(second.contains(placeholder), "stable per session: {second}");
        assert_eq!(vault.len(), 1);

        let restored = vault.restore(&first);
        assert!(restored.contains("sk-aaaaaaaaaaaaaaaaaaaaaaaa"));
        let masked = vault.mask("echo sk-aaaaaaaaaaaaaaaaaaaaaaaa");
        assert!(masked.contains(placeholder), "{masked}");
    }

    #[test]
    fn block_mode_refuses_with_named_error() {
        let err =
            gate_outbound("sk-aaaaaaaaaaaaaaaaaaaaaaaa", SecretsMode::Block, &[]).unwrap_err();
        assert!(err.to_string().contains("PI_SECRET_BLOCK"), "{err}");
        assert!(gate_outbound("clean text", SecretsMode::Block, &[]).is_ok());
        assert!(gate_outbound("sk-aaaaaaaaaaaaaaaaaaaaaaaa", SecretsMode::Off, &[]).is_ok());
    }

    #[test]
    fn overlapping_hits_collapse_cleanly() {
        let mut vault = SecretVault::default();
        // The generic rule overlaps the specific sk- rule: one placeholder.
        let (out, audit) = obfuscate("api_key=sk-aaaaaaaaaaaaaaaaaaaaaaaa", &mut vault, &[]);
        assert!(out.contains("<pi-secret:"), "{out}");
        assert!(audit.detections >= 1);
    }
}
