//! Secrets obfuscation vault (bd-cv653.7.9).
//!
//! Credential-shaped values are replaced with stable placeholders before
//! provider calls and restored on the way back: read .env → tool result →
//! provider payload never flows raw. The vault map lives in memory for the
//! session and dies with it — session files and compaction summaries carry
//! placeholders only.
//!
//! Detection: versioned pattern rules (sk-ant-*, sk-* including dotted
//! BaiLian-style keys, gh?_*, AKIA*, AIza*, xox[bap]-*, JWTs, private-key
//! headers, DSN/connection strings, generic KEY=value high-entropy
//! assignments) with a measured false-positive budget. This module is the
//! SINGLE detector for the whole program (bd-cv653.4.1's memory screener
//! composes with it).
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

/// Versioned in-tree rules (bump SECRETS_RULESET_VERSION when editing).
///
/// v2 (gh #211): provider keys may carry `.` inside the token (Alibaba
/// BaiLian issues `sk-sp-H.EEDDM.…` keys through the OpenAI-compatible
/// surface; JWTs are three dot-separated base64url segments). Dots are
/// accepted only *between* token characters — never leading, trailing, or
/// doubled — so a key at the end of a sentence does not swallow the period,
/// and dots do not count toward the minimum length, so `sk-a.b.c` shapes
/// stay below the bar. Rules are ordered most-specific first: overlapping
/// hits are resolved by start offset and then rule order, so the audit
/// label names the tightest rule.
pub const SECRETS_RULESET_VERSION: u32 = 2;

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
#[must_use]
pub fn scan(text: &str, extra_patterns: &[regex::Regex]) -> Vec<Detection> {
    let mut out = Vec::new();
    for rule in rules() {
        for caps in rule.regex.captures_iter(text) {
            // When a rule isolates the value in group 1 (the KEY=value
            // assignment shape), vault only the value: vaulting the whole
            // `KEY=value` corrupts inbound restores (`export X=KEY=value`)
            // and leaves a bare echo of the value unmasked.
            let m = caps.get(1).or_else(|| caps.get(0)).expect("match group 0");
            if rule.reject.is_some_and(|reject| reject(m.as_str())) {
                continue;
            }
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
        // The generic rule overlaps the specific sk- rule: one placeholder.
        let (out, audit) = obfuscate("api_key=sk-aaaaaaaaaaaaaaaaaaaaaaaa", &mut vault, &[]);
        assert!(out.contains("<pi-secret:"), "{out}");
        assert!(audit.detections >= 1);
    }
}
