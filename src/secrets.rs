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
pub const SECRETS_RULESET_VERSION: u32 = 3;

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
            // Generic KEY=value assignments with a high-entropy value:
            // 16+ token characters (dots allowed between them), no spaces,
            // ending on a base64/alphanumeric character.
            Rule {
                name: "generic-assignment",
                regex: regex::Regex::new(&format!(
                    r#"(?i)(?:api[_-]?key|secret|token|password|passwd|pwd)\s*[=:]\s*['"]?({})['"]?"#,
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
    /// Longest value first: when one vaulted value is a substring of
    /// another, masking the shorter one first would leave fragments of the
    /// longer secret exposed (HashMap order is arbitrary).
    #[must_use]
    pub fn mask(&self, text: &str) -> String {
        let mut pairs: Vec<(&String, &String)> = self.by_placeholder.iter().collect();
        pairs.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(b.0)));
        let mut out = text.to_string();
        for (placeholder, real) in pairs {
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
    /// Number of distinct redacted regions, not the number of overlapping rules.
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
}
