//! Secrets obfuscation vault (bd-cv653.7.9).
//!
//! Credential-shaped values are replaced with stable placeholders before
//! provider calls and restored before local tool execution. The raw-value
//! map lives in memory for the session and dies with it; callers must apply
//! the appropriate outbound/export transform before publishing transcript
//! text, since the local transcript may retain user-authored input.
//!
//! Detection: versioned pattern rules (sk-ant-*, sk-* including dotted
//! BaiLian-style keys, gh?_*, AKIA*, AIza*, xox[bap]-*, JWTs, complete private
//! key envelopes, DSN/connection strings, generic KEY=value high-entropy
//! assignments). This module is the SINGLE detector for the whole program;
//! memory and crash-report screening compose with it.
//!
//! Modes (`secrets.mode`): `off` | `obfuscate` (default) | `block`
//! (refuse to send, loud named error). Audit events per transform are
//! redacted (counts only, never values).

use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::BTreeMap;

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
    /// User-added regex patterns (each nonempty match becomes a placeholder).
    pub extra_patterns: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// Detector (single source of truth for the whole program)
// ---------------------------------------------------------------------------

/// One detector rule: name + regex + placeholder label, plus an optional
/// post-filter that can veto a regex hit on the captured value.
struct Rule {
    name: &'static str,
    regex: regex::Regex,
    label: &'static str,
    reject: Option<fn(&str) -> bool>,
}

/// Veto for the generic rule's dotted values: a dotted value with no digit
/// is far more often a code path (`apiKey: process.env.OPENAI_API_KEY`,
/// `password: self.config.password`) than a credential, while real dotted
/// keys carry digits. Undotted values keep the pre-v2 behavior.
fn dotted_without_digit(value: &str) -> bool {
    value.contains('.') && !value.bytes().any(|b| b.is_ascii_digit())
}

/// Versioned in-tree rules (bump when detection semantics change).
///
/// v2 added dotted provider keys without consuming trailing punctuation or
/// treating dots as token characters for the minimum length.
/// v3 protects the entire private-key envelope, including encrypted-key
/// metadata and truncated bodies, and unions overlapping matches so a short
/// match cannot expose the tail of a longer credential. The first rule in a
/// merged region supplies its diagnostic label; rule order breaks start ties.
/// v4 recognizes quoted assignment keys in JSON/configuration text too.
pub const SECRETS_RULESET_VERSION: u32 = 4;

/// Token body: `min` or more characters from `class`, with single dots
/// permitted between characters, ending on a `tail_class` character (the
/// callers pass alphanumerics so trailing punctuation is never captured).
/// `min` must be ≥ 2.
fn dotted_body(class: &str, tail_class: &str, min: usize) -> String {
    debug_assert!(min >= 2, "dotted_body needs room for head + tail");
    let middle = min - 2;
    format!("[{class}](?:\\.?[{class}]){{{middle},}}\\.?[{tail_class}]")
}

fn rules() -> &'static Vec<Rule> {
    static RULES: std::sync::LazyLock<Vec<Rule>> = std::sync::LazyLock::new(|| {
        const TOKEN: &str = r"A-Za-z0-9_\-";
        const ALNUM: &str = "A-Za-z0-9";
        let sk_body = dotted_body(TOKEN, ALNUM, 16);
        vec![
            // Before `openai-key`: same prefix, tighter shape.
            Rule {
                name: "anthropic-key",
                regex: regex::Regex::new(&format!(r"sk-ant-{sk_body}")).expect("rule"),
                label: "anthropic_key",
                reject: None,
            },
            Rule {
                name: "openai-key",
                regex: regex::Regex::new(&format!(r"sk-{sk_body}")).expect("rule"),
                label: "openai_key",
                reject: None,
            },
            // Classic PATs plus the OAuth/user/server/refresh prefixes that
            // share the `gh?_` shape.
            Rule {
                name: "github-pat",
                regex: regex::Regex::new(r"gh[pousr]_[A-Za-z0-9]{20,}").expect("rule"),
                label: "github_pat",
                reject: None,
            },
            Rule {
                name: "github-pat-fine",
                regex: regex::Regex::new(r"github_pat_[A-Za-z0-9_]{20,}").expect("rule"),
                label: "github_pat",
                reject: None,
            },
            Rule {
                name: "aws-access-key",
                regex: regex::Regex::new(r"AKIA[0-9A-Z]{16}").expect("rule"),
                label: "aws_access_key",
                reject: None,
            },
            Rule {
                name: "private-key",
                // `scan` extends this marker through its matching footer.
                // Masking only the header leaves the private material intact.
                regex: regex::Regex::new(r"-----BEGIN [A-Z ]*PRIVATE KEY-----").expect("rule"),
                label: "private_key",
                reject: None,
            },
            Rule {
                name: "google-api-key",
                regex: regex::Regex::new(r"AIza[0-9A-Za-z_\-]{20,}").expect("rule"),
                label: "google_api_key",
                reject: None,
            },
            Rule {
                name: "slack-token",
                regex: regex::Regex::new(r"xox[baprs]-[A-Za-z0-9\-]{10,}").expect("rule"),
                label: "slack_token",
                reject: None,
            },
            // Signed JWT: `eyJ` (base64url of `{"`) header, payload,
            // signature — three dot-separated base64url segments.
            Rule {
                name: "jwt",
                regex: regex::Regex::new(
                    r"eyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}",
                )
                .expect("rule"),
                label: "jwt",
                reject: None,
            },
            Rule {
                name: "dsn",
                regex: regex::Regex::new(
                    // Non-capturing: group 1 is reserved for "the value to
                    // vault" (see `scan`); the whole DSN is the secret here.
                    r"(?i)(?:postgres|mysql|mongodb|redis|amqp)://[^\s/:@]+:[^\s/@]+@",
                )
                .expect("rule"),
                label: "dsn",
                reject: None,
            },
            // Quoted or bare assignment keys; group 1 is still ONLY the
            // value. JSON's closing key quote must not bypass this rule.
            Rule {
                name: "generic-assignment",
                regex: regex::Regex::new(&format!(
                    r#"(?i)["']?(?:api[_-]?key|secret|token|password|passwd|pwd)["']?\s*[=:]\s*['"]?({})['"]?"#,
                    dotted_body(r"A-Za-z0-9+/=_\-", "A-Za-z0-9+/=", 16)
                ))
                .expect("rule"),
                label: "generic_secret",
                reject: Some(dotted_without_digit),
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
///
/// Returns sorted, nonempty, nonoverlapping byte ranges. Every byte covered
/// by any matching rule is covered by the result. Overlapping matches form
/// one region; adjacent independent matches remain separate. Consumers can
/// replace these ranges in order without exposing overlapping suffixes.
#[must_use]
pub fn scan(text: &str, extra_patterns: &[regex::Regex]) -> Vec<Detection> {
    let mut out = Vec::new();
    for rule in rules() {
        let mut covered_until = 0;
        for caps in rule.regex.captures_iter(text) {
            // Group 1 isolates the value in KEY=value assignments. Keep the
            // assignment key outside the vault so inbound restores remain values.
            let m = caps.get(1).or_else(|| caps.get(0)).expect("match group 0");
            if m.start() < covered_until
                || rule.reject.is_some_and(|reject| reject(m.as_str()))
            {
                continue;
            }
            let end = if rule.name == "private-key" {
                private_key_end(text, m.start(), m.end())
            } else {
                m.end()
            };
            // A missing footer protects the remainder. Do not repeatedly
            // search that same suffix for nested or malformed opening markers.
            covered_until = end;
            out.push(Detection {
                start: m.start(),
                end,
                rule: rule.name,
                label: rule.label,
            });
        }
    }
    for pattern in extra_patterns {
        for m in pattern.find_iter(text) {
            if !m.is_empty() {
                out.push(Detection {
                    start: m.start(),
                    end: m.end(),
                    rule: "user",
                    label: "user_pattern",
                });
            }
        }
    }
    merge_detections(out)
}

/// PEM labels are ASCII and the marker regex already validated the bounds.
/// Match the exact label, not just the next END line: a mismatched footer
/// cannot terminate a secret and expose the rest. A truncated envelope is
/// ambiguous, so fail closed through the end of the supplied text.
fn private_key_end(text: &str, start: usize, header_end: usize) -> usize {
    const BEGIN: &str = "-----BEGIN ";
    const DASHES: &str = "-----";
    let label = &text[start + BEGIN.len()..header_end - DASHES.len()];
    let footer = format!("-----END {label}-----");
    text[header_end..].find(&footer).map_or(text.len(), |offset| {
        header_end + offset + footer.len()
    })
}

fn merge_detections(mut detections: Vec<Detection>) -> Vec<Detection> {
    // Stable sort preserves specific-rule precedence for equal start offsets.
    detections.sort_by_key(|detection| detection.start);
    let mut merged: Vec<Detection> = Vec::with_capacity(detections.len());
    for detection in detections {
        if let Some(previous) = merged.last_mut()
            && detection.start < previous.end
        {
            previous.end = previous.end.max(detection.end);
        } else {
            merged.push(detection);
        }
    }
    merged
}

/// Does the text contain any credential shape?
#[must_use]
pub fn contains_secret(text: &str, extra_patterns: &[regex::Regex]) -> bool {
    !scan(text, extra_patterns).is_empty()
}

// ---------------------------------------------------------------------------
// Session vault
// ---------------------------------------------------------------------------

/// Session-scoped placeholder map. Lives in memory; dies with the session.
#[derive(Default)]
pub struct SecretVault {
    /// real value → placeholder (stable per session).
    by_value: std::collections::HashMap<String, String>,
    /// placeholder → real value.
    by_placeholder: std::collections::HashMap<String, String>,
    /// Placeholder sequence.
    next: u64,
}

impl std::fmt::Debug for SecretVault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretVault")
            .field("entries", &self.by_value.len())
            .finish_non_exhaustive()
    }
}

type MaskReplacements<'a> = BTreeMap<Cow<'a, str>, &'a str>;

/// An overlapping combination with no exact registered value cannot be
/// restored safely by a read-only masker. Do not label it with a different
/// credential's placeholder; redact it explicitly instead.
const OVERLAP_REDACTION: &str = "<pi-secret:redacted>";

fn placeholder_pattern() -> &'static regex::Regex {
    static PATTERN: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"<pi-secret:[0-9a-f]{6,16}>").expect("placeholder grammar")
    });
    &PATTERN
}

fn inside_placeholder(start: usize, end: usize, protected: &[(usize, usize)]) -> bool {
    protected
        .iter()
        .any(|&(left, right)| start >= left && end <= right)
}

/// Find literal values against the ORIGINAL input, including overlapping
/// occurrences. Advancing one Unicode scalar after a match preserves byte
/// boundaries without skipping the final occurrence of a repeated value.
fn literal_detections<'a>(
    text: &str,
    values: impl IntoIterator<Item = &'a str>,
    protected: &[(usize, usize)],
) -> Vec<Detection> {
    let mut detections = Vec::new();
    for value in values {
        if value.is_empty() {
            continue;
        }
        let mut offset = 0;
        while let Some(relative) = text[offset..].find(value) {
            let start = offset + relative;
            let end = start + value.len();
            if !inside_placeholder(start, end, protected) {
                detections.push(Detection {
                    start,
                    end,
                    rule: "known-value",
                    label: "known_secret",
                });
            }
            offset = start + value.chars().next().expect("nonempty value").len_utf8();
        }
    }
    detections
}

impl SecretVault {
    fn placeholder_for(&mut self, value: &str, _label: &str) -> String {
        if let Some(existing) = self.by_value.get(value) {
            return existing.clone();
        }
        self.next = self.next.saturating_add(1);
        let placeholder = format!("<pi-secret:{:06x}>", self.next);
        self.by_value.insert(value.to_string(), placeholder.clone());
        self.by_placeholder
            .insert(placeholder.clone(), value.to_string());
        placeholder
    }

    fn protected_placeholders(&self, text: &str) -> Vec<(usize, usize)> {
        placeholder_pattern()
            .find_iter(text)
            .filter(|found| self.by_placeholder.contains_key(found.as_str()))
            .map(|found| (found.start(), found.end()))
            .collect()
    }

    /// Restore placeholders from the input exactly once. A stored value may
    /// itself contain placeholder-shaped text; it is data, not a second
    /// substitution program. Unknown/malformed placeholders remain unchanged.
    #[must_use]
    pub fn restore(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut cursor = 0;
        for found in placeholder_pattern().find_iter(text) {
            out.push_str(&text[cursor..found.start()]);
            out.push_str(
                self.by_placeholder
                    .get(found.as_str())
                    .map_or(found.as_str(), String::as_str),
            );
            cursor = found.end();
        }
        out.push_str(&text[cursor..]);
        out
    }

    fn mask_replacements(&self) -> MaskReplacements<'_> {
        // Exact raw values take precedence if one secret's JSON spelling is
        // another independently registered raw value.
        let mut replacements: MaskReplacements<'_> = self
            .by_value
            .iter()
            .map(|(value, placeholder)| {
                (Cow::Borrowed(value.as_str()), placeholder.as_str())
            })
            .collect();
        for (value, placeholder) in &self.by_value {
            // JSON string serialization cannot fail. Retain only the string
            // contents, so quotes surrounding an export field stay intact.
            let encoded = serde_json::to_string(value).expect("serialize secret string");
            let escaped = &encoded[1..encoded.len() - 1];
            if escaped != value {
                replacements
                    .entry(Cow::Owned(escaped.to_string()))
                    .or_insert(placeholder.as_str());
            }
        }
        replacements
    }

    fn mask_literal_text(&self, text: &str, replacements: &MaskReplacements<'_>) -> String {
        let protected = self.protected_placeholders(text);
        let detections = merge_detections(literal_detections(
            text,
            replacements.keys().map(|value| value.as_ref()),
            &protected,
        ));
        let mut out = String::with_capacity(text.len());
        let mut cursor = 0;
        for detection in detections {
            out.push_str(&text[cursor..detection.start]);
            let value = &text[detection.start..detection.end];
            out.push_str(
                replacements
                    .get(value)
                    .copied()
                    .unwrap_or(OVERLAP_REDACTION),
            );
            cursor = detection.end;
        }
        out.push_str(&text[cursor..]);
        out
    }

    fn mask_json_value(
        &self,
        value: &mut serde_json::Value,
        replacements: &MaskReplacements<'_>,
    ) -> bool {
        match value {
            serde_json::Value::String(text) => {
                let masked = self.mask_literal_text(text, replacements);
                let changed = masked != *text;
                *text = masked;
                changed
            }
            serde_json::Value::Array(items) => {
                let mut changed = false;
                for item in items {
                    changed |= self.mask_json_value(item, replacements);
                }
                changed
            }
            serde_json::Value::Object(map) => {
                let mut changed = false;
                for (key, mut item) in std::mem::take(map) {
                    let masked_key = self.mask_literal_text(&key, replacements);
                    changed |= masked_key != key;
                    changed |= self.mask_json_value(&mut item, replacements);
                    map.insert(masked_key, item);
                }
                changed
            }
            other => {
                // A remembered numeric token may be echoed as a JSON number.
                // Preserve nonsecret primitives; secret ones become strings
                // holding placeholders rather than disclosing their value.
                if let Some(placeholder) = self.by_value.get(&other.to_string()) {
                    *other = serde_json::Value::String(placeholder.clone());
                    true
                } else {
                    false
                }
            }
        }
    }

    fn mask_json_document(
        &self,
        text: &str,
        replacements: &MaskReplacements<'_>,
    ) -> Option<String> {
        if !matches!(text.trim_start().chars().next()?, '{' | '[' | '"') {
            return None;
        }
        let mut value: serde_json::Value = serde_json::from_str(text).ok()?;
        if let Some(placeholder) = self.by_value.get(text.trim()) {
            value = serde_json::Value::String(placeholder.clone());
        } else if !self.mask_json_value(&mut value, replacements) {
            // Screening must not reformat clean structured data.
            return Some(text.to_string());
        }
        Some(serde_json::to_string(&value).expect("serialize masked JSON value"))
    }

    fn mask_json_lines(
        &self,
        text: &str,
        replacements: &MaskReplacements<'_>,
    ) -> Option<String> {
        let mut output = String::with_capacity(text.len());
        let mut saw_document = false;
        for line in text.split_inclusive('\n') {
            let (body, ending) = if let Some(body) = line.strip_suffix("\r\n") {
                (body, "\r\n")
            } else if let Some(body) = line.strip_suffix('\n') {
                (body, "\n")
            } else {
                (line, "")
            };
            if body.trim().is_empty() {
                output.push_str(line);
                continue;
            }
            // Only take the line-oriented route when every nonblank line is
            // structured JSON. Otherwise mask the full text, so multiline
            // plaintext credentials are never split into unmatchable pieces.
            output.push_str(&self.mask_json_document(body, replacements)?);
            output.push_str(ending);
            saw_document = true;
        }
        saw_document.then_some(output)
    }

    /// Mask remembered values, including JSON-escaped multiline credentials.
    /// JSON documents/JSONL are transformed structurally so escaped quotes,
    /// backslashes, object keys and control characters cannot corrupt the
    /// export or make a caller discard the redaction on a parse failure.
    ///
    /// Literal matches are unioned against the original input, never against
    /// generated placeholders. Unregistered overlapping combinations use an
    /// explicit non-restorable redaction marker rather than a wrong credential.
    #[must_use]
    pub fn mask(&self, text: &str) -> String {
        if self.by_value.is_empty() {
            return text.to_string();
        }
        let replacements = self.mask_replacements();
        if let Some(masked) = self.mask_json_document(text, &replacements) {
            return masked;
        }
        if text.contains('\n')
            && let Some(masked) = self.mask_json_lines(text, &replacements)
        {
            return masked;
        }
        self.mask_literal_text(text, &replacements)
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
    /// Number of distinct redacted regions, not the number of overlapping rules.
    pub detections: usize,
    pub rules: Vec<String>,
}

/// Replace newly detected AND remembered values with stable placeholders.
/// Learn detections first so a bare echo earlier in the same text is protected
/// too. Remembered values remain protected after their assignment/type hint
/// disappears from later context, including after compaction.
pub fn obfuscate(
    text: &str,
    vault: &mut SecretVault,
    extra_patterns: &[regex::Regex],
) -> (String, TransformAudit) {
    let protected = vault.protected_placeholders(text);
    let mut detections = scan(text, extra_patterns);
    detections.retain(|detection| {
        !inside_placeholder(detection.start, detection.end, &protected)
    });
    for detection in &detections {
        let _ = vault.placeholder_for(&text[detection.start..detection.end], detection.label);
    }
    detections.extend(literal_detections(
        text,
        vault.by_value.keys().map(String::as_str),
        &protected,
    ));
    let detections = merge_detections(detections);
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    let mut rules_hit: Vec<String> = Vec::new();
    for detection in &detections {
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
    fn generic_assignment_vaults_only_the_value() {
        let mut vault = SecretVault::default();
        let (out, _) = obfuscate("API_KEY=hunter2hunter2hunter2", &mut vault, &[]);
        assert!(
            out.starts_with("API_KEY=<pi-secret:"),
            "key name must survive, only the value is vaulted: {out}"
        );
        // Restore of a model-written command must expand to the bare value.
        let restored = vault.restore("export TOKEN=<pi-secret:000001>");
        assert_eq!(restored, "export TOKEN=hunter2hunter2hunter2");
        // Echo hygiene must catch the bare value too.
        assert_eq!(vault.mask("hunter2hunter2hunter2"), "<pi-secret:000001>");
        // A whole-match rule (DSN) still vaults the full credential.
        let (dsn_out, _) = obfuscate("postgres://user:pw@host/db", &mut vault, &[]);
        assert!(dsn_out.starts_with("<pi-secret:"), "{dsn_out}");
    }

    /// Synthetic credential shapes per provider (never real keys), each with
    /// the "plain" form and — where the provider issues them — the dotted
    /// form that #211 reported slipping through. `expect` names the rule
    /// that must fire.
    #[test]
    fn detector_matrix_positive_shapes() {
        let cases: &[(&str, &str, &str)] = &[
            (
                "openai plain",
                "sk-proj-abcdefghijklmnopqrstuvwxyz0123",
                "openai-key",
            ),
            (
                "openai dotted (BaiLian sk-sp)",
                "sk-sp-H.EEDDM.JOZh.MEQ.aBcDeFgHiJ.kLmNoPqRsTuV.wXyZ01234",
                "openai-key",
            ),
            (
                "openai dotted short segments",
                "sk-ab.cd.ef.gh.ij.kl.mn.op.qr",
                "openai-key",
            ),
            (
                "anthropic",
                "sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWxYz_0123456789-AbCdEfGh",
                "anthropic-key",
            ),
            (
                "github classic",
                "ghp_AbCdEfGhIjKlMnOpQrStUvWxYz0123456789",
                "github-pat",
            ),
            (
                "github oauth",
                "gho_AbCdEfGhIjKlMnOpQrStUvWxYz0123456789",
                "github-pat",
            ),
            (
                "github fine-grained",
                "github_pat_11ABCDEFG0abcdefghijklmn_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij",
                "github-pat-fine",
            ),
            ("aws", concat!("AKIA", "IOSFODNN7EXAMPLE"), "aws-access-key"),
            (
                "google",
                "AIzaSyA-bCdEfGhIjKlMnOpQrStUvWxYz0123456",
                "google-api-key",
            ),
            (
                "slack bot",
                "xoxb-1234567890-1234567890123-AbCdEfGhIjKlMnOpQrStUvWx",
                "slack-token",
            ),
            (
                "jwt",
                "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c",
                "jwt",
            ),
            (
                "private key header",
                concat!("-----BEGIN ", "RSA PRIVATE KEY-----"),
                "private-key",
            ),
            (
                "dsn dotted password",
                "postgres://svc:p.ass.word@db.internal:5432/prod",
                "dsn",
            ),
            (
                "generic plain",
                "API_KEY=hunter2hunter2hunter2",
                "generic-assignment",
            ),
            (
                "generic dotted",
                "apiKey: \"H.EEDDM1.JOZh.MEQ.aBcDeFgHiJ.kLmNoPqRsTuV\"",
                "generic-assignment",
            ),
            (
                "generic dotted, digit only in the last segment",
                "token=abcdefgh.ijklmnop.qrstuvwxyz.42",
                "generic-assignment",
            ),
        ];
        for (name, text, expect) in cases {
            let hits = scan(text, &[]);
            assert!(
                hits.iter().any(|d| d.rule == *expect),
                "{name}: expected rule {expect} to fire on {text:?}, got {:?}",
                hits.iter().map(|d| d.rule).collect::<Vec<_>>()
            );
        }
    }

    /// Shapes that must NOT be vaulted: prose, hostnames, version strings,
    /// the vault's own placeholders, and too-short dotted values.
    #[test]
    fn detector_matrix_negative_shapes() {
        let cases: &[&str] = &[
            "sk-foo.example.com",
            "sk-8 is the Skoda model. Not a key.",
            "sk-",
            "sk-abc.def",
            "<pi-secret:000001>",
            "restored <pi-secret:00000a> and <pi-secret:0000ff> fine",
            "version: 1.2.3.4",
            "token: docs.example.com",
            // Dotted identifier paths in code (no digits) are not vaulted.
            "apiKey: process.env.OPENAI_API_KEY",
            "const token = process.env.GITHUB_TOKEN;",
            "password: self.config.database.password",
            "secret = settings.integrations.slack.signing_secret",
            "password: correct horse battery staple",
            "eyJhbGciOiJIUzI1NiJ9 alone is not a jwt",
            "let x = a.b.c.d.e.f.g.h.i.j.k.l.m.n.o.p;",
        ];
        for text in cases {
            let hits = scan(text, &[]);
            assert!(
                hits.is_empty(),
                "false positive on {text:?}: {:?}",
                hits.iter()
                    .map(|d| (d.rule, &text[d.start..d.end]))
                    .collect::<Vec<_>>()
            );
        }
    }

    /// A key at the end of a sentence must not swallow the closing period,
    /// and the vaulted value must be exactly the key.
    #[test]
    fn dotted_keys_do_not_capture_trailing_punctuation() {
        let key = "sk-sp-H.EEDDM.JOZh.MEQ.aBcDeFgHiJ.kLmNoPqRsTuV";
        for (text, expected_tail) in [
            (format!("the key is {key}."), "."),
            (format!("the key is {key}..."), "..."),
            (format!("the key is {key}.\nnext line"), ".\nnext line"),
            (format!("(\"{key}\")"), "\")"),
        ] {
            let hits = scan(&text, &[]);
            assert_eq!(hits.len(), 1, "{text:?}: {hits:?}");
            assert_eq!(&text[hits[0].start..hits[0].end], key, "{text:?}");
            assert!(text[hits[0].end..].starts_with(expected_tail), "{text:?}");
        }
        // Generic assignment: the value ends on the last alphanumeric.
        let text = "password: H.EEDDM1.JOZh.MEQ.aBcDeFgHiJ.kLmNoPqRsTuV.";
        let hits = scan(text, &[]);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(
            &text[hits[0].start..hits[0].end],
            "H.EEDDM1.JOZh.MEQ.aBcDeFgHiJ.kLmNoPqRsTuV"
        );
    }

    /// Round trip for a dotted key: outbound placeholder, inbound restore,
    /// echo re-mask, and the placeholder itself is never re-detected.
    #[test]
    fn dotted_key_round_trips_through_the_vault() {
        let key = "sk-sp-H.EEDDM.JOZh.MEQ.aBcDeFgHiJ.kLmNoPqRsTuV.wXyZ01234";
        let mut vault = SecretVault::default();
        let (out, audit) = obfuscate(&format!("\"apiKey\": \"{key}\""), &mut vault, &[]);
        assert_eq!(out, "\"apiKey\": \"<pi-secret:000001>\"", "{out}");
        assert_eq!(audit.detections, 1, "one detection, not one per segment");
        assert!(
            scan(&out, &[]).is_empty(),
            "placeholder must not be re-detected"
        );
        assert_eq!(
            vault.restore("export KEY=<pi-secret:000001>"),
            format!("export KEY={key}")
        );
        assert_eq!(
            vault.mask(&format!("echo {key}")),
            "echo <pi-secret:000001>"
        );
        // The digit-substituted variant is a different value → a new slot.
        let (again, _) = obfuscate(&key.replace('.', "1"), &mut vault, &[]);
        assert_eq!(again, "<pi-secret:000002>");
        assert_eq!(vault.len(), 2);
    }

    #[test]
    fn overlapping_hits_collapse_cleanly() {
        let mut vault = SecretVault::default();
        let (out, audit) = obfuscate("api_key=sk-aaaaaaaaaaaaaaaaaaaaaaaa", &mut vault, &[]);
        assert_eq!(out, "api_key=<pi-secret:000001>");
        assert_eq!(audit.detections, 1);
    }

    #[test]
    fn complete_private_keys_are_one_restorable_region() {
        for label in [
            "PRIVATE KEY",
            "RSA PRIVATE KEY",
            "EC PRIVATE KEY",
            "DSA PRIVATE KEY",
            "OPENSSH PRIVATE KEY",
            "ENCRYPTED PRIVATE KEY",
        ] {
            for newline in ["\n", "\r\n", "\\n"] {
                let key = format!(
                    "-----BEGIN {label}-----{newline}Proc-Type: 4,ENCRYPTED{newline}DEK-Info: AES-256-CBC,0123456789ABCDEF{newline}U1lOVEhFVElDLUtFWS1CT0RZ{newline}-----END {label}-----"
                );
                let input = format!("before\n{key}\nafter");
                let hits = scan(&input, &[]);
                assert_eq!(hits.len(), 1, "{label}, {newline:?}");
                assert_eq!(&input[hits[0].start..hits[0].end], key);
                assert_eq!(hits[0].rule, "private-key");
                let mut vault = SecretVault::default();
                let (masked, audit) = obfuscate(&input, &mut vault, &[]);
                assert_eq!(masked, "before\n<pi-secret:000001>\nafter");
                assert_eq!(audit.detections, 1);
                assert_eq!(vault.restore(&masked), input);
                assert_eq!(vault.mask(&input), masked);
                assert_eq!(obfuscate(&input, &mut vault, &[]).0, masked);
                assert!(gate_outbound(&input, SecretsMode::Block, &[]).is_err());
            }
        }
    }

    #[test]
    fn truncated_or_mismatched_private_key_envelopes_fail_closed() {
        for suffix in [
            "",
            "\n-----END PUBLIC KEY-----\nMORE-PRIVATE-MATERIAL",
            "\n-----END EC PRIVATE KEY-----\nMORE-PRIVATE-MATERIAL",
            "\n-----BEGIN PRIVATE KEY-----\nNESTED-PRIVATE-MATERIAL",
        ] {
            let input = format!(
                "safe prefix\n-----BEGIN RSA PRIVATE KEY-----\nPRIVATE-BODY{suffix}"
            );
            let hits = scan(&input, &[]);
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].end, input.len());
            let mut vault = SecretVault::default();
            let (masked, _) = obfuscate(&input, &mut vault, &[]);
            assert_eq!(masked, "safe prefix\n<pi-secret:000001>");
            assert_eq!(vault.restore(&masked), input);
        }
    }

    #[test]
    fn separate_pem_blocks_do_not_swallow_intervening_safe_text() {
        let first = "-----BEGIN PRIVATE KEY-----\nFIRST-BODY\n-----END PRIVATE KEY-----";
        let second = "-----BEGIN EC PRIVATE KEY-----\nSECOND-BODY\n-----END EC PRIVATE KEY-----";
        let input = format!("{first}\nsafe separator\n{second}\nsafe tail");
        let mut vault = SecretVault::default();
        let (masked, audit) = obfuscate(&input, &mut vault, &[]);
        assert_eq!(
            masked,
            "<pi-secret:000001>\nsafe separator\n<pi-secret:000002>\nsafe tail"
        );
        assert_eq!(audit.detections, 2);
        assert_eq!(vault.restore(&masked), input);
        for public in [
            "-----BEGIN PUBLIC KEY-----\nPUBLIC-BODY\n-----END PUBLIC KEY-----",
            "-----BEGIN CERTIFICATE-----\nPUBLIC-CERT\n-----END CERTIFICATE-----",
        ] {
            assert!(scan(public, &[]).is_empty());
        }
    }

    #[test]
    fn overlapping_user_patterns_redact_the_entire_transitive_union() {
        let input = "prefix abcdefghij suffix";
        for patterns in [["abcde", "defgh", "ghij"], ["ghij", "defgh", "abcde"]] {
            let patterns: Vec<_> = patterns
                .into_iter()
                .map(|pattern| regex::Regex::new(pattern).unwrap())
                .collect();
            let hits = scan(input, &patterns);
            assert_eq!(hits.len(), 1);
            assert_eq!(&input[hits[0].start..hits[0].end], "abcdefghij");
            let mut vault = SecretVault::default();
            let (masked, audit) = obfuscate(input, &mut vault, &patterns);
            assert_eq!(masked, "prefix <pi-secret:000001> suffix");
            assert_eq!(audit.detections, 1);
            assert_eq!(vault.restore(&masked), input);
        }
    }

    #[test]
    fn same_start_short_matches_cannot_hide_longer_or_adjacent_matches() {
        let patterns = ["abc", "abcdef", "bcde", "XYZ"]
            .map(|pattern| regex::Regex::new(pattern).unwrap());
        let input = "α abcdefXYZ ω";
        let hits = scan(input, &patterns);
        assert_eq!(hits.len(), 2);
        assert_eq!(&input[hits[0].start..hits[0].end], "abcdef");
        assert_eq!(&input[hits[1].start..hits[1].end], "XYZ");
        let mut vault = SecretVault::default();
        let (masked, _) = obfuscate(input, &mut vault, &patterns);
        assert_eq!(masked, "α <pi-secret:000001><pi-secret:000002> ω");
        assert_eq!(vault.restore(&masked), input);
    }

    #[test]
    fn zero_width_user_matches_are_not_credentials() {
        let patterns = ["", "^", "$", "SENSITIVE"]
            .map(|pattern| regex::Regex::new(pattern).unwrap());
        assert!(scan("", &patterns).is_empty());
        assert!(scan("ordinary text", &patterns).is_empty());
        assert!(gate_outbound("ordinary text", SecretsMode::Block, &patterns).is_ok());
        let mut vault = SecretVault::default();
        let (masked, audit) = obfuscate("before SENSITIVE after", &mut vault, &patterns);
        assert_eq!(masked, "before <pi-secret:000001> after");
        assert_eq!(audit.detections, 1);
    }

    #[test]
    fn quoted_configuration_keys_vault_only_the_credential_value() {
        let secret = "hunter2hunter2hunter2";
        for key in ["apiKey", "API_KEY", "secret", "token", "password", "passwd", "pwd"] {
            for quote in ["\"", "'", ""] {
                let input = format!("{quote}{key}{quote}: \"{secret}\"");
                let hits = scan(&input, &[]);
                assert_eq!(hits.len(), 1);
                assert_eq!(&input[hits[0].start..hits[0].end], secret);
                let mut vault = SecretVault::default();
                let (masked, _) = obfuscate(&input, &mut vault, &[]);
                assert_eq!(masked, format!("{quote}{key}{quote}: \"<pi-secret:000001>\""));
                assert_eq!(vault.restore(&masked), input);
            }
        }
        for input in [
            "\"apiKey\": \"process.env.OPENAI_API_KEY\"",
            "'password': 'self.config.database.password'",
            "\"token\": \"docs.example.com\"",
        ] {
            assert!(scan(input, &[]).is_empty());
        }
    }

    #[test]
    fn discovery_protects_bare_echoes_before_and_after_the_assignment() {
        let secret = "hunter2hunter2hunter2";
        let input = format!("echo {secret}; API_KEY={secret}; again {secret}");
        let mut vault = SecretVault::default();
        let (masked, audit) = obfuscate(&input, &mut vault, &[]);
        assert_eq!(
            masked,
            "echo <pi-secret:000001>; API_KEY=<pi-secret:000001>; again <pi-secret:000001>"
        );
        assert_eq!(audit.detections, 3);
        assert_eq!(vault.len(), 1);
        assert_eq!(vault.restore(&masked), input);
        let (later, audit) = obfuscate(&format!("only {secret} remains"), &mut vault, &[]);
        assert_eq!(later, "only <pi-secret:000001> remains");
        assert_eq!(audit.detections, 1);
        assert_eq!(audit.rules, ["known_secret"]);
    }

    #[test]
    fn restore_does_not_interpret_inserted_values_as_more_placeholders() {
        let mut vault = SecretVault::default();
        let first_value = "A:<pi-secret:000002>";
        let second_value = "B:<pi-secret:000001>";
        let first = vault.placeholder_for(first_value, "user");
        let second = vault.placeholder_for(second_value, "user");
        assert_eq!(
            vault.restore(&format!("{first}|{second}")),
            format!("{first_value}|{second_value}")
        );
        let unknown = "<pi-secret:ffffff> <pi-secret:bad> <pi-secret:000001";
        assert_eq!(vault.restore(unknown), unknown);
    }

    #[test]
    fn known_placeholders_are_not_rewritten_by_literal_or_custom_matches() {
        let mut vault = SecretVault::default();
        let pattern = regex::Regex::new("SENSITIVE|000001").unwrap();
        let patterns = [pattern];
        let (masked, _) = obfuscate("SENSITIVE", &mut vault, &patterns);
        assert_eq!(masked, "<pi-secret:000001>");
        let (again, audit) = obfuscate(&masked, &mut vault, &patterns);
        assert_eq!(again, masked);
        assert_eq!(audit.detections, 0);
        let _ = vault.placeholder_for("000001", "user");
        assert_eq!(vault.mask(&masked), masked);
    }

    #[test]
    fn json_masking_preserves_structure_and_escaped_secret_values() {
        let mut vault = SecretVault::default();
        let secret = "line one\r\nquote=\"x\"; path=C:\\keys\\value\tend";
        let placeholder = vault.placeholder_for(secret, "user");
        let input = serde_json::json!({
            "nested": [{"credential": secret, "safe": 42}],
            "clean": "unchanged",
            "keyed": {secret: "kept"}
        });
        let encoded = serde_json::to_string_pretty(&input).unwrap();
        let masked = vault.mask(&encoded);
        let decoded: serde_json::Value = serde_json::from_str(&masked).unwrap();
        assert_eq!(decoded["nested"][0]["credential"], placeholder);
        assert_eq!(decoded["nested"][0]["safe"], 42);
        assert_eq!(decoded["clean"], "unchanged");
        assert_eq!(decoded["keyed"][&placeholder], "kept");
        assert!(!masked.contains("line one"));
        assert!(!masked.contains("keys"));
        let clean = " { \"safe\" : 42, \"text\" : \"unchanged\" } \n";
        assert_eq!(vault.mask(clean), clean);
        assert_eq!(vault.mask(&masked), masked);
    }

    #[test]
    fn json_lines_masking_keeps_record_boundaries_and_safe_primitives() {
        let mut vault = SecretVault::default();
        let secret = "first line\nsecond line";
        let placeholder = vault.placeholder_for(secret, "user");
        let numeric = vault.placeholder_for("1234567890123456", "user");
        let first = serde_json::json!({"content": secret, "safe": true}).to_string();
        let second = serde_json::json!({"token": 1234567890123456_u64, "safe": 7}).to_string();
        let input = format!("{first}\r\n\r\n{second}\n");
        let masked = vault.mask(&input);
        assert!(masked.contains("\r\n\r\n"));
        assert!(masked.ends_with('\n'));
        let records: Vec<serde_json::Value> = masked
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["content"], placeholder);
        assert_eq!(records[0]["safe"], true);
        assert_eq!(records[1]["token"], numeric);
        assert_eq!(records[1]["safe"], 7);
    }

    #[test]
    fn prefixed_logs_and_plain_multiline_values_both_mask_without_cascading() {
        let mut vault = SecretVault::default();
        let secret = "α secret\nβ secret";
        let placeholder = vault.placeholder_for(secret, "user");
        let encoded = serde_json::to_string(secret).unwrap();
        assert_eq!(vault.mask(&format!("log={encoded}")), format!("log=\"{placeholder}\""));
        assert_eq!(vault.mask(&format!("before\n{secret}\nafter")), format!("before\n{placeholder}\nafter"));
    }

    #[test]
    fn overlapping_remembered_values_never_expose_a_suffix_or_restore_the_wrong_key() {
        let mut vault = SecretVault::default();
        let _ = vault.placeholder_for("abcdef", "user");
        let _ = vault.placeholder_for("defghi", "user");
        assert_eq!(vault.mask("safe abcdefghi safe"), format!("safe {OVERLAP_REDACTION} safe"));
        let (masked, audit) = obfuscate("safe abcdefghi safe", &mut vault, &[]);
        assert_eq!(masked, "safe <pi-secret:000003> safe");
        assert_eq!(audit.detections, 1);
        assert_eq!(vault.restore(&masked), "safe abcdefghi safe");
        assert_eq!(vault.mask("safe abcdefghi safe"), masked);
    }

    #[test]
    fn repeated_overlapping_literals_cover_the_last_match_too() {
        let mut vault = SecretVault::default();
        let _ = vault.placeholder_for("aaa", "user");
        let (masked, _) = obfuscate("aaaa", &mut vault, &[]);
        assert_eq!(masked, "<pi-secret:000002>");
        assert_eq!(vault.restore(&masked), "aaaa");
        assert_eq!(vault.mask("aaaa"), masked);
    }

    #[test]
    fn vault_debug_never_prints_values_or_the_raw_value_map() {
        let mut vault = SecretVault::default();
        let _ = vault.placeholder_for("private-debug-canary", "user");
        let debug = format!("{vault:?}");
        assert!(debug.contains("entries: 1"));
        assert!(!debug.contains("private-debug-canary"));
        assert!(!debug.contains("<pi-secret:"));
        assert!(!debug.contains("by_value"));
    }
}
