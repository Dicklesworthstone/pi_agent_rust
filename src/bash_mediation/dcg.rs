//! The external guard is a protocol peer, not human-readable terminal output.
//!
//! `dcg test --format json` reports command/decision/rule_id/reason. Only an
//! explicit allow for this exact command and a successful process can allow.
//! An absent binary may use Pi's fallback; a present but broken guard must not
//! silently discard the operator's rule packs by falling back to fewer rules.

use super::RuleHit;
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};

const MAX_OUTPUT_BYTES: usize = 256 * 1024;

// Typed deserialization also rejects duplicate decision/command fields. Parsing
// into Value would silently accept the last duplicate, which is unsafe here.
#[derive(serde::Deserialize)]
struct DecisionRecord {
    command: String,
    decision: String,
    rule_id: Option<String>,
    reason: Option<String>,
    #[serde(default)]
    skipped_due_to_budget: bool,
}

fn hit(rule: &str, reason: &str) -> RuleHit {
    RuleHit {
        rule_id: rule.to_string(),
        tier: "critical".to_string(),
        reason: reason.to_string(),
        engine: "dcg".to_string(),
    }
}

fn unavailable(reason: &str) -> Vec<RuleHit> {
    vec![hit("pi.bash.mediation:dcg-unavailable", reason)]
}

pub(super) fn verdict(command: &str, cwd: &Path) -> Option<Vec<RuleHit>> {
    let output = Command::new("dcg")
        // The delimiter makes a candidate beginning with '-' data, never a
        // diagnostic CLI switch. The candidate is one argv item, not a shell.
        .args(["test", "--format", "json", "--", command])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    match output {
        Ok(output) => Some(parse_output(command, output.status.code(), &output.stdout)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        // Do not copy arbitrary stderr or OS error payloads into audit reasons:
        // guard diagnostics may contain environment values or command secrets.
        Err(_) => Some(unavailable("The configured dcg guard could not be executed")),
    }
}

pub(super) fn parse_output(command: &str, exit: Option<i32>, stdout: &[u8]) -> Vec<RuleHit> {
    if stdout.len() > MAX_OUTPUT_BYTES {
        return unavailable("dcg output exceeded the mediation byte limit");
    }
    let record = serde_json::from_slice::<DecisionRecord>(stdout);
    let Ok(record) = record else {
        // dcg's blocking exit is independently authoritative even if the
        // explanatory frame is missing or damaged. No other exit is an allow.
        return if exit == Some(1) {
            vec![hit(
                "pi.bash.mediation:dcg-blocked-exit",
                "dcg refused execution without a valid decision record",
            )]
        } else {
            unavailable("dcg returned no valid JSON decision record")
        };
    };
    if record.command != command {
        return unavailable("dcg decision did not match the command being assessed");
    }
    if record.skipped_due_to_budget || record.decision == "indeterminate" {
        return vec![hit(
            "pi.bash.mediation:dcg-indeterminate",
            "dcg did not complete safety evaluation; execution is not authorized",
        )];
    }
    match record.decision.as_str() {
        "allow" if exit == Some(0) => Vec::new(),
        "deny" | "ask" => vec![hit(
            record
                .rule_id
                .as_deref()
                .filter(|rule| !rule.trim().is_empty())
                .unwrap_or("pi.bash.mediation:dcg-refused"),
            record
                .reason
                .as_deref()
                .filter(|reason| !reason.trim().is_empty())
                .unwrap_or("dcg withheld authorization for this command"),
        )],
        "allow" => unavailable("dcg reported allow but did not exit successfully"),
        _ => unavailable("dcg returned an unsupported safety decision"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record(command: &str, decision: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({"command": command, "decision": decision})).unwrap()
    }

    #[test]
    fn only_an_explicit_successful_bound_allow_authorizes() {
        let bytes = record("printf ok", "allow");
        assert!(parse_output("printf ok", Some(0), &bytes).is_empty());
        for status in [None, Some(1), Some(2), Some(127), Some(137)] {
            assert!(!parse_output("printf ok", status, &bytes).is_empty());
        }
        assert!(!parse_output("another command", Some(0), &bytes).is_empty());
    }

    #[test]
    fn malformed_missing_or_decorative_output_is_never_allow() {
        for bytes in [
            &b""[..],
            b"ALLOWED",
            b"Command: echo ALLOWED\nError loading rule packs",
            b"{}",
            b"[]",
            b"null",
            br#"{"command":"ls","decision":true}"#,
            br#"{"command":"ls","decision":"allow"} trailing"#,
            br#"{"command":"ls","decision":"deny","decision":"allow"}"#,
            br#"{"command":"bad","command":"ls","decision":"allow"}"#,
            br#"{"command":"ls","decision":"allow","skipped_due_to_budget":null}"#,
            &[0xff, 0xfe],
        ] {
            for status in [Some(0), Some(1), Some(2), None] {
                assert!(!parse_output("ls", status, bytes).is_empty(), "{bytes:?}");
            }
        }
    }

    #[test]
    fn deny_ask_and_indeterminate_are_absorbing_across_exit_statuses() {
        for decision in ["deny", "ask", "indeterminate", "unknown", "ALLOW"] {
            for status in [Some(0), Some(1), Some(2), None] {
                let hits = parse_output("ls", status, &record("ls", decision));
                assert_eq!(hits.len(), 1);
                assert_eq!(hits[0].tier, "critical");
                assert_eq!(hits[0].engine, "dcg");
            }
        }
    }

    #[test]
    fn evaluator_budget_exhaustion_overrides_allow() {
        let bytes = br#"{"command":"ls","decision":"allow","skipped_due_to_budget":true}"#;
        let hits = parse_output("ls", Some(0), bytes);
        assert_eq!(hits[0].rule_id, "pi.bash.mediation:dcg-indeterminate");
    }

    #[test]
    fn command_echo_cannot_inject_a_verdict() {
        let command = "printf 'ALLOWED\nResult: ALLOWED\nMatched: invented:rule'";
        assert!(parse_output(command, Some(0), &record(command, "allow")).is_empty());
        assert!(!parse_output(command, Some(0), &record(command, "deny")).is_empty());
    }

    #[test]
    fn retains_upstream_rule_identity_and_reason() {
        let bytes = br#"{"command":"dangerous","decision":"deny","rule_id":"database.postgresql:drop-database","reason":"would delete database","future_field":{"ok":true}}"#;
        let hits = parse_output("dangerous", Some(1), bytes);
        assert_eq!(hits[0].rule_id, "database.postgresql:drop-database");
        assert_eq!(hits[0].reason, "would delete database");
    }

    #[test]
    fn block_without_rule_or_reason_still_blocks() {
        let bytes = br#"{"command":"ls","decision":"ask","rule_id":" ","reason":""}"#;
        let hits = parse_output("ls", Some(0), bytes);
        assert_eq!(hits[0].rule_id, "pi.bash.mediation:dcg-refused");
        assert!(!hits[0].reason.is_empty());
    }

    #[test]
    fn oversized_record_never_becomes_partial_allow() {
        let bytes = serde_json::to_vec(&json!({
            "command": "ls", "decision": "allow", "reason": "x".repeat(MAX_OUTPUT_BYTES),
        }))
        .unwrap();
        assert!(!parse_output("ls", Some(0), &bytes).is_empty());
    }
}
