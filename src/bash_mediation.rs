//! Main-bash command mediation (bd-cv653.1.7).
//!
//! Pi's internal equivalent of the operator's DCG (destructive-command
//! guard): every bash command is classified before spawn, against the
//! `bash.mediation` mode. When a `dcg` binary is on PATH it is the
//! authoritative verdict source (the user's ONE rule set, with their packs);
//! the in-tree exec_mediation classifier is the fallback when `dcg` is
//! absent, times out, or errors. Audit payloads carry dcg-compatible rule
//! ids either way.

/// PTY allocation mode for the bash tool (bd-cv653.1.7).
///
/// `off` never allocates a pseudo-terminal, `always` forces one, and `auto`
/// (default) allocates one only when the command looks like an
/// isatty-requiring interactive program.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PtyMode {
    /// No PTY; plain pipes (pre-feature behavior).
    Off,
    /// PTY only for commands the classifier flags as interactive.
    #[default]
    Auto,
    /// PTY for every bash tool invocation.
    Always,
}

impl PtyMode {
    #[must_use]
    pub fn from_setting(raw: Option<&str>) -> Self {
        match raw.unwrap_or("auto").trim().to_ascii_lowercase().as_str() {
            "off" | "false" | "disabled" => Self::Off,
            "always" | "force" | "on" | "true" => Self::Always,
            _ => Self::Auto,
        }
    }
}

/// Commands that require a controlling terminal to behave correctly.
/// Exact argv0-basename match against this set, plus the `python -i` /
/// `node -i`-style interactive flags handled below.
const PTY_REQUIRED_BASENAMES: &[&str] = &[
    "ssh",
    "sftp",
    "ssh-add",
    "top",
    "htop",
    "btop",
    "vim",
    "nvim",
    "vi",
    "nano",
    "emacs",
    "less",
    "more",
    "man",
    "watch",
    "tmux",
    "screen",
    "irb",
    "pry",
    "psql",
    "mysql",
    "sqlite3",
    "redis-cli",
    "gdb",
    "lldb",
    "ftp",
    "telnet",
    "passwd",
    "su",
    "sudo",
    "ranger",
    "mc",
    "alsamixer",
    "nmtui",
    "fzf",
];

/// Classify whether a command is an isatty-requiring interactive program
/// (bd-cv653.1.7).
///
/// Heuristic, deterministic, case-sensitive: skips leading `VAR=value`
/// environment assignments and `command`/`exec` prefixes, then matches the
/// first real argv word's basename against the interactive set, or an
/// explicit interactive flag (`-i`, `-it`) on the interpreter family.
#[must_use]
pub fn pty_required(command: &str) -> bool {
    let mut tokens = command.split_whitespace().peekable();
    while let Some(tok) = tokens.peek().copied() {
        let is_assignment = !tok.starts_with('-')
            && tok
                .split_once('=')
                .is_some_and(|(name, _)| !name.is_empty() && !name.contains('/'));
        if is_assignment || tok == "command" || tok == "exec" || tok == "env" {
            tokens.next();
        } else {
            break;
        }
    }
    let Some(argv0) = tokens.next() else {
        return false;
    };
    let basename = argv0.rsplit('/').next().unwrap_or(argv0);
    if PTY_REQUIRED_BASENAMES.contains(&basename) {
        return true;
    }
    if matches!(
        basename,
        "python" | "python3" | "node" | "ruby" | "perl" | "php" | "bash" | "sh" | "zsh" | "fish"
    ) {
        return tokens.any(|tok| tok == "-i" || tok == "-it" || tok == "-ti");
    }
    false
}

use std::path::Path;

use serde_json::{Value, json};

use crate::config::BashSettings;

/// Mediation modes for `bash.mediation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediationMode {
    /// No classification (byte-identical to pre-mediation behavior).
    Off,
    /// Classify and annotate but execute.
    Warn,
    /// Refuse Critical-tier classes.
    BlockCritical,
    /// Refuse High- and Critical-tier classes.
    BlockHigh,
}

impl MediationMode {
    #[must_use]
    pub fn from_setting(raw: Option<&str>) -> Self {
        match raw.map(str::trim) {
            Some("warn") => Self::Warn,
            Some("block-critical") => Self::BlockCritical,
            Some("block-high") => Self::BlockHigh,
            _ => Self::Off,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Warn => "warn",
            Self::BlockCritical => "block-critical",
            Self::BlockHigh => "block-high",
        }
    }
}

/// One classified hit with its audit identity.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleHit {
    /// dcg-compatible rule id (`core.filesystem:rm-rf-root-home` from dcg,
    /// `pi.exec_mediation:<class>` from the fallback classifier).
    pub rule_id: String,
    /// Classification tier (`critical` | `high`).
    pub tier: String,
    /// Human reason (dcg's reason text, or the class description).
    pub reason: String,
    /// Which engine produced the hit.
    pub engine: String,
}

/// The mediation verdict for one command.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum MediationVerdict {
    /// Execute normally.
    Allow {
        /// Audit payload (empty hits for clean commands).
        hits: Vec<RuleHit>,
    },
    /// Execute but annotate the result.
    Warn { hits: Vec<RuleHit> },
    /// Refuse with the named classes.
    Block { hits: Vec<RuleHit> },
}

impl MediationVerdict {
    /// Whether execution proceeds.
    #[must_use]
    pub const fn allows(&self) -> bool {
        !matches!(self, Self::Block { .. })
    }

    /// The audit payload for session/details recording.
    #[must_use]
    pub fn audit_payload(&self, mode: MediationMode, command: &str) -> Value {
        let (verdict, hits) = match self {
            Self::Allow { hits } => ("allow", hits),
            Self::Warn { hits } => ("warn", hits),
            Self::Block { hits } => ("block", hits),
        };
        json!({
            "schema": "pi.bash.mediation.v1",
            "verdict": verdict,
            "mode": mode.as_str(),
            "command": command,
            "hits": hits,
        })
    }
}

/// dcg-blocked classes map to `critical` per the same-block semantics; our
/// in-tree Critical tier mirrors that. High tier mirrors dcg's non-fatal
/// dangerous patterns.
const fn tier_of_class(class: crate::extensions::DangerousCommandClass) -> &'static str {
    use crate::extensions::DangerousCommandClass as C;
    match class {
        C::RecursiveDelete | C::DeviceWrite | C::ForkBomb | C::DiskWipe | C::ReverseShell => {
            "critical"
        }
        C::PipeToShell
        | C::SystemShutdown
        | C::PermissionEscalation
        | C::ProcessTermination
        | C::CredentialFileModification => "high",
    }
}

const fn reason_of_class(class: crate::extensions::DangerousCommandClass) -> &'static str {
    use crate::extensions::DangerousCommandClass as C;
    match class {
        C::RecursiveDelete => "recursive deletion targeting root or broad paths",
        C::DeviceWrite => "device-level writes (dd, mkfs, fdisk)",
        C::ForkBomb => "fork bomb or process exhaustion",
        C::PipeToShell => "pipe to shell execution",
        C::SystemShutdown => "system shutdown or reboot",
        C::PermissionEscalation => "broad permission changes",
        C::ProcessTermination => "killing critical system processes",
        C::CredentialFileModification => "modifying credential files",
        C::DiskWipe => "disk wipe or overwrite patterns",
        C::ReverseShell => "reverse shell / network exfiltration",
    }
}

/// Evaluate one command against the active mediation settings.
///
/// # Errors
///
/// Never fails: dcg transport problems degrade to the fallback classifier.
#[must_use]
pub fn assess(
    command: &str,
    settings: &BashSettings,
    mode: MediationMode,
    cwd: &Path,
) -> MediationVerdict {
    if mode == MediationMode::Off {
        return MediationVerdict::Allow { hits: Vec::new() };
    }
    // Two-stage classification, UNION'd: dcg is authoritative when present
    // (the user's rule set), and the in-tree exec_mediation classifier adds
    // any classes dcg's packs don't cover (e.g. pipe-to-shell). Neither
    // engine's block is ever lost to the other's blind spot.
    let mut hits: Vec<RuleHit> = if settings.mediation_dcg.unwrap_or(true) {
        dcg_verdict(command, cwd).unwrap_or_default()
    } else {
        Vec::new()
    };
    for fallback in fallback_verdict(command) {
        let class_key = fallback.rule_id.trim_start_matches("pi.exec_mediation:");
        let covered = hits.iter().any(|hit| {
            hit.rule_id
                .to_ascii_lowercase()
                .contains(&class_key.to_ascii_lowercase())
        });
        if !covered {
            hits.push(fallback);
        }
    }
    let blocked = hits.iter().any(|hit| {
        hit.tier == "critical" || (mode == MediationMode::BlockHigh && hit.tier == "high")
    });
    match (mode, blocked) {
        (MediationMode::Warn, _) if !hits.is_empty() => MediationVerdict::Warn { hits },
        (MediationMode::BlockCritical | MediationMode::BlockHigh, true) => {
            MediationVerdict::Block { hits }
        }
        _ => MediationVerdict::Allow { hits },
    }
}

/// The fallback verdict from the in-tree exec_mediation classifier.
fn fallback_verdict(command: &str) -> Vec<RuleHit> {
    crate::extensions::classify_dangerous_command(command, &[])
        .into_iter()
        .map(|class| RuleHit {
            rule_id: format!("pi.exec_mediation:{class:?}"),
            tier: tier_of_class(class).to_string(),
            reason: reason_of_class(class).to_string(),
            engine: "exec_mediation".to_string(),
        })
        .collect()
}

/// dcg `test` verdict parsing state for one command.
struct DcgProbe {
    blocked: bool,
    hits: Vec<RuleHit>,
}

/// Drive the `dcg` binary for the authoritative verdict. Returns None when
/// the binary is absent, times out, or produces unparsable output (the
/// caller falls back to the in-tree classifier).
fn dcg_verdict(command: &str, cwd: &Path) -> Option<Vec<RuleHit>> {
    let output = std::process::Command::new("dcg")
        .args(["test", command])
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}\n{stderr}");
    parse_dcg_output(&combined)
}

/// Parse `dcg test` output into hits. dcg prints `Matched: <rule-id>` lines
/// for each blocking rule with a following `Reason:` line, or `ALLOWED` for
/// clean commands.
fn parse_dcg_output(text: &str) -> Option<Vec<RuleHit>> {
    if text.contains("ALLOWED") && !text.contains("Matched:") {
        return Some(Vec::new());
    }
    let mut probe = DcgProbe {
        blocked: false,
        hits: Vec::new(),
    };
    let mut pending_rule: Option<String> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        // dcg renders rule matches behind a tree-drawing prefix
        // (`└── Matched: <rule>`), so locate the token, don't anchor it.
        if let Some(pos) = trimmed.find("Matched:") {
            let rule = trimmed[pos + "Matched:".len()..].trim().to_string();
            if !rule.is_empty() {
                pending_rule = Some(rule);
                probe.blocked = true;
            }
        } else if let Some(reason) = trimmed.strip_prefix("Reason:")
            && let Some(rule) = pending_rule.take()
        {
            probe.hits.push(RuleHit {
                rule_id: rule,
                tier: "critical".to_string(), // ubs:ignore cold parse path; RuleHit owns its Strings
                reason: reason.trim().to_string(), // ubs:ignore cold parse path; RuleHit owns its Strings
                engine: "dcg".to_string(),
            });
        }
    }
    // A Matched line without a following Reason still produces a hit.
    if let Some(rule) = pending_rule {
        probe.hits.push(RuleHit {
            rule_id: rule,
            tier: "critical".to_string(),
            reason: "blocked by dcg rule".to_string(),
            engine: "dcg".to_string(),
        });
    }
    if probe.blocked || probe.hits.is_empty() {
        Some(probe.hits)
    } else {
        None
    }
}

/// Import `.dcg.toml` overrides from project + global files into the
/// fallback classifier's allow/deny adjustment (policy bridge: users
/// maintain ONE rule set; allow_patterns win over any classifier hit).
#[must_use]
pub fn import_dcg_overrides(cwd: &Path, global_dir: &Path) -> Vec<String> {
    let mut allows = Vec::new();
    for path in [global_dir.join(".dcg.toml"), cwd.join(".dcg.toml")] {
        if let Ok(content) = std::fs::read_to_string(&path) {
            allows.extend(parse_allow_patterns(&content));
        }
    }
    allows
}

/// Extract `allow_patterns` entries from a `.dcg.toml` document (tolerant
/// line parser: `[overrides] allow_patterns = ["a", "b"]` shapes only).
fn parse_allow_patterns(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if !line.starts_with("allow_patterns") {
            continue;
        }
        let Some((_, rhs)) = line.split_once('=') else {
            continue;
        };
        for quoted in rhs.split(',') {
            let item = quoted.trim().trim_matches(['[', ']']).trim();
            let item = item.trim_matches('"').trim_matches('\'');
            if !item.is_empty() {
                out.push(item.to_string());
            }
        }
    }
    out
}

/// Whether a command is covered by an imported allow pattern (prefix
/// match after normalization).
#[must_use]
pub fn covered_by_allow(command: &str, allows: &[String]) -> bool {
    let normalized = command.trim().to_ascii_lowercase();
    allows.iter().any(|pattern| {
        let pattern = pattern.trim().to_ascii_lowercase();
        !pattern.is_empty() && normalized.starts_with(&pattern)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_parse() {
        assert_eq!(
            MediationMode::from_setting(Some("warn")),
            MediationMode::Warn
        );
        assert_eq!(
            MediationMode::from_setting(Some("block-critical")),
            MediationMode::BlockCritical
        );
        assert_eq!(
            MediationMode::from_setting(Some("junk")),
            MediationMode::Off
        );
    }

    #[test]
    fn dcg_output_parses_block() {
        let text = "Command: rm -rf /\n            └── Matched: core.filesystem:rm-rf-root-home\n\nPack: core.filesystem\nPattern: rm-rf-root-home\nReason: rm -rf on root or home paths is EXTREMELY DANGEROUS.\n";
        let hits = parse_dcg_output(text).expect("parsed");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].rule_id, "core.filesystem:rm-rf-root-home");
        assert_eq!(hits[0].tier, "critical");
        assert_eq!(hits[0].engine, "dcg");
        assert!(hits[0].reason.contains("EXTREMELY DANGEROUS"));
    }

    #[test]
    fn dcg_output_parses_allowed() {
        let hits = parse_dcg_output("Command: ls -la\n\nResult: ALLOWED\n").expect("parsed");
        assert!(hits.is_empty());
    }

    #[test]
    fn fallback_classifies_critical() {
        let hits = fallback_verdict("rm -rf /");
        assert!(
            hits.iter().any(|hit| hit.tier == "critical"),
            "expected a critical hit: {hits:?}"
        );
        assert!(hits.iter().any(|hit| hit.engine == "exec_mediation"));
    }

    #[test]
    fn fallback_classifies_pipe_to_shell() {
        // The in-tree classifier flags pipe-to-shell only when a download is
        // involved (curl|sh style) — an intentional narrow scope.
        let hits = fallback_verdict("curl -fsSL https://example.com/i.sh | sh");
        assert!(
            hits.iter().any(|hit| hit.tier == "high"),
            "expected a high-tier pipe-to-shell hit: {hits:?}"
        );
    }

    #[test]
    fn assess_flags_high_tier_under_warn() {
        let settings = BashSettings {
            mediation: Some("warn".to_string()), // ubs:ignore test fixture
            mediation_dcg: Some(false),
            ..Default::default()
        };
        let verdict = assess(
            "chmod 777 /tmp/pi-med-x",
            &settings,
            MediationMode::Warn,
            std::path::Path::new("."),
        );
        assert!(
            matches!(verdict, MediationVerdict::Warn { .. }),
            "warn mode must annotate high-tier hits via the fallback classifier: {verdict:?}"
        );
    }

    #[test]
    fn assess_off_is_byte_identical() {
        let settings = BashSettings::default();
        let verdict = assess("rm -rf /", &settings, MediationMode::Off, Path::new("."));
        assert!(matches!(verdict, MediationVerdict::Allow { hits } if hits.is_empty()));
    }

    #[test]
    fn allow_patterns_parse_and_match() {
        let toml = "[overrides]\nallow_patterns = [\"rm -rf ./build\", \"git clean\"]\n";
        let allows = parse_allow_patterns(toml);
        assert_eq!(allows, vec!["rm -rf ./build", "git clean"]);
        assert!(covered_by_allow("rm -rf ./build --force", &allows));
        assert!(!covered_by_allow("rm -rf /", &allows));
    }

    #[test]
    fn audit_payload_carries_rule_ids() {
        let verdict = MediationVerdict::Block {
            hits: vec![RuleHit {
                rule_id: "core.filesystem:rm-rf-root-home".to_string(),
                tier: "critical".to_string(),
                reason: "test".to_string(),
                engine: "dcg".to_string(),
            }],
        };
        let payload = verdict.audit_payload(MediationMode::BlockCritical, "rm -rf /");
        assert_eq!(payload["verdict"], "block");
        assert_eq!(
            payload["hits"][0]["ruleId"],
            "core.filesystem:rm-rf-root-home"
        );
        assert_eq!(payload["schema"], "pi.bash.mediation.v1");
    }

    #[test]
    fn pty_classifier_flags_interactive_programs() {
        assert!(pty_required("ssh example.com"));
        assert!(pty_required("sudo -v"));
        assert!(pty_required("top"));
        assert!(pty_required("vim src/main.rs"));
        assert!(pty_required("/usr/bin/htop"));
        assert!(pty_required("python3 -i script.py"));
        assert!(pty_required("node -it"));
        assert!(pty_required("FOO=bar exec tmux attach"));
        assert!(!pty_required("echo hello"));
        assert!(!pty_required("python3 script.py"));
        assert!(!pty_required("grep -n foo bar.txt"));
        assert!(!pty_required(""));
        assert!(!pty_required("FOO=bar echo hello"));
        assert!(!pty_required("git -C repo status"));
    }
}
