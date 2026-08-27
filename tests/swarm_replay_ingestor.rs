#![allow(clippy::too_many_lines)]
#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use pi::swarm_replay::{
    SWARM_REPLAY_PERFORMANCE_EVIDENCE_SCHEMA, SWARM_REPLAY_POLICY_REPORT_SCHEMA,
    SWARM_REPLAY_REPORT_SCHEMA, SWARM_REPLAY_TRACE_SCHEMA, SwarmReplayBaselinePolicy,
    SwarmReplayEvent, SwarmReplayEventUncertainty, SwarmReplayGuards, SwarmReplayIngestRequest,
    SwarmReplayOrdering, SwarmReplayPerformanceBudget, SwarmReplayPerformanceObservation,
    SwarmReplayPolicyDecision, SwarmReplayRedactionSummary, SwarmReplayTrace,
    SwarmReplayUncertaintySummary, build_swarm_replay_performance_evidence,
    build_swarm_replay_trace, default_swarm_replay_baseline_policies,
    evaluate_swarm_replay_baseline_policies, replay_swarm_trace, swarm_replay_ordering_cost_units,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const GENERATED_AT: &str = "2026-05-13T18:40:00Z";
const GOLDEN_TRACE: &str = "tests/golden_corpus/swarm_replay_trace/normalized_trace.json";
const FAULT_INJECTION_CORPUS: &str =
    "tests/golden_corpus/swarm_replay_trace/fault_injection_corpus.json";

type TestResult = Result<(), Box<dyn Error>>;

static WORKSPACE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Deserialize)]
struct FaultInjectionCorpus {
    schema: String,
    generated_at: String,
    scenarios: Vec<FaultInjectionScenario>,
}

#[derive(Debug, Deserialize)]
struct FaultInjectionScenario {
    scenario_id: String,
    title: String,
    event_log_path: String,
    artifact_manifest: Vec<FaultInjectionArtifact>,
    expected_diagnostics: Vec<String>,
    expected_decisions: Vec<ExpectedPolicyDecision>,
    expected_reservation_conflict_count: u64,
    expected_saturation_reasons: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct FaultInjectionArtifact {
    path: String,
    artifact_schema: String,
    evidence_kind: String,
}

#[derive(Debug, Deserialize)]
struct ExpectedPolicyDecision {
    policy_id: String,
    action: String,
    target_id: String,
    reason_codes: Vec<String>,
    would_require_live_mutation: bool,
}

fn test_workspace(name: &str) -> Result<PathBuf, Box<dyn Error>> {
    let nonce = WORKSPACE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let target_root = std::env::var_os("CARGO_TARGET_DIR").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"),
        PathBuf::from,
    );
    let root = target_root
        .join("swarm_replay_ingestor_tests")
        .join(format!("{name}-{}-{nonce}", std::process::id()));
    fs::create_dir_all(&root)?;
    Ok(root)
}

fn write_text(root: &Path, rel: &str, text: &str) -> std::io::Result<()> {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, text)
}

fn write_json(root: &Path, rel: &str, value: &Value) -> std::io::Result<()> {
    write_text(root, rel, &serde_json::to_string_pretty(value)?)
}

fn write_jsonl_rows<T: Serialize>(
    root: &Path,
    rel: &str,
    rows: &[T],
) -> Result<(), Box<dyn Error>> {
    let mut text = String::new();
    for row in rows {
        text.push_str(&serde_json::to_string(row)?);
        text.push('\n');
    }
    write_text(root, rel, &text)?;
    Ok(())
}

fn load_json(rel: &str) -> Result<Value, Box<dyn Error>> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    let raw = fs::read_to_string(&path)?;
    Ok(serde_json::from_str(&raw)?)
}

fn load_jsonl_events(rel: &str) -> Result<Vec<SwarmReplayEvent>, Box<dyn Error>> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    let raw = fs::read_to_string(&path)?;
    let mut events = Vec::new();
    for (line_index, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let event = serde_json::from_str::<SwarmReplayEvent>(line).map_err(|err| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{rel}:{} invalid replay event: {err}", line_index + 1),
            )
        })?;
        events.push(event);
    }
    Ok(events)
}

fn base_request(root: &Path) -> SwarmReplayIngestRequest {
    SwarmReplayIngestRequest::new("fixture-clean-replay-trace", GENERATED_AT, root)
        .with_git_identity("abc123", "main")
        .with_source_override("agent_mail_archive", "mail/archive.json")
        .with_source_override("git_refs", "git/refs.json")
        .with_source_override("validation_command_records", "validation/records.json")
        .with_source_override("swarm_flight_recorder", "flight/events.jsonl")
        .with_source_override("swarm_activity_ledger", "activity/events.jsonl")
}

fn write_clean_sources(root: &Path, include_agent_mail: bool) -> std::io::Result<()> {
    write_text(
        root,
        ".beads/issues.jsonl",
        r#"{"id":"bd-clean","status":"in_progress","priority":3,"assignee":"AmberOsprey","updated_at":"2026-05-13T18:00:00Z"}"#,
    )?;
    write_text(root, ".beads/beads.db", "sqlite fixture bytes")?;
    if include_agent_mail {
        write_json(
            root,
            "mail/archive.json",
            &json!({
                "messages": [{
                    "thread_id": "bd-clean",
                    "sender": "AmberOsprey",
                    "recipients": ["SilentReef"],
                    "importance": "normal",
                    "ack_required": true,
                    "created_at": "2026-05-13T18:01:00Z",
                    "body": "SECRET BODY SHOULD NOT SURVIVE"
                }],
                "reservations": [{
                    "id": "res-1",
                    "path_patterns": ["src/swarm_replay.rs"],
                    "exclusive": true,
                    "ttl_seconds": 3600,
                    "reason": "bd-in57w.2",
                    "holder": "AmberOsprey",
                    "created_at": "2026-05-13T18:02:00Z"
                }],
                "reservation_conflicts": [{
                    "path_pattern": "src/doctor.rs",
                    "holder": "SunnyBeacon",
                    "conflict_reason": "active exclusive lease",
                    "created_at": "2026-05-13T18:03:00Z"
                }],
                "build_slots": [{
                    "slot": "cargo-all-targets",
                    "holder": "AmberOsprey",
                    "state": "released",
                    "expires_at_utc": "2026-05-13T19:00:00Z",
                    "created_at": "2026-05-13T18:04:00Z"
                }]
            }),
        )?;
    }
    write_json(
        root,
        "docs/evidence/doctor-swarm.json",
        &json!({
            "findings": [{
                "finding_id": "mail_degraded",
                "severity": "degraded",
                "surface": "agent_mail",
                "status": "observed",
                "created_at": "2026-05-13T18:05:00Z"
            }]
        }),
    )?;
    write_json(
        root,
        "docs/evidence/rch-queue-status.json",
        &json!({
            "jobs": [{
                "job_id": "rch-1",
                "state": "finished",
                "worker": "worker-redacted",
                "command": "rch exec -- cargo check --all-targets",
                "queue_position": 0,
                "created_at": "2026-05-13T18:06:00Z"
            }]
        }),
    )?;
    write_json(
        root,
        "docs/evidence/swarm-operator-runpack.json",
        &json!({
            "recommendations": [{
                "action": "continue_bd_in57w_2",
                "severity": "normal",
                "evidence_paths": ["docs/contracts/swarm-replay-trace-contract.json"],
                "operator_notes": "read-only replay ingestion",
                "created_at": "2026-05-13T18:07:00Z"
            }],
            "operator_handoff": {
                "handoff_id": "handoff-clean",
                "summary": "continue replay lab",
                "next_actions": ["implement replay engine"],
                "evidence_paths": ["tests/golden_corpus/swarm_replay_trace/normalized_trace.json"],
                "created_at": "2026-05-13T18:08:00Z"
            }
        }),
    )?;
    write_json(
        root,
        "git/refs.json",
        &json!({
            "head": "abc123",
            "branch": "main",
            "dirty": false,
            "changed_paths": [],
            "created_at": "2026-05-13T18:09:00Z"
        }),
    )?;
    write_json(
        root,
        "validation/records.json",
        &json!({
            "commands": [{
                "command": "rch exec -- cargo test --test swarm_replay_ingestor",
                "runner": "rch",
                "exit_code": 0,
                "target_dir": "/data/tmp/pi_agent_rust_cargo/amberosprey/target",
                "tmpdir": "/data/tmp/pi_agent_rust_cargo/amberosprey/tmp",
                "created_at": "2026-05-13T18:10:00Z"
            }],
            "artifacts": [{
                "artifact_path": "tests/golden_corpus/swarm_replay_trace/normalized_trace.json",
                "artifact_schema": "pi.swarm.replay_trace.v1",
                "verdict": "pass",
                "command": "cargo test --test swarm_replay_ingestor",
                "created_at": "2026-05-13T18:11:00Z"
            }]
        }),
    )?;
    write_json(
        root,
        "docs/evidence/context-intelligence-closeout-gate.json",
        &json!({
            "schema": "pi.context_intelligence.closeout_gate.v1",
            "verdict": "pass",
            "generated_at": "2026-05-13T18:12:00Z"
        }),
    )?;
    write_text(
        root,
        "flight/events.jsonl",
        r#"{"schema":"pi.swarm.flight_recorder.event.v1","event_kind":"agent_turn","agent_name":"AmberOsprey","created_at":"2026-05-13T18:13:00Z"}"#,
    )?;
    write_text(
        root,
        "activity/events.jsonl",
        r#"{"schema":"pi.swarm.activity_ledger.v1","event_kind":"operator_handoff","handoff_id":"activity-handoff","summary":"handoff from activity ledger","next_actions":["inspect replay"],"evidence_paths":["tests/full_suite_gate/swarm_activity_digest.json"],"created_at":"2026-05-13T18:14:00Z"}"#,
    )
}

fn no_mock_e2e_request(root: &Path) -> SwarmReplayIngestRequest {
    SwarmReplayIngestRequest::new("fixture-no-mock-swarm-replay-e2e", GENERATED_AT, root)
        .with_git_identity("e2e123", "main")
        .with_source_override("agent_mail_archive", "mail/archive.json")
        .with_source_override("git_refs", "git/refs.json")
        .with_source_override("validation_command_records", "validation/records.json")
        .with_source_override("swarm_flight_recorder", "flight/events.jsonl")
        .with_source_override("swarm_activity_ledger", "activity/events.jsonl")
}

fn write_no_mock_e2e_sources(root: &Path) -> std::io::Result<()> {
    write_text(
        root,
        ".beads/issues.jsonl",
        concat!(
            r#"{"id":"bd-e2e-ready","status":"open","priority":1,"assignee":"unassigned","updated_at":"2026-05-13T18:00:00Z"}"#,
            "\n",
            r#"{"id":"bd-e2e-stale","status":"in_progress","priority":2,"assignee":"LongGoneAgent","updated_at":"2026-05-13T18:01:00Z"}"#,
            "\n",
            r#"{"id":"bd-e2e-closed","status":"closed","priority":3,"assignee":"AmberOsprey","updated_at":"2026-05-13T18:02:00Z"}"#,
            "\n"
        ),
    )?;
    write_text(
        root,
        ".beads/beads.db",
        "sqlite fixture bytes for no-mock swarm replay e2e",
    )?;
    write_json(
        root,
        "mail/archive.json",
        &json!({
            "messages": [{
                "thread_id": "bd-e2e-ready",
                "sender": "AmberOsprey",
                "recipients": ["SilentReef"],
                "importance": "high",
                "ack_required": true,
                "created_at": "2026-05-13T18:03:00Z",
                "body": "SECRET MAIL BODY MUST BE REDACTED"
            }],
            "reservations": [{
                "id": "res-e2e-source",
                "path_patterns": ["src/swarm_replay.rs", "tests/swarm_replay_ingestor.rs"],
                "exclusive": true,
                "ttl_seconds": 3600,
                "reason": "bd-in57w.9",
                "holder": "AmberOsprey",
                "state": "active",
                "created_at": "2026-05-13T18:04:00Z"
            }],
            "reservation_conflicts": [{
                "path_pattern": "src/providers/**",
                "holder": "OtherAgent",
                "conflict_reason": "active exclusive lease",
                "created_at": "2026-05-13T18:05:00Z"
            }],
            "build_slots": [{
                "slot": "cargo-all-targets",
                "holder": "AmberOsprey",
                "state": "active",
                "expires_at_utc": "2026-05-13T19:00:00Z",
                "created_at": "2026-05-13T18:06:00Z"
            }]
        }),
    )?;
    write_json(
        root,
        "docs/evidence/doctor-swarm.json",
        &json!({
            "host_profile": {
                "profile_id": "no-mock-e2e-64-core",
                "cpu_cores": 64,
                "memory_gib": 256,
                "numa_nodes": 4,
                "cgroup_cpu_quota": 48,
                "cgroup_memory_gib": 192,
                "max_agent_concurrency": 32,
                "max_tool_concurrency": 16,
                "extension_hostcall_lanes": 24,
                "rch_worker_slots": 8,
                "target_dir": "/data/tmp/pi_agent_rust_cargo/amberosprey/target",
                "target_free_gib": 512,
                "tmpdir": "/data/tmp/pi_agent_rust_cargo/amberosprey/tmp",
                "tmpdir_free_gib": 256,
                "numa_hint": "pin_rch_workers_by_socket",
                "created_at": "2026-05-13T18:07:00Z"
            },
            "findings": [{
                "finding_id": "agent_mail_degraded_but_readable",
                "severity": "warn",
                "surface": "agent_mail",
                "status": "observed",
                "created_at": "2026-05-13T18:08:00Z"
            }]
        }),
    )?;
    write_json(
        root,
        "docs/evidence/rch-queue-status.json",
        &json!({
            "jobs": [{
                "job_id": "e2e-rch-queued",
                "state": "queued",
                "worker": "worker-redacted",
                "command": "rch exec -- cargo clippy --all-targets -- -D warnings",
                "queue_position": 4,
                "created_at": "2026-05-13T18:09:00Z"
            }]
        }),
    )?;
    write_json(
        root,
        "docs/evidence/swarm-operator-runpack.json",
        &json!({
            "recommendations": [{
                "action": "continue_bd_in57w_9",
                "severity": "normal",
                "evidence_paths": [
                    "docs/contracts/swarm-operator-runpack-contract.json",
                    "tests/golden_corpus/swarm_operator_runpack/complete_runpack_projection.json"
                ],
                "operator_notes": "offline replay lab harness; no network or live mutation",
                "created_at": "2026-05-13T18:10:00Z"
            }],
            "operator_handoff": {
                "handoff_id": "handoff-no-mock-e2e",
                "summary": "Use replay evidence before claiming more swarm work",
                "next_actions": [
                    "inspect policy comparison report",
                    "verify runpack recommendation references checked-in evidence"
                ],
                "evidence_paths": [
                    "tests/golden_corpus/swarm_replay_trace/normalized_trace.json",
                    "tests/e2e_results/20260422T201126Z/replay_bundle.json"
                ],
                "created_at": "2026-05-13T18:11:00Z"
            }
        }),
    )?;
    write_json(
        root,
        "git/refs.json",
        &json!({
            "head": "e2e123",
            "branch": "main",
            "dirty": false,
            "changed_paths": [],
            "created_at": "2026-05-13T18:12:00Z"
        }),
    )?;
    write_json(
        root,
        "validation/records.json",
        &json!({
            "commands": [{
                "command": "rch exec -- cargo test --test swarm_replay_ingestor",
                "runner": "rch",
                "exit_code": 0,
                "target_dir": "/data/tmp/pi_agent_rust_cargo/amberosprey/target",
                "tmpdir": "/data/tmp/pi_agent_rust_cargo/amberosprey/tmp",
                "created_at": "2026-05-13T18:13:00Z"
            }],
            "artifacts": [
                {
                    "artifact_path": "tests/e2e_results/20260422T201126Z/replay_bundle.json",
                    "artifact_schema": "pi.e2e.replay_bundle.v1",
                    "verdict": "observed",
                    "command": "scripts/e2e/run_all.sh",
                    "created_at": "2026-05-13T18:14:00Z"
                },
                {
                    "artifact_path": "tests/golden_corpus/swarm_replay_trace/normalized_trace.json",
                    "artifact_schema": "pi.swarm.replay_trace.v1",
                    "verdict": "pass",
                    "command": "cargo test --test swarm_replay_ingestor",
                    "created_at": "2026-05-13T18:15:00Z"
                }
            ]
        }),
    )?;
    write_json(
        root,
        "docs/evidence/context-intelligence-closeout-gate.json",
        &json!({
            "schema": "pi.context_intelligence.closeout_gate.v1",
            "verdict": "pass",
            "generated_at": "2026-05-13T18:16:00Z"
        }),
    )?;
    write_text(
        root,
        "flight/events.jsonl",
        concat!(
            r#"{"schema":"pi.swarm.flight_recorder.event.v1","event_kind":"agent_turn","agent_name":"AmberOsprey","created_at":"2026-05-13T18:17:00Z"}"#,
            "\n",
            r#"{"schema":"pi.swarm.flight_recorder.event.v1","event_kind":"validation_gate","agent_name":"AmberOsprey","created_at":"2026-05-13T18:18:00Z"}"#,
            "\n"
        ),
    )?;
    write_text(
        root,
        "activity/events.jsonl",
        concat!(
            r#"{"schema":"pi.swarm.activity_ledger.v1","event_kind":"operator_handoff","handoff_id":"activity-no-mock-e2e","summary":"activity ledger handoff","next_actions":["compare policies"],"evidence_paths":["tests/full_suite_gate/swarm_activity_digest.json"],"created_at":"2026-05-13T18:19:00Z"}"#,
            "\n",
            r#"{"schema":"pi.swarm.activity_ledger.v1","event_kind":"verification","agent_name":"AmberOsprey","created_at":"2026-05-13T18:20:00Z"}"#,
            "\n"
        ),
    )?;
    write_text(root, "negative/malformed-rch.json", "{\"jobs\":[")?;
    Ok(())
}

fn source_row<'a>(
    trace: &'a SwarmReplayTrace,
    source_id: &str,
) -> Result<&'a pi::swarm_replay::SwarmReplaySourceInventoryRow, String> {
    trace
        .source_inventory
        .iter()
        .find(|row| row.source_id == source_id)
        .ok_or_else(|| format!("missing source row {source_id}"))
}

fn event_types(trace: &SwarmReplayTrace) -> BTreeSet<String> {
    trace
        .events
        .iter()
        .map(|event| event.event_type.clone())
        .collect()
}

fn assert_monotonic_sequence(trace: &SwarmReplayTrace) -> TestResult {
    for (index, event) in trace.events.iter().enumerate() {
        let expected = u64::try_from(index + 1)?;
        assert_eq!(event.sequence, expected);
    }
    Ok(())
}

fn replay_event(
    event_id: &str,
    sequence: u64,
    occurred_at_utc: &str,
    event_type: &str,
    source_ref: &str,
    payload: Value,
) -> SwarmReplayEvent {
    SwarmReplayEvent {
        event_id: event_id.to_string(),
        sequence,
        occurred_at_utc: occurred_at_utc.to_string(),
        observed_at_utc: GENERATED_AT.to_string(),
        event_type: event_type.to_string(),
        actor: "AmberOsprey".to_string(),
        source_ref: source_ref.to_string(),
        source_hash: None,
        redaction_state: "none".to_string(),
        uncertainty: SwarmReplayEventUncertainty {
            state: "certain".to_string(),
            reasons: Vec::new(),
            suppressed_claims: Vec::new(),
        },
        payload,
    }
}

fn replay_event_with_actor(
    actor: &str,
    event_id: &str,
    sequence: u64,
    occurred_at_utc: &str,
    event_type: &str,
    source_ref: &str,
    payload: Value,
) -> SwarmReplayEvent {
    let mut event = replay_event(
        event_id,
        sequence,
        occurred_at_utc,
        event_type,
        source_ref,
        payload,
    );
    event.actor = actor.to_string();
    event
}

fn uncertain_replay_event(
    event_id: &str,
    sequence: u64,
    event_type: &str,
    source_ref: &str,
    reasons: &[&str],
    suppressed_claims: &[&str],
    payload: Value,
) -> SwarmReplayEvent {
    let mut event = replay_event(
        event_id,
        sequence,
        "2026-05-13T18:00:00Z",
        event_type,
        source_ref,
        payload,
    );
    event.uncertainty = SwarmReplayEventUncertainty {
        state: "missing_source".to_string(),
        reasons: reasons.iter().map(ToString::to_string).collect(),
        suppressed_claims: suppressed_claims.iter().map(ToString::to_string).collect(),
    };
    event
}

fn trace_from_events(events: Vec<SwarmReplayEvent>) -> SwarmReplayTrace {
    SwarmReplayTrace {
        schema: SWARM_REPLAY_TRACE_SCHEMA.to_string(),
        trace_id: "engine-fixture".to_string(),
        generated_at: GENERATED_AT.to_string(),
        contract_version: "1.0.0".to_string(),
        source_inventory: Vec::new(),
        ordering: SwarmReplayOrdering {
            monotonic_sequence_required: true,
            timestamp_normalization: "utc_rfc3339_z".to_string(),
            tie_breakers: vec![
                "sequence".to_string(),
                "source_ref".to_string(),
                "event_id".to_string(),
            ],
        },
        events,
        redaction_summary: SwarmReplayRedactionSummary {
            redacted_count: 0,
            sensitive_omitted_count: 0,
            raw_secret_bytes_emitted: 0,
            redacted_fields: Vec::new(),
        },
        uncertainty_summary: SwarmReplayUncertaintySummary {
            missing_sources: Vec::new(),
            malformed_sources: Vec::new(),
            stale_sources: Vec::new(),
            suppressed_claims: Vec::new(),
            event_count_by_uncertainty: std::collections::BTreeMap::default(),
        },
        replay_guards: SwarmReplayGuards {
            read_only: true,
            no_live_mutation: true,
            no_network_required: true,
            fail_closed_on_missing_required_sources: true,
            requires_source_inventory: true,
            disallowed_live_actions: Vec::new(),
        },
    }
}

fn diagnostic_codes(report: &pi::swarm_replay::SwarmReplayReport) -> BTreeSet<String> {
    report
        .diagnostics
        .iter()
        .map(|diagnostic| diagnostic.code.clone())
        .collect()
}

fn metamorphic_semantic_events() -> Vec<SwarmReplayEvent> {
    vec![
        replay_event(
            "mr-bead-in-progress",
            10,
            "2026-05-13T18:00:00Z",
            "bead_lifecycle",
            "beads_jsonl",
            json!({
                "bead_id": "bd-metamorphic",
                "to_status": "in_progress",
                "priority": 2,
                "assignee": "Codex"
            }),
        ),
        replay_event(
            "mr-validation-gate",
            20,
            "2026-05-13T18:01:00Z",
            "cargo_gate_result",
            "validation_command_records",
            json!({
                "command": "rch exec -- cargo test metamorphic_swarm -- --nocapture",
                "runner": "rch",
                "exit_code": 0,
                "target_dir": "/data/tmp/pi_agent_rust_cargo/codex/target",
                "tmpdir": "/data/tmp/pi_agent_rust_cargo/codex/tmp"
            }),
        ),
        replay_event(
            "mr-runpack-recommendation",
            30,
            "2026-05-13T18:02:00Z",
            "runpack_recommendation",
            "operator_runpack",
            json!({
                "action": "continue_bd_zeccr_5",
                "severity": "normal",
                "evidence_paths": [
                    "tests/swarm_replay_ingestor.rs",
                    "tests/golden_corpus/swarm_replay_trace/normalized_trace.json"
                ]
            }),
        ),
        replay_event(
            "mr-operator-handoff",
            40,
            "2026-05-13T18:03:00Z",
            "operator_handoff",
            "operator_runpack",
            json!({
                "handoff_id": "handoff-metamorphic",
                "summary": "Metamorphic replay state converged",
                "next_actions": [
                    "inspect metamorphic comparison log",
                    "close bd-zeccr.5 after validation"
                ],
                "evidence_paths": ["target/swarm_replay_ingestor_tests"]
            }),
        ),
        replay_event(
            "mr-worktree-final",
            50,
            "2026-05-13T18:04:00Z",
            "worktree_state",
            "git_refs",
            json!({
                "head": "metamorphic-head",
                "branch": "main",
                "dirty": false,
                "changed_paths": ["tests/swarm_replay_ingestor.rs"]
            }),
        ),
        replay_event(
            "mr-bead-closed",
            60,
            "2026-05-13T18:05:00Z",
            "bead_lifecycle",
            "beads_jsonl",
            json!({
                "bead_id": "bd-metamorphic",
                "to_status": "closed",
                "priority": 2,
                "assignee": "Codex"
            }),
        ),
    ]
}

fn with_event(mut events: Vec<SwarmReplayEvent>, event: SwarmReplayEvent) -> Vec<SwarmReplayEvent> {
    events.push(event);
    events
}

fn metamorphic_state_projection(report: &pi::swarm_replay::SwarmReplayReport) -> Value {
    let beads = report
        .final_state
        .beads
        .values()
        .map(|bead| {
            json!({
                "bead_id": bead.bead_id,
                "status": bead.status,
                "priority": bead.priority,
                "assignee": bead.assignee
            })
        })
        .collect::<Vec<_>>();
    let operator_handoffs = report
        .final_state
        .operator_handoffs
        .values()
        .map(|handoff| {
            json!({
                "handoff_id": handoff.handoff_id,
                "summary": handoff.summary,
                "next_actions": handoff.next_actions,
                "evidence_paths": handoff.evidence_paths
            })
        })
        .collect::<Vec<_>>();
    let validation_gates = report
        .final_state
        .validation_gates
        .values()
        .map(|gate| {
            json!({
                "command": gate.command,
                "runner": gate.runner,
                "exit_code": gate.exit_code,
                "target_dir": gate.target_dir,
                "tmpdir": gate.tmpdir
            })
        })
        .collect::<Vec<_>>();
    let runpack_recommendations = report
        .final_state
        .runpack_recommendations
        .values()
        .map(|recommendation| {
            json!({
                "action": recommendation.action,
                "severity": recommendation.severity,
                "evidence_paths": recommendation.evidence_paths
            })
        })
        .collect::<Vec<_>>();
    let worktree = report.final_state.worktree.as_ref().map(|worktree| {
        json!({
            "head": worktree.head,
            "branch": worktree.branch,
            "dirty": worktree.dirty,
            "changed_paths": worktree.changed_paths
        })
    });
    json!({
        "beads": beads,
        "operator_handoffs": operator_handoffs,
        "validation_gates": validation_gates,
        "runpack_recommendations": runpack_recommendations,
        "worktree": worktree,
        "coordination": {
            "agent_mail_available": report.final_state.coordination.agent_mail_available,
            "missing_agent_mail_evidence": report.final_state.coordination.missing_agent_mail_evidence,
            "reservation_conflict_count": report.final_state.coordination.reservation_conflict_count,
            "last_operator_action": report.final_state.coordination.last_operator_action
        }
    })
}

fn push_metamorphic_comparison(
    records: &mut Vec<Value>,
    transform_id: &str,
    fixture_id: &str,
    left: &pi::swarm_replay::SwarmReplayReport,
    right: &pi::swarm_replay::SwarmReplayReport,
    expected_equivalent: bool,
) -> bool {
    let compared_fields = [
        "beads",
        "operator_handoffs",
        "validation_gates",
        "runpack_recommendations",
        "worktree",
        "coordination",
    ];
    let left_projection = metamorphic_state_projection(left);
    let right_projection = metamorphic_state_projection(right);
    let mismatches = compared_fields
        .iter()
        .filter(|field| left_projection.get(*field) != right_projection.get(*field))
        .map(|field| {
            json!({
                "field": field,
                "left": left_projection.get(*field),
                "right": right_projection.get(*field)
            })
        })
        .collect::<Vec<_>>();
    let equivalent = mismatches.is_empty();
    records.push(json!({
        "schema": "pi.swarm.metamorphic_replay_comparison.v1",
        "transform_id": transform_id,
        "fixture_id": fixture_id,
        "expected_equivalent": expected_equivalent,
        "equivalent": equivalent,
        "compared_state_fields": compared_fields,
        "mismatch_reason": if mismatches.is_empty() {
            Value::Null
        } else {
            json!(mismatches)
        },
        "left_replayed_event_count": left.replayed_event_count,
        "right_replayed_event_count": right.replayed_event_count,
        "left_diagnostic_codes": diagnostic_codes(left),
        "right_diagnostic_codes": diagnostic_codes(right)
    }));
    equivalent
}

fn decision<'a>(
    decisions: &'a [SwarmReplayPolicyDecision],
    policy_id: &str,
    action: &str,
) -> Result<&'a SwarmReplayPolicyDecision, String> {
    decisions
        .iter()
        .find(|item| item.policy_id == policy_id && item.action == action)
        .ok_or_else(|| format!("missing policy decision {policy_id}/{action}"))
}

fn write_no_mock_e2e_outputs(
    root: &Path,
    trace: &SwarmReplayTrace,
    replay: &pi::swarm_replay::SwarmReplayReport,
    policy_report: &pi::swarm_replay::SwarmReplayPolicyReport,
) -> Result<Value, Box<dyn Error>> {
    write_json(root, "evidence/trace.json", &serde_json::to_value(trace)?)?;
    write_json(
        root,
        "evidence/replay-report.json",
        &serde_json::to_value(replay)?,
    )?;
    write_json(
        root,
        "evidence/policy-report.json",
        &serde_json::to_value(policy_report)?,
    )?;
    write_jsonl_rows(root, "evidence/replay-events.jsonl", &trace.events)?;

    let comparison_report = json!({
        "schema": "pi.swarm.replay_e2e_comparison_report.v1",
        "trace_id": trace.trace_id,
        "policy_count": policy_report.policy_ids.len(),
        "decision_count": policy_report.decision_count,
        "comparison_count": policy_report.comparison_count,
        "policy_deltas": policy_report.policy_comparisons,
        "guards": policy_report.policy_guards
    });
    write_json(
        root,
        "evidence/policy-comparison-report.json",
        &comparison_report,
    )?;

    let replay_summary = json!({
        "schema": "pi.swarm.replay_e2e_summary.v1",
        "trace_id": trace.trace_id,
        "source_count": trace.source_inventory.len(),
        "event_count": trace.events.len(),
        "replayed_event_count": replay.replayed_event_count,
        "policy_count": policy_report.policy_ids.len(),
        "decision_count": policy_report.decision_count,
        "comparison_count": policy_report.comparison_count,
        "runpack_recommendation_count": replay.final_state.runpack_recommendations.len(),
        "operator_handoff_count": replay.final_state.operator_handoffs.len(),
        "diagnostic_count": replay.diagnostics.len(),
        "guards": {
            "trace_read_only": trace.replay_guards.read_only,
            "replay_no_live_mutation": replay.replay_guards.no_live_mutation,
            "policy_advisory_only": policy_report.policy_guards.advisory_only
        }
    });
    write_json(root, "evidence/replay-summary.json", &replay_summary)?;

    let manifest = json!({
        "schema": "pi.swarm.replay_e2e_artifact_manifest.v1",
        "generated_at": GENERATED_AT,
        "trace_id": trace.trace_id,
        "entries": [
            {
                "path": "evidence/replay-events.jsonl",
                "artifact_schema": "pi.swarm.replay_events_jsonl.v1",
                "evidence_kind": "jsonl_event_log",
                "record_count": trace.events.len()
            },
            {
                "path": "evidence/replay-summary.json",
                "artifact_schema": "pi.swarm.replay_e2e_summary.v1",
                "evidence_kind": "replay_summary"
            },
            {
                "path": "evidence/policy-comparison-report.json",
                "artifact_schema": "pi.swarm.replay_e2e_comparison_report.v1",
                "evidence_kind": "comparison_report"
            },
            {
                "path": "evidence/trace.json",
                "artifact_schema": SWARM_REPLAY_TRACE_SCHEMA,
                "evidence_kind": "normalized_trace"
            },
            {
                "path": "evidence/replay-report.json",
                "artifact_schema": SWARM_REPLAY_REPORT_SCHEMA,
                "evidence_kind": "replay_report"
            },
            {
                "path": "evidence/policy-report.json",
                "artifact_schema": SWARM_REPLAY_POLICY_REPORT_SCHEMA,
                "evidence_kind": "policy_report"
            }
        ],
        "source_inventory": trace.source_inventory
    });
    write_json(root, "evidence/artifact-manifest.json", &manifest)?;
    Ok(manifest)
}

#[test]
fn clean_sources_normalize_into_contract_events() -> TestResult {
    let root = test_workspace("clean_sources")?;
    write_clean_sources(&root, true)?;

    let trace = build_swarm_replay_trace(&base_request(&root))?;
    assert_eq!(trace.schema, SWARM_REPLAY_TRACE_SCHEMA);
    assert_eq!(trace.source_inventory.len(), 11);
    assert!(trace.replay_guards.read_only);
    assert!(trace.replay_guards.no_live_mutation);
    assert_eq!(trace.redaction_summary.raw_secret_bytes_emitted, 0);
    assert!(
        trace
            .redaction_summary
            .redacted_fields
            .iter()
            .any(|field| field.contains("body")),
        "agent mail body must be redacted"
    );

    let required_event_types = [
        "bead_lifecycle",
        "reservation_intent",
        "reservation_conflict",
        "agent_message",
        "build_slot_state",
        "rch_job_state",
        "cargo_gate_result",
        "worktree_state",
        "doctor_finding",
        "runpack_recommendation",
        "validation_artifact",
        "operator_handoff",
    ];
    let observed = event_types(&trace);
    for required in required_event_types {
        assert!(
            observed.contains(required),
            "missing normalized event type {required}"
        );
    }
    assert_monotonic_sequence(&trace)
}

#[test]
fn missing_agent_mail_keeps_beads_rch_and_doctor_usable() -> TestResult {
    let root = test_workspace("missing_agent_mail")?;
    write_clean_sources(&root, false)?;

    let trace = build_swarm_replay_trace(&base_request(&root))?;
    let mail = source_row(&trace, "agent_mail_archive")?;
    assert_eq!(mail.availability, "unavailable");
    assert_eq!(mail.freshness_state, "missing");
    assert!(
        mail.uncertainty
            .iter()
            .any(|reason| reason == "source_missing")
    );

    let observed = event_types(&trace);
    assert!(observed.contains("bead_lifecycle"));
    assert!(observed.contains("rch_job_state"));
    assert!(observed.contains("doctor_finding"));
    assert!(
        trace
            .uncertainty_summary
            .suppressed_claims
            .iter()
            .any(|claim| claim == "mail_thread_completeness")
    );
    Ok(())
}

#[test]
fn malformed_rch_snapshot_suppresses_queue_claims() -> TestResult {
    let root = test_workspace("malformed_rch_snapshot")?;
    write_clean_sources(&root, true)?;
    write_text(&root, "docs/evidence/rch-queue-status.json", "{not-json")?;

    let trace = build_swarm_replay_trace(&base_request(&root))?;
    let rch = source_row(&trace, "rch_queue_status")?;
    assert_eq!(rch.availability, "malformed");
    assert_eq!(rch.freshness_state, "malformed");
    assert!(!event_types(&trace).contains("rch_job_state"));
    assert!(
        trace
            .uncertainty_summary
            .suppressed_claims
            .iter()
            .any(|claim| claim == "queue_depth")
    );
    Ok(())
}

#[test]
fn doctor_preflight_budget_profile_feeds_resource_timeline() -> TestResult {
    let root = test_workspace("doctor_preflight_resource_profile")?;
    write_clean_sources(&root, true)?;
    write_json(
        &root,
        "docs/evidence/doctor-swarm.json",
        &json!({
            "host_profile": {
                "profile_id": "doctor-64-core",
                "cpu_cores": 64,
                "memory_gib": 256,
                "numa_nodes": 4,
                "cgroup_cpu_quota": 48,
                "cgroup_memory_gib": 192,
                "max_agent_concurrency": 32,
                "max_tool_concurrency": 16,
                "extension_hostcall_lanes": 24,
                "rch_worker_slots": 8,
                "target_dir": "/data/tmp/pi_agent_rust_cargo/doctor/target",
                "target_free_gib": 512,
                "tmpdir": "/data/tmp/pi_agent_rust_cargo/doctor/tmp",
                "tmpdir_free_gib": 256,
                "numa_hint": "pin_rch_workers_by_socket",
                "created_at": "2026-05-13T18:05:00Z"
            },
            "findings": [{
                "finding_id": "resource_budget_loaded",
                "severity": "info",
                "surface": "resource_governor",
                "status": "observed",
                "created_at": "2026-05-13T18:05:30Z"
            }]
        }),
    )?;

    let trace = build_swarm_replay_trace(&base_request(&root))?;
    assert!(event_types(&trace).contains("host_resource_profile"));

    let replay = replay_swarm_trace(&trace)?;
    let profile = replay
        .final_state
        .resource_budget
        .as_ref()
        .ok_or("missing resource budget")?;
    assert_eq!(profile.profile_id, "doctor-64-core");
    assert_eq!(profile.cpu_cores, Some(64));
    assert_eq!(profile.memory_gib, Some(256));
    assert_eq!(profile.numa_nodes, Some(4));
    assert_eq!(profile.rch_worker_slots, Some(8));
    assert_eq!(profile.numa_hint, "pin_rch_workers_by_socket");
    assert!(
        replay
            .resource_pressure_timeline
            .iter()
            .any(|snapshot| snapshot.profile_id.as_deref() == Some("doctor-64-core"))
    );
    Ok(())
}

#[test]
fn stale_runpack_is_classified_without_discarding_inventory() -> TestResult {
    let root = test_workspace("stale_runpack")?;
    write_clean_sources(&root, true)?;
    write_json(
        &root,
        "docs/evidence/swarm-operator-runpack.json",
        &json!({
            "freshness_state": "stale",
            "operator_handoff": {
                "handoff_id": "stale-handoff",
                "summary": "old runpack",
                "next_actions": ["refresh"],
                "evidence_paths": [],
                "created_at": "2026-05-13T18:08:00Z"
            }
        }),
    )?;

    let trace = build_swarm_replay_trace(&base_request(&root))?;
    let runpack = source_row(&trace, "operator_runpack")?;
    assert_eq!(runpack.availability, "stale");
    assert_eq!(runpack.freshness_state, "stale");
    assert!(
        trace
            .uncertainty_summary
            .stale_sources
            .iter()
            .any(|source| source == "operator_runpack")
    );
    Ok(())
}

#[test]
fn duplicate_source_event_ids_are_deduplicated_and_marked() -> TestResult {
    let root = test_workspace("duplicate_source_event_ids")?;
    write_clean_sources(&root, true)?;
    write_json(
        &root,
        "validation/records.json",
        &json!({
            "artifacts": [
                {
                    "artifact_path": "same.json",
                    "artifact_schema": "pi.test",
                    "verdict": "pass",
                    "command": "first",
                    "created_at": "2026-05-13T18:11:00Z"
                },
                {
                    "artifact_path": "same.json",
                    "artifact_schema": "pi.test",
                    "verdict": "pass",
                    "command": "second",
                    "created_at": "2026-05-13T18:11:00Z"
                }
            ]
        }),
    )?;

    let trace = build_swarm_replay_trace(&base_request(&root))?;
    let mut ids = BTreeSet::new();
    for event in &trace.events {
        assert!(
            ids.insert(event.event_id.clone()),
            "duplicate final event id {}",
            event.event_id
        );
    }
    assert!(trace.events.iter().any(|event| {
        event
            .uncertainty
            .reasons
            .iter()
            .any(|reason| reason == "duplicate_source_event_id_deduplicated")
    }));
    Ok(())
}

#[test]
fn checked_in_golden_trace_fixture_is_downstream_consumable() -> TestResult {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(GOLDEN_TRACE);
    let raw = fs::read_to_string(path)?;
    let trace: SwarmReplayTrace = serde_json::from_str(&raw)?;

    assert_eq!(trace.schema, SWARM_REPLAY_TRACE_SCHEMA);
    assert_eq!(trace.contract_version, "1.0.0");
    assert_eq!(trace.source_inventory.len(), 11);
    assert!(trace.replay_guards.read_only);
    assert!(
        trace
            .events
            .iter()
            .any(|event| event.event_type == "bead_lifecycle")
    );
    assert!(
        trace
            .events
            .iter()
            .any(|event| event.event_type == "validation_artifact")
    );
    assert_monotonic_sequence(&trace)
}

#[test]
fn fault_injection_corpus_replays_coordination_failures() -> TestResult {
    let corpus_value = load_json(FAULT_INJECTION_CORPUS)?;
    let corpus: FaultInjectionCorpus = serde_json::from_value(corpus_value)?;

    assert_eq!(corpus.schema, "pi.swarm.replay_fault_injection_corpus.v1");
    assert_eq!(corpus.generated_at, GENERATED_AT);
    assert!(corpus.scenarios.len() >= 3);

    let policies = default_swarm_replay_baseline_policies();
    let mut observed_scenarios = BTreeSet::new();
    for scenario in &corpus.scenarios {
        assert!(
            observed_scenarios.insert(scenario.scenario_id.as_str()),
            "duplicate scenario {}",
            scenario.scenario_id
        );
        assert_ne!(scenario.title.trim(), "");
        assert!(
            scenario.artifact_manifest.iter().any(|artifact| {
                artifact.path == scenario.event_log_path
                    && artifact.artifact_schema == "pi.swarm.replay_events_jsonl.v1"
                    && artifact.evidence_kind == "jsonl_event_log"
            }),
            "scenario {} must manifest its replay JSONL log",
            scenario.scenario_id
        );

        for artifact in &scenario.artifact_manifest {
            assert_ne!(artifact.artifact_schema.trim(), "");
            assert_ne!(artifact.evidence_kind.trim(), "");
            let artifact_path = Path::new(env!("CARGO_MANIFEST_DIR")).join(&artifact.path);
            assert!(
                artifact_path.exists(),
                "scenario {} references missing artifact {}",
                scenario.scenario_id,
                artifact.path
            );
        }

        let raw_log = fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join(&scenario.event_log_path),
        )?;
        assert!(!raw_log.contains("SECRET"));
        assert!(!raw_log.contains("Bearer "));

        let mut trace = trace_from_events(load_jsonl_events(&scenario.event_log_path)?);
        trace.trace_id = format!("fault-injection-{}", scenario.scenario_id);
        trace.generated_at = corpus.generated_at.clone();
        assert_monotonic_sequence(&trace)?;

        let replay = replay_swarm_trace(&trace)?;
        assert!(replay.replay_guards.consumed_trace_only);
        assert_eq!(
            replay.final_state.coordination.reservation_conflict_count,
            scenario.expected_reservation_conflict_count,
            "scenario {} reservation conflict count mismatch",
            scenario.scenario_id
        );

        let diagnostics = diagnostic_codes(&replay);
        for expected in &scenario.expected_diagnostics {
            assert!(
                diagnostics.contains(expected),
                "scenario {} missing diagnostic {expected}",
                scenario.scenario_id
            );
        }

        let report = evaluate_swarm_replay_baseline_policies(&replay, &policies)?;
        assert!(report.policy_guards.advisory_only);
        assert!(report.policy_guards.no_live_mutation);
        for expected in &scenario.expected_decisions {
            let actual = decision(&report.decisions, &expected.policy_id, &expected.action)
                .map_err(|err| {
                    std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("scenario {}: {err}", scenario.scenario_id),
                    )
                })?;
            assert_eq!(
                actual.target_id, expected.target_id,
                "scenario {} decision target mismatch for {}/{}",
                scenario.scenario_id, expected.policy_id, expected.action
            );
            assert_eq!(
                actual.would_require_live_mutation, expected.would_require_live_mutation,
                "scenario {} decision mutation flag mismatch for {}/{}",
                scenario.scenario_id, expected.policy_id, expected.action
            );
            for reason in &expected.reason_codes {
                assert!(
                    actual.reason_codes.contains(reason),
                    "scenario {} decision {}/{} missing reason {reason}",
                    scenario.scenario_id,
                    expected.policy_id,
                    expected.action
                );
            }
        }

        let peak_pressure = replay
            .resource_pressure_timeline
            .last()
            .ok_or("missing replay resource pressure snapshot")?;
        for expected in &scenario.expected_saturation_reasons {
            assert!(
                peak_pressure.saturation_reasons.contains(expected),
                "scenario {} missing saturation reason {expected}",
                scenario.scenario_id
            );
        }
    }

    for required in [
        "agent_mail_unavailable_continue_via_beads",
        "rch_scratch_saturated_stop_cargo",
        "reservation_conflict_dirty_stale_bead",
    ] {
        assert!(
            observed_scenarios.contains(required),
            "missing required fault scenario {required}"
        );
    }
    Ok(())
}

#[test]
fn no_mock_e2e_harness_emits_auditable_replay_evidence() -> TestResult {
    let root = test_workspace("no_mock_e2e")?;
    let source_root = root.join("source_fixture");
    write_no_mock_e2e_sources(&source_root)?;

    let trace = build_swarm_replay_trace(&no_mock_e2e_request(&source_root))?;
    assert_eq!(trace.schema, SWARM_REPLAY_TRACE_SCHEMA);
    assert!(trace.replay_guards.read_only);
    assert!(trace.replay_guards.no_live_mutation);
    assert!(trace.replay_guards.no_network_required);
    assert_eq!(trace.redaction_summary.raw_secret_bytes_emitted, 0);
    assert!(
        trace
            .redaction_summary
            .redacted_fields
            .iter()
            .any(|field| field.contains("body")),
        "Agent Mail bodies must be redacted from the replay trace"
    );

    for source_id in [
        "beads_jsonl",
        "beads_db",
        "agent_mail_archive",
        "doctor_swarm_diagnostics",
        "rch_queue_status",
        "operator_runpack",
        "git_refs",
        "validation_command_records",
        "context_intelligence_evidence",
        "swarm_flight_recorder",
        "swarm_activity_ledger",
    ] {
        let source = source_row(&trace, source_id)?;
        assert_ne!(
            source.availability, "unavailable",
            "source {source_id} should be present in the no-mock fixture"
        );
    }

    let observed_events = event_types(&trace);
    for required in [
        "bead_lifecycle",
        "agent_message",
        "reservation_intent",
        "reservation_conflict",
        "build_slot_state",
        "host_resource_profile",
        "doctor_finding",
        "rch_job_state",
        "runpack_recommendation",
        "operator_handoff",
        "worktree_state",
        "cargo_gate_result",
        "validation_artifact",
    ] {
        assert!(
            observed_events.contains(required),
            "no-mock E2E trace missing event type {required}"
        );
    }
    assert_monotonic_sequence(&trace)?;

    let replay = replay_swarm_trace(&trace)?;
    assert!(replay.replay_guards.consumed_trace_only);
    assert!(replay.replay_guards.no_live_mutation);
    assert!(replay.replay_guards.no_network_required);
    assert_eq!(
        replay.replayed_event_count,
        u64::try_from(trace.events.len())?
    );
    assert!(
        replay
            .final_state
            .runpack_recommendations
            .contains_key("continue_bd_in57w_9"),
        "runpack recommendation was not replayed into final state"
    );
    assert!(
        replay
            .final_state
            .operator_handoffs
            .values()
            .any(|handoff| {
                !handoff.next_actions.is_empty() && !handoff.evidence_paths.is_empty()
            }),
        "operator handoff should carry next actions and evidence paths"
    );

    let policies = [
        SwarmReplayBaselinePolicy::ConservativeManual,
        SwarmReplayBaselinePolicy::ExistingAutopilot,
        SwarmReplayBaselinePolicy::RchFanoutLimited,
        SwarmReplayBaselinePolicy::StaleBeadReclaiming,
        SwarmReplayBaselinePolicy::BuildSlotProtective,
    ];
    let policy_report = evaluate_swarm_replay_baseline_policies(&replay, &policies)?;
    assert_eq!(policy_report.policy_ids.len(), 5);
    assert!(policy_report.policy_guards.advisory_only);
    assert!(policy_report.policy_guards.no_live_mutation);
    assert!(policy_report.policy_guards.no_network_required);

    let claim = decision(&policy_report.decisions, "existing_autopilot", "claim_bead")?;
    assert_eq!(claim.target_id, "bd-e2e-ready");
    assert!(claim.would_require_live_mutation);
    assert!(
        decision(
            &policy_report.decisions,
            "rch_fanout_limited",
            "back_off_cargo",
        )?
        .reason_codes
        .contains(&"rch_queue_position_positive".to_string())
    );
    let stale_scan_claim = decision(
        &policy_report.decisions,
        "stale_bead_reclaiming",
        "claim_bead",
    );
    let stale_scan_claim = stale_scan_claim?;
    assert_eq!(stale_scan_claim.target_id, "bd-e2e-ready");
    assert!(
        stale_scan_claim
            .reason_codes
            .contains(&"ready_bead_available_after_stale_scan".to_string())
    );
    assert_eq!(
        decision(
            &policy_report.decisions,
            "build_slot_protective",
            "wait_for_build_slot",
        )?
        .target_id,
        "cargo-all-targets"
    );

    let comparison_policies = policy_report
        .policy_comparisons
        .iter()
        .map(|comparison| comparison.policy_id.as_str())
        .collect::<BTreeSet<_>>();
    for required in [
        "existing_autopilot",
        "rch_fanout_limited",
        "stale_bead_reclaiming",
    ] {
        assert!(
            comparison_policies.contains(required),
            "comparison report missing policy {required}"
        );
    }
    let rch_delta = policy_report
        .policy_comparisons
        .iter()
        .find(|comparison| comparison.policy_id == "rch_fanout_limited")
        .ok_or("missing rch policy comparison")?;
    assert!(
        rch_delta.metrics.validation_commands_deferred > 0,
        "RCH policy delta should record deferred validation under queue pressure"
    );

    let manifest = write_no_mock_e2e_outputs(&root, &trace, &replay, &policy_report)?;
    let event_log_path = root.join("evidence/replay-events.jsonl");
    let event_log = fs::read_to_string(&event_log_path)?;
    assert!(!event_log.contains("SECRET"));
    assert!(!event_log.contains("Bearer "));
    let replayed_events = event_log
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str::<SwarmReplayEvent>)
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(replayed_events.len(), trace.events.len());

    let entries = manifest
        .get("entries")
        .and_then(Value::as_array)
        .ok_or("manifest entries missing")?;
    for evidence_kind in ["jsonl_event_log", "replay_summary", "comparison_report"] {
        assert!(
            entries.iter().any(|entry| {
                entry
                    .get("evidence_kind")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| kind == evidence_kind)
            }),
            "manifest missing {evidence_kind}"
        );
    }

    let checked_in_paths = [
        "docs/contracts/swarm-operator-runpack-contract.json",
        "tests/golden_corpus/swarm_operator_runpack/complete_runpack_projection.json",
        "tests/e2e_results/20260422T201126Z/replay_bundle.json",
    ];
    for path in checked_in_paths {
        assert!(
            Path::new(env!("CARGO_MANIFEST_DIR")).join(path).exists(),
            "checked-in evidence path referenced by E2E fixture is missing: {path}"
        );
    }
    Ok(())
}

#[test]
fn no_mock_e2e_harness_fails_closed_on_missing_and_malformed_artifacts() -> TestResult {
    let root = test_workspace("no_mock_e2e_negative")?;
    let source_root = root.join("source_fixture");
    write_no_mock_e2e_sources(&source_root)?;

    let missing_runpack = build_swarm_replay_trace(
        &no_mock_e2e_request(&source_root)
            .with_source_override("operator_runpack", "missing/runpack.json"),
    )?;
    let runpack = source_row(&missing_runpack, "operator_runpack")?;
    assert_eq!(runpack.availability, "unavailable");
    assert_eq!(runpack.freshness_state, "missing");
    assert!(
        missing_runpack
            .uncertainty_summary
            .missing_sources
            .contains(&"operator_runpack".to_string())
    );
    assert!(
        missing_runpack
            .uncertainty_summary
            .suppressed_claims
            .contains(&"operator_next_action".to_string())
    );

    let malformed_rch = build_swarm_replay_trace(
        &no_mock_e2e_request(&source_root)
            .with_source_override("rch_queue_status", "negative/malformed-rch.json"),
    )?;
    let rch = source_row(&malformed_rch, "rch_queue_status")?;
    assert_eq!(rch.availability, "malformed");
    assert_eq!(rch.freshness_state, "malformed");
    assert!(
        malformed_rch
            .uncertainty_summary
            .malformed_sources
            .contains(&"rch_queue_status".to_string())
    );
    assert!(
        malformed_rch
            .uncertainty_summary
            .suppressed_claims
            .contains(&"queue_depth".to_string())
    );
    assert!(
        !event_types(&malformed_rch).contains("rch_job_state"),
        "malformed RCH artifact should not produce queue state events"
    );
    Ok(())
}

#[test]
fn replay_engine_orders_events_by_sequence_not_input_order() -> TestResult {
    let later = replay_event(
        "event-a",
        2,
        "2026-05-13T18:00:00Z",
        "bead_lifecycle",
        "beads_jsonl",
        json!({
            "bead_id": "bd-a",
            "to_status": "closed",
            "priority": 3,
            "assignee": "AmberOsprey"
        }),
    );
    let earlier = replay_event(
        "event-b",
        1,
        "2026-05-13T18:00:00Z",
        "bead_lifecycle",
        "beads_jsonl",
        json!({
            "bead_id": "bd-b",
            "to_status": "in_progress",
            "priority": 2,
            "assignee": "SilentReef"
        }),
    );

    let report_a = replay_swarm_trace(&trace_from_events(vec![later.clone(), earlier.clone()]))?;
    let report_b = replay_swarm_trace(&trace_from_events(vec![earlier, later]))?;
    let order_a = report_a
        .snapshots
        .iter()
        .map(|snapshot| snapshot.event_id.clone())
        .collect::<Vec<_>>();
    let order_b = report_b
        .snapshots
        .iter()
        .map(|snapshot| snapshot.event_id.clone())
        .collect::<Vec<_>>();

    assert_eq!(report_a.schema, SWARM_REPLAY_REPORT_SCHEMA);
    assert_eq!(order_a, ["event-b", "event-a"]);
    assert_eq!(order_a, order_b);
    assert_eq!(report_a.final_logical_clock, 2);
    assert_eq!(report_a.final_state.beads["bd-a"].status, "closed");
    Ok(())
}

#[test]
fn replay_engine_skips_duplicate_event_ids_deterministically() -> TestResult {
    let trace = trace_from_events(vec![
        replay_event(
            "same-event",
            1,
            "2026-05-13T18:00:00Z",
            "bead_lifecycle",
            "beads_jsonl",
            json!({
                "bead_id": "bd-dup",
                "to_status": "in_progress",
                "priority": 3,
                "assignee": "AmberOsprey"
            }),
        ),
        replay_event(
            "same-event",
            2,
            "2026-05-13T18:01:00Z",
            "bead_lifecycle",
            "beads_jsonl",
            json!({
                "bead_id": "bd-dup",
                "to_status": "closed",
                "priority": 3,
                "assignee": "AmberOsprey"
            }),
        ),
    ]);

    let report = replay_swarm_trace(&trace)?;
    assert_eq!(report.replayed_event_count, 1);
    assert_eq!(report.final_state.beads["bd-dup"].status, "in_progress");
    assert!(diagnostic_codes(&report).contains("duplicate_event_id_skipped"));
    Ok(())
}

#[test]
fn replay_engine_preserves_logical_clock_for_out_of_order_timestamps() -> TestResult {
    let trace = trace_from_events(vec![
        replay_event(
            "newer",
            1,
            "2026-05-13T18:02:00Z",
            "bead_lifecycle",
            "beads_jsonl",
            json!({
                "bead_id": "bd-time",
                "to_status": "open",
                "priority": 3,
                "assignee": "AmberOsprey"
            }),
        ),
        replay_event(
            "older",
            2,
            "2026-05-13T18:01:00Z",
            "doctor_finding",
            "doctor_swarm_diagnostics",
            json!({
                "finding_id": "old-finding",
                "severity": "info",
                "surface": "swarm",
                "status": "observed"
            }),
        ),
    ]);

    let report = replay_swarm_trace(&trace)?;
    assert_eq!(report.snapshots[0].logical_clock, 1);
    assert_eq!(report.snapshots[1].logical_clock, 2);
    assert!(diagnostic_codes(&report).contains("event_timestamp_regressed"));
    Ok(())
}

#[test]
fn replay_engine_flags_missing_and_impossible_reservation_releases() -> TestResult {
    let missing_release = trace_from_events(vec![replay_event(
        "reservation-active",
        1,
        "2026-05-13T18:00:00Z",
        "reservation_intent",
        "agent_mail_archive",
        json!({
            "reservation_id": "res-1",
            "holder": "AmberOsprey",
            "path_patterns": ["src/swarm_replay.rs"],
            "exclusive": true,
            "state": "active"
        }),
    )]);
    let missing_release_report = replay_swarm_trace(&missing_release)?;
    assert!(
        diagnostic_codes(&missing_release_report).contains("reservation_missing_release_event")
    );

    let impossible_release = trace_from_events(vec![replay_event(
        "reservation-release",
        1,
        "2026-05-13T18:00:00Z",
        "reservation_intent",
        "agent_mail_archive",
        json!({
            "reservation_id": "res-2",
            "holder": "AmberOsprey",
            "path_patterns": ["src/swarm_replay.rs"],
            "exclusive": true,
            "state": "released"
        }),
    )]);
    let impossible_release_report = replay_swarm_trace(&impossible_release)?;
    assert!(
        diagnostic_codes(&impossible_release_report).contains("impossible_reservation_release")
    );
    assert!(!impossible_release_report.final_state.reservations["res-2"].active);
    Ok(())
}

#[test]
fn replay_engine_classifies_stale_rch_progress_and_negative_queue_depth() -> TestResult {
    let mut event = replay_event(
        "rch-stale",
        1,
        "2026-05-13T18:00:00Z",
        "rch_job_state",
        "rch_queue_status",
        json!({
            "job_id": "rch-1",
            "state": "running",
            "worker": "worker-1",
            "command": "rch exec -- cargo check --all-targets",
            "queue_position": -1
        }),
    );
    event.uncertainty = SwarmReplayEventUncertainty {
        state: "partial".to_string(),
        reasons: vec!["source_stale".to_string()],
        suppressed_claims: vec!["queue_depth".to_string()],
    };

    let report = replay_swarm_trace(&trace_from_events(vec![event]))?;
    let codes = diagnostic_codes(&report);
    assert!(codes.contains("rch_progress_from_uncertain_source"));
    assert!(codes.contains("negative_rch_queue_position"));
    assert!(report.final_state.rch_jobs["rch-1"].stale_progress);
    Ok(())
}

#[test]
fn replay_engine_requires_explicit_bead_reopen_evidence() -> TestResult {
    let closed = replay_event(
        "closed",
        1,
        "2026-05-13T18:00:00Z",
        "bead_lifecycle",
        "beads_jsonl",
        json!({
            "bead_id": "bd-reopen",
            "to_status": "closed",
            "priority": 3,
            "assignee": "AmberOsprey"
        }),
    );
    let implicit_reopen = replay_event(
        "implicit-reopen",
        2,
        "2026-05-13T18:01:00Z",
        "bead_lifecycle",
        "beads_jsonl",
        json!({
            "bead_id": "bd-reopen",
            "to_status": "open",
            "priority": 3,
            "assignee": "AmberOsprey"
        }),
    );
    let implicit_report =
        replay_swarm_trace(&trace_from_events(vec![closed.clone(), implicit_reopen]))?;
    assert!(
        diagnostic_codes(&implicit_report).contains("closed_bead_reopened_without_explicit_reopen")
    );

    let explicit_reopen = replay_event(
        "explicit-reopen",
        2,
        "2026-05-13T18:01:00Z",
        "bead_lifecycle",
        "beads_jsonl",
        json!({
            "bead_id": "bd-reopen",
            "to_status": "open",
            "priority": 3,
            "assignee": "AmberOsprey",
            "reopen": true
        }),
    );
    let explicit_report = replay_swarm_trace(&trace_from_events(vec![closed, explicit_reopen]))?;
    assert!(
        !diagnostic_codes(&explicit_report)
            .contains("closed_bead_reopened_without_explicit_reopen")
    );
    assert_eq!(
        explicit_report.final_state.beads["bd-reopen"].status,
        "open"
    );
    Ok(())
}

#[test]
fn replay_engine_classifies_agent_mail_outage_without_live_mail() -> TestResult {
    let event = uncertain_replay_event(
        "missing-mail",
        1,
        "agent_message",
        "agent_mail_archive",
        &["source_missing"],
        &["mail_thread_completeness", "active_reservation_holder"],
        json!({
            "thread_id": "unknown",
            "sender": "unknown",
            "recipients": [],
            "importance": "unknown",
            "ack_required": false
        }),
    );

    let report = replay_swarm_trace(&trace_from_events(vec![event]))?;
    assert!(!report.final_state.coordination.agent_mail_available);
    assert!(report.final_state.coordination.missing_agent_mail_evidence);
    assert!(diagnostic_codes(&report).contains("agent_mail_source_unavailable"));
    assert!(report.replay_guards.consumed_trace_only);
    Ok(())
}

#[test]
fn metamorphic_swarm_replay_equivalence_transforms_emit_jsonl() -> TestResult {
    let root = test_workspace("metamorphic_swarm")?;
    let mut comparison_records = Vec::new();

    let low_value_original = with_event(
        with_event(
            metamorphic_semantic_events(),
            replay_event(
                "mr-low-value-progress-a",
                15,
                "2026-05-13T18:00:30Z",
                "doctor_finding",
                "doctor_swarm_diagnostics",
                json!({
                    "finding_id": "progress-a",
                    "severity": "info",
                    "surface": "swarm",
                    "status": "observed"
                }),
            ),
        ),
        replay_event(
            "mr-low-value-progress-b",
            25,
            "2026-05-13T18:01:30Z",
            "validation_artifact",
            "swarm_flight_recorder",
            json!({
                "artifact_id": "progress-b",
                "class": "low_value_progress",
                "semantic": false
            }),
        ),
    );
    let mut low_value_coalesced = with_event(
        metamorphic_semantic_events(),
        replay_event(
            "mr-low-value-coalesced",
            15,
            "2026-05-13T18:00:30Z",
            "validation_artifact",
            "swarm_flight_recorder",
            json!({
                "artifact_id": "coalesced-progress",
                "class": "low_value_progress",
                "coalesced_event_ids": [
                    "mr-low-value-progress-a",
                    "mr-low-value-progress-b"
                ],
                "semantic": false
            }),
        ),
    );
    low_value_coalesced.reverse();
    let low_value_original_report = replay_swarm_trace(&trace_from_events(low_value_original))?;
    let low_value_coalesced_report = replay_swarm_trace(&trace_from_events(low_value_coalesced))?;
    assert!(push_metamorphic_comparison(
        &mut comparison_records,
        "low_value_event_reorder_coalesce",
        "fixture-metamorphic-low-value",
        &low_value_original_report,
        &low_value_coalesced_report,
        true,
    ));

    let compaction_before = with_event(
        with_event(
            metamorphic_semantic_events(),
            replay_event(
                "mr-compaction-before-tool",
                18,
                "2026-05-13T18:00:45Z",
                "validation_artifact",
                "swarm_activity_ledger",
                json!({
                    "artifact_id": "compaction-before-tool",
                    "transform_role": "compaction_boundary",
                    "compacted_entries": 3
                }),
            ),
        ),
        replay_event(
            "mr-compaction-after-handoff",
            42,
            "2026-05-13T18:03:30Z",
            "validation_artifact",
            "swarm_activity_ledger",
            json!({
                "artifact_id": "compaction-after-handoff",
                "transform_role": "compaction_boundary",
                "compacted_entries": 5
            }),
        ),
    );
    let compaction_shifted = with_event(
        with_event(
            metamorphic_semantic_events(),
            replay_event(
                "mr-compaction-before-bead",
                8,
                "2026-05-13T17:59:45Z",
                "validation_artifact",
                "swarm_activity_ledger",
                json!({
                    "artifact_id": "compaction-before-bead",
                    "transform_role": "compaction_boundary",
                    "compacted_entries": 3
                }),
            ),
        ),
        replay_event(
            "mr-compaction-before-close",
            55,
            "2026-05-13T18:04:30Z",
            "validation_artifact",
            "swarm_activity_ledger",
            json!({
                "artifact_id": "compaction-before-close",
                "transform_role": "compaction_boundary",
                "compacted_entries": 5
            }),
        ),
    );
    let compaction_before_report = replay_swarm_trace(&trace_from_events(compaction_before))?;
    let compaction_shifted_report = replay_swarm_trace(&trace_from_events(compaction_shifted))?;
    assert!(push_metamorphic_comparison(
        &mut comparison_records,
        "compaction_boundary_shift",
        "fixture-metamorphic-compaction",
        &compaction_before_report,
        &compaction_shifted_report,
        true,
    ));

    let branch_a = with_event(
        metamorphic_semantic_events(),
        replay_event(
            "mr-branch-a-start",
            5,
            "2026-05-13T17:59:00Z",
            "worktree_state",
            "git_refs",
            json!({
                "head": "branch-a-head",
                "branch": "feature/metamorphic-a",
                "dirty": true,
                "changed_paths": ["tests/swarm_replay_ingestor.rs"]
            }),
        ),
    );
    let branch_b = with_event(
        with_event(
            metamorphic_semantic_events(),
            replay_event(
                "mr-branch-b-start",
                5,
                "2026-05-13T17:59:00Z",
                "worktree_state",
                "git_refs",
                json!({
                    "head": "branch-b-head",
                    "branch": "feature/metamorphic-b",
                    "dirty": true,
                    "changed_paths": ["tests/swarm_replay_ingestor.rs"]
                }),
            ),
        ),
        replay_event(
            "mr-branch-b-summary",
            35,
            "2026-05-13T18:02:30Z",
            "validation_artifact",
            "swarm_activity_ledger",
            json!({
                "artifact_id": "branch-b-summary",
                "branch": "feature/metamorphic-b",
                "final_semantic_marker": "handoff-metamorphic"
            }),
        ),
    );
    let primary_replay_report = replay_swarm_trace(&trace_from_events(branch_a))?;
    let replay_with_branch_summary_report = replay_swarm_trace(&trace_from_events(branch_b))?;
    assert!(push_metamorphic_comparison(
        &mut comparison_records,
        "branch_replay_identical_final_markers",
        "fixture-metamorphic-branch-replay",
        &primary_replay_report,
        &replay_with_branch_summary_report,
        true,
    ));

    let mut non_equivalent_events = metamorphic_semantic_events();
    let last_event = non_equivalent_events
        .last_mut()
        .ok_or("missing final metamorphic event")?;
    let last_payload = last_event
        .payload
        .as_object_mut()
        .ok_or("missing final metamorphic payload object")?;
    last_payload.insert("to_status".to_owned(), json!("in_progress"));
    let non_equivalent_report = replay_swarm_trace(&trace_from_events(non_equivalent_events))?;
    assert!(!push_metamorphic_comparison(
        &mut comparison_records,
        "negative_non_equivalent_final_bead_state",
        "fixture-metamorphic-negative-control",
        &primary_replay_report,
        &non_equivalent_report,
        false,
    ));

    write_jsonl_rows(
        &root,
        "evidence/metamorphic-comparison.jsonl",
        &comparison_records,
    )?;
    let comparison_log = fs::read_to_string(root.join("evidence/metamorphic-comparison.jsonl"))?;
    for required_fragment in [
        "low_value_event_reorder_coalesce",
        "compaction_boundary_shift",
        "branch_replay_identical_final_markers",
        "negative_non_equivalent_final_bead_state",
        "mismatch_reason",
        "compared_state_fields",
    ] {
        assert!(
            comparison_log.contains(required_fragment),
            "missing metamorphic log fragment: {required_fragment}"
        );
    }
    Ok(())
}

#[test]
fn policy_runner_is_deterministic_and_advisory_only() -> TestResult {
    let trace = trace_from_events(vec![replay_event(
        "ready-bead",
        1,
        "2026-05-13T18:00:00Z",
        "bead_lifecycle",
        "beads_jsonl",
        json!({
            "bead_id": "bd-ready",
            "to_status": "open",
            "priority": 3,
            "assignee": "unassigned"
        }),
    )]);
    let replay = replay_swarm_trace(&trace)?;
    let policies = default_swarm_replay_baseline_policies();

    let first = evaluate_swarm_replay_baseline_policies(&replay, &policies)?;
    let second = evaluate_swarm_replay_baseline_policies(&replay, &policies)?;

    assert_eq!(first, second);
    assert_eq!(first.schema, SWARM_REPLAY_POLICY_REPORT_SCHEMA);
    assert!(first.policy_guards.advisory_only);
    assert!(first.policy_guards.no_live_mutation);
    assert!(first.policy_guards.no_network_required);
    assert!(first.policy_guards.consumed_replay_report_only);
    assert_eq!(
        first.policy_ids,
        [
            "build_slot_protective",
            "conservative_manual",
            "existing_autopilot",
            "rch_fanout_limited",
            "stale_bead_reclaiming"
        ]
    );
    assert!(first.decisions.iter().all(|item| item.advisory_only));
    assert!(
        first
            .decisions
            .iter()
            .all(|item| !item.reason_codes.is_empty())
    );
    assert!(
        first
            .decisions
            .iter()
            .all(|item| !item.source_evidence.is_empty())
    );
    Ok(())
}

#[test]
fn policy_report_includes_golden_comparison_metrics() -> TestResult {
    let trace = trace_from_events(vec![
        replay_event(
            "ready-bead",
            1,
            "2026-05-13T18:00:00Z",
            "bead_lifecycle",
            "beads_jsonl",
            json!({
                "bead_id": "bd-compare",
                "to_status": "open",
                "priority": 2,
                "assignee": "unassigned"
            }),
        ),
        replay_event(
            "reservation-conflict",
            2,
            "2026-05-13T18:02:00Z",
            "reservation_conflict",
            "agent_mail_archive",
            json!({
                "path_pattern": "src/swarm_replay.rs",
                "holder": "OtherAgent",
                "conflict_reason": "active exclusive lease"
            }),
        ),
        replay_event(
            "rch-queued",
            3,
            "2026-05-13T18:05:00Z",
            "rch_job_state",
            "rch_queue_status",
            json!({
                "job_id": "rch-queued",
                "state": "queued",
                "worker": "worker-1",
                "command": "rch exec -- cargo check --all-targets",
                "queue_position": 3
            }),
        ),
        replay_event(
            "operator-handoff",
            4,
            "2026-05-13T18:09:00Z",
            "operator_handoff",
            "operator_runpack",
            json!({
                "handoff_id": "handoff-compare",
                "summary": "continue via beads while mail is unavailable",
                "next_actions": ["continue bd-compare"],
                "evidence_paths": ["docs/evidence/swarm-operator-runpack.json"]
            }),
        ),
        replay_event(
            "closed-bead",
            5,
            "2026-05-13T18:14:00Z",
            "bead_lifecycle",
            "beads_jsonl",
            json!({
                "bead_id": "bd-compare",
                "to_status": "closed",
                "priority": 2,
                "assignee": "Codex"
            }),
        ),
    ]);
    let replay = replay_swarm_trace(&trace)?;
    let policies = [
        SwarmReplayBaselinePolicy::ConservativeManual,
        SwarmReplayBaselinePolicy::ExistingAutopilot,
        SwarmReplayBaselinePolicy::RchFanoutLimited,
    ];
    let report = evaluate_swarm_replay_baseline_policies(&replay, &policies)?;

    assert_eq!(report.comparison_count, 3);
    assert_eq!(
        report
            .policy_comparisons
            .iter()
            .map(|row| row.policy_id.as_str())
            .collect::<Vec<_>>(),
        [
            "existing_autopilot",
            "rch_fanout_limited",
            "conservative_manual"
        ]
    );

    let observed = serde_json::to_value(&report.policy_comparisons)?;
    let expected =
        load_json("tests/golden_corpus/swarm_replay_trace/policy_comparison_metrics.json")?;
    assert_eq!(observed, expected);
    Ok(())
}

#[test]
fn policy_comparison_suppresses_latency_when_timestamps_are_missing() -> TestResult {
    let trace = trace_from_events(vec![
        replay_event(
            "missing-time",
            1,
            "not-a-timestamp",
            "bead_lifecycle",
            "beads_jsonl",
            json!({
                "bead_id": "bd-missing-time",
                "to_status": "open",
                "priority": 2,
                "assignee": "unassigned"
            }),
        ),
        replay_event(
            "normal-time",
            2,
            "2026-05-13T18:01:00Z",
            "rch_job_state",
            "rch_queue_status",
            json!({
                "job_id": "rch-missing-time",
                "state": "queued",
                "worker": "worker-1",
                "command": "rch exec -- cargo check --all-targets",
                "queue_position": 2
            }),
        ),
    ]);
    let replay = replay_swarm_trace(&trace)?;
    let report = evaluate_swarm_replay_baseline_policies(
        &replay,
        &[SwarmReplayBaselinePolicy::RchFanoutLimited],
    )?;
    let comparison = report
        .policy_comparisons
        .iter()
        .find(|row| row.policy_id == "rch_fanout_limited")
        .ok_or("missing rch comparison")?;

    assert_eq!(comparison.metrics.blocked_time_minutes, None);
    assert_eq!(comparison.metrics.average_wait_minutes, None);
    assert_eq!(comparison.metrics.p95_wait_minutes, None);
    assert!(
        comparison
            .missing_data
            .iter()
            .any(|missing| missing.claim == "latency_claims")
    );
    assert!(
        comparison
            .confidence
            .reasons
            .contains(&"missing_data_suppressed_claims".to_string())
    );
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct ResourceProfileFixture<'a> {
    profile_id: &'a str,
    cpu_cores: u64,
    memory_gib: u64,
    target_free_gib: u64,
    tmpdir_free_gib: u64,
    extension_hostcall_lanes: u64,
    rch_worker_slots: u64,
    numa_hint: &'a str,
}

fn resource_profile_payload(profile: ResourceProfileFixture<'_>) -> Value {
    json!({
        "profile_id": profile.profile_id,
        "cpu_cores": profile.cpu_cores,
        "memory_gib": profile.memory_gib,
        "numa_nodes": if profile.memory_gib >= 256 { 4 } else { 1 },
        "cgroup_cpu_quota": profile.cpu_cores,
        "cgroup_memory_gib": profile.memory_gib,
        "max_agent_concurrency": profile.extension_hostcall_lanes,
        "max_tool_concurrency": profile.rch_worker_slots,
        "extension_hostcall_lanes": profile.extension_hostcall_lanes,
        "rch_worker_slots": profile.rch_worker_slots,
        "target_dir": format!(
            "/data/tmp/pi_agent_rust_cargo/{profile_id}/target",
            profile_id = profile.profile_id
        ),
        "target_free_gib": profile.target_free_gib,
        "tmpdir": format!(
            "/data/tmp/pi_agent_rust_cargo/{profile_id}/tmp",
            profile_id = profile.profile_id
        ),
        "tmpdir_free_gib": profile.tmpdir_free_gib,
        "numa_hint": profile.numa_hint
    })
}

fn pressure_trace_for_profile(profile: Value) -> SwarmReplayTrace {
    trace_from_events(vec![
        replay_event(
            "resource-profile",
            1,
            "2026-05-13T18:00:00Z",
            "host_resource_profile",
            "synthetic_host_profile",
            profile,
        ),
        replay_event_with_actor(
            "AgentOne",
            "agent-one",
            2,
            "2026-05-13T18:01:00Z",
            "agent_message",
            "agent_mail_archive",
            json!({
                "thread_id": "bd-resource",
                "sender": "AgentOne",
                "recipients": ["AgentTwo"],
                "importance": "normal",
                "ack_required": false
            }),
        ),
        replay_event_with_actor(
            "AgentTwo",
            "agent-two",
            3,
            "2026-05-13T18:02:00Z",
            "agent_message",
            "agent_mail_archive",
            json!({
                "thread_id": "bd-resource",
                "sender": "AgentTwo",
                "recipients": ["AgentThree"],
                "importance": "normal",
                "ack_required": false
            }),
        ),
        replay_event_with_actor(
            "AgentThree",
            "agent-three",
            4,
            "2026-05-13T18:03:00Z",
            "agent_message",
            "agent_mail_archive",
            json!({
                "thread_id": "bd-resource",
                "sender": "AgentThree",
                "recipients": ["AgentOne"],
                "importance": "normal",
                "ack_required": false
            }),
        ),
        replay_event(
            "build-slot",
            5,
            "2026-05-13T18:04:00Z",
            "build_slot_state",
            "agent_mail_archive",
            json!({
                "slot": "cargo-all-targets",
                "holder": "AgentOne",
                "state": "active",
                "expires_at_utc": "2026-05-13T19:00:00Z"
            }),
        ),
        replay_event(
            "rch-queued",
            6,
            "2026-05-13T18:05:00Z",
            "rch_job_state",
            "rch_queue_status",
            json!({
                "job_id": "rch-pressure",
                "state": "queued",
                "worker": "worker-1",
                "command": "rch exec -- cargo check --all-targets",
                "queue_position": 3
            }),
        ),
    ])
}

fn push_large_trace_event(
    events: &mut Vec<SwarmReplayEvent>,
    sequence: &mut u64,
    actor: &str,
    event_id: impl Into<String>,
    event_type: &str,
    source_ref: &str,
    payload: Value,
) {
    let event_id = event_id.into();
    events.push(replay_event_with_actor(
        actor,
        &event_id,
        *sequence,
        "2026-05-13T19:00:00Z",
        event_type,
        source_ref,
        payload,
    ));
    *sequence = sequence.saturating_add(1);
}

fn large_replay_performance_trace() -> SwarmReplayTrace {
    let mut events = Vec::new();
    let mut sequence = 1_u64;

    push_large_trace_event(
        &mut events,
        &mut sequence,
        "doctor",
        "large-resource-profile",
        "host_resource_profile",
        "synthetic_host_profile",
        resource_profile_payload(ResourceProfileFixture {
            profile_id: "large-trace-ci-host",
            cpu_cores: 64,
            memory_gib: 256,
            target_free_gib: 512,
            tmpdir_free_gib: 512,
            extension_hostcall_lanes: 96,
            rch_worker_slots: 96,
            numa_hint: "pin_rch_workers_by_socket",
        }),
    );

    for index in 0_u64..64 {
        let agent = format!("Agent{index:03}");
        push_large_trace_event(
            &mut events,
            &mut sequence,
            &agent,
            format!("large-agent-message-{index:03}"),
            "agent_message",
            "agent_mail_archive",
            json!({
                "thread_id": "bd-large-trace",
                "sender": agent,
                "recipients": ["Coordinator"],
                "importance": "normal",
                "ack_required": false
            }),
        );
    }

    for bead_index in 0_u64..120 {
        let priority = i64::try_from(bead_index % 5).unwrap_or(0);
        for (status, step) in [
            ("open", "ready"),
            ("in_progress", "claimed"),
            ("closed", "closed"),
        ] {
            push_large_trace_event(
                &mut events,
                &mut sequence,
                "Beads",
                format!("large-bead-{bead_index:03}-{step}"),
                "bead_lifecycle",
                "beads_jsonl",
                json!({
                    "bead_id": format!("bd-large-{bead_index:03}"),
                    "to_status": status,
                    "priority": priority,
                    "assignee": format!("Agent{:03}", bead_index % 64)
                }),
            );
        }
    }

    for job_index in 0_u64..64 {
        push_large_trace_event(
            &mut events,
            &mut sequence,
            "rch",
            format!("large-rch-job-{job_index:03}"),
            "rch_job_state",
            "rch_queue_status",
            json!({
                "job_id": format!("rch-large-{job_index:03}"),
                "state": "queued",
                "worker": format!("worker-{}", job_index % 8),
                "command": "rch exec -- cargo test --test swarm_replay_ingestor",
                "queue_position": (job_index % 4) + 1
            }),
        );
    }

    for reservation_index in 0_u64..48 {
        let reservation_id = format!("res-large-{reservation_index:03}");
        push_large_trace_event(
            &mut events,
            &mut sequence,
            "AgentMail",
            format!("large-reservation-{reservation_index:03}-active"),
            "reservation_intent",
            "agent_mail_archive",
            json!({
                "reservation_id": reservation_id,
                "path_patterns": [format!("src/replay_surface_{reservation_index:03}.rs")],
                "exclusive": true,
                "ttl_seconds": 3600,
                "reason": "bd-in57w.10",
                "holder": format!("Agent{:03}", reservation_index % 64),
                "state": "active"
            }),
        );
        push_large_trace_event(
            &mut events,
            &mut sequence,
            "AgentMail",
            format!("large-reservation-{reservation_index:03}-released"),
            "reservation_intent",
            "agent_mail_archive",
            json!({
                "reservation_id": reservation_id,
                "path_patterns": [format!("src/replay_surface_{reservation_index:03}.rs")],
                "exclusive": true,
                "ttl_seconds": 3600,
                "reason": "bd-in57w.10",
                "holder": format!("Agent{:03}", reservation_index % 64),
                "state": "released"
            }),
        );
    }

    trace_from_events(events)
}

#[test]
fn resource_budget_profiles_model_small_ci_large_hosts() -> TestResult {
    let small = replay_swarm_trace(&pressure_trace_for_profile(resource_profile_payload(
        ResourceProfileFixture {
            profile_id: "small-laptop",
            cpu_cores: 4,
            memory_gib: 8,
            target_free_gib: 8,
            tmpdir_free_gib: 8,
            extension_hostcall_lanes: 2,
            rch_worker_slots: 1,
            numa_hint: "single_socket",
        },
    )))?;
    let small_peak = small
        .resource_pressure_timeline
        .last()
        .ok_or("missing small peak")?;
    assert_eq!(small_peak.cpu_pressure, "saturated");
    assert_eq!(small_peak.memory_pressure, "saturated");
    assert_eq!(small_peak.tmpdir_pressure, "saturated");
    assert_eq!(small_peak.target_dir_pressure, "saturated");
    assert_eq!(small_peak.rch_worker_pressure, "saturated");
    assert_eq!(small_peak.extension_lane_pressure, "saturated");
    assert!(
        small_peak
            .saturation_reasons
            .contains(&"cpu_saturated".to_string())
    );

    let ci = replay_swarm_trace(&pressure_trace_for_profile(resource_profile_payload(
        ResourceProfileFixture {
            profile_id: "default-ci",
            cpu_cores: 8,
            memory_gib: 16,
            target_free_gib: 64,
            tmpdir_free_gib: 64,
            extension_hostcall_lanes: 4,
            rch_worker_slots: 4,
            numa_hint: "shared_runner",
        },
    )))?;
    let ci_peak = ci
        .resource_pressure_timeline
        .last()
        .ok_or("missing ci peak")?;
    assert_eq!(ci_peak.cpu_pressure, "saturated");
    assert_eq!(ci_peak.memory_pressure, "high");
    assert_eq!(ci_peak.tmpdir_pressure, "low");
    assert_eq!(ci_peak.target_dir_pressure, "low");
    assert_eq!(ci_peak.rch_worker_pressure, "high");

    let large_64 = replay_swarm_trace(&pressure_trace_for_profile(resource_profile_payload(
        ResourceProfileFixture {
            profile_id: "large-64-core",
            cpu_cores: 64,
            memory_gib: 128,
            target_free_gib: 256,
            tmpdir_free_gib: 256,
            extension_hostcall_lanes: 16,
            rch_worker_slots: 8,
            numa_hint: "spread_agents_across_sockets",
        },
    )))?;
    let large_peak = large_64
        .resource_pressure_timeline
        .last()
        .ok_or("missing large peak")?;
    assert_eq!(large_peak.cpu_pressure, "low");
    assert_eq!(large_peak.memory_pressure, "low");
    assert_eq!(large_peak.rch_worker_pressure, "low");

    let huge = replay_swarm_trace(&pressure_trace_for_profile(resource_profile_payload(
        ResourceProfileFixture {
            profile_id: "huge-256gb",
            cpu_cores: 64,
            memory_gib: 256,
            target_free_gib: 512,
            tmpdir_free_gib: 512,
            extension_hostcall_lanes: 32,
            rch_worker_slots: 16,
            numa_hint: "pin_rch_workers_by_socket",
        },
    )))?;
    let huge_profile = huge
        .final_state
        .resource_budget
        .as_ref()
        .ok_or("missing huge profile")?;
    assert_eq!(huge_profile.memory_gib, Some(256));
    assert_eq!(huge_profile.numa_nodes, Some(4));
    assert_eq!(huge_profile.numa_hint, "pin_rch_workers_by_socket");
    assert!(
        huge.resource_pressure_timeline
            .iter()
            .all(|snapshot| snapshot.saturation_reasons.is_empty())
    );
    Ok(())
}

#[test]
fn large_trace_performance_evidence_passes_ci_budget() -> TestResult {
    let trace = large_replay_performance_trace();
    let replay = replay_swarm_trace(&trace)?;
    assert!(replay.replayed_event_count >= 500);

    let policy_report = evaluate_swarm_replay_baseline_policies(
        &replay,
        &default_swarm_replay_baseline_policies(),
    )?;
    let budget = SwarmReplayPerformanceBudget {
        budget_id: "ci-large-trace-smoke".to_string(),
        min_replayed_events: 500,
        max_wall_time_ms: 5_000,
        max_peak_rss_gib: 256,
        max_report_size_bytes: 256 * 1024 * 1024,
        max_ordering_cost_units: swarm_replay_ordering_cost_units(replay.replayed_event_count)
            .saturating_add(10),
    };
    let observation = SwarmReplayPerformanceObservation {
        command: "rch exec -- cargo test --test swarm_replay_ingestor large_trace_performance_evidence_passes_ci_budget".to_string(),
        runner: "rch".to_string(),
        host_profile_id: Some("large-trace-ci-host".to_string()),
        wall_time_ms: Some(900),
        peak_rss_gib: Some(32),
        caveats: vec!["ci_smoke_not_release_certification".to_string()],
    };

    let evidence = build_swarm_replay_performance_evidence(
        &replay,
        Some(&policy_report),
        budget,
        observation,
    )?;
    assert_eq!(evidence.schema, SWARM_REPLAY_PERFORMANCE_EVIDENCE_SCHEMA);
    assert_eq!(evidence.verdict, "pass");
    assert_eq!(
        evidence.observed.replayed_event_count,
        u64::try_from(trace.events.len())?
    );
    assert_eq!(
        evidence.observed.policy_decision_count,
        policy_report.decision_count
    );
    assert!(evidence.observed.report_size_bytes.unwrap_or_default() > 0);
    assert!(evidence.checks.iter().all(|check| check.status == "pass"));
    assert!(
        evidence
            .caveats
            .contains(&"ci_smoke_not_release_certification".to_string())
    );
    assert!(evidence.guards.evidence_only);
    assert!(evidence.guards.release_claims_suppressed);

    let evidence_json = serde_json::to_value(&evidence)?;
    assert_eq!(
        evidence_json["schema"],
        json!(SWARM_REPLAY_PERFORMANCE_EVIDENCE_SCHEMA)
    );
    assert_eq!(evidence_json["budget"]["budget_id"], "ci-large-trace-smoke");
    Ok(())
}

#[test]
fn performance_evidence_fails_closed_without_wall_time_or_rss() -> TestResult {
    let replay = replay_swarm_trace(&pressure_trace_for_profile(resource_profile_payload(
        ResourceProfileFixture {
            profile_id: "default-ci",
            cpu_cores: 8,
            memory_gib: 16,
            target_free_gib: 64,
            tmpdir_free_gib: 64,
            extension_hostcall_lanes: 4,
            rch_worker_slots: 4,
            numa_hint: "shared_runner",
        },
    )))?;
    let budget = SwarmReplayPerformanceBudget {
        budget_id: "ci-missing-observation".to_string(),
        min_replayed_events: 1,
        max_wall_time_ms: 1_000,
        max_peak_rss_gib: 64,
        max_report_size_bytes: 64 * 1024 * 1024,
        max_ordering_cost_units: 1_000,
    };
    let observation = SwarmReplayPerformanceObservation {
        command: "rch exec -- cargo test --test swarm_replay_ingestor".to_string(),
        runner: "rch".to_string(),
        host_profile_id: Some("default-ci".to_string()),
        wall_time_ms: None,
        peak_rss_gib: None,
        caveats: Vec::new(),
    };

    let evidence = build_swarm_replay_performance_evidence(&replay, None, budget, observation)?;
    assert_eq!(evidence.verdict, "fail");
    assert!(
        evidence
            .checks
            .iter()
            .any(|check| { check.metric == "wall_time_ms" && check.status == "missing" })
    );
    assert!(
        evidence
            .checks
            .iter()
            .any(|check| { check.metric == "peak_rss_gib" && check.status == "missing" })
    );
    assert!(
        evidence
            .missing_data
            .iter()
            .any(|reason| reason.contains("wall_time_ms observation missing"))
    );
    assert!(
        evidence
            .caveats
            .contains(&"policy_report_not_supplied".to_string())
    );
    Ok(())
}

#[test]
fn policy_comparison_suppresses_resource_claims_without_host_facts() -> TestResult {
    let trace = trace_from_events(vec![replay_event(
        "ready-bead",
        1,
        "2026-05-13T18:00:00Z",
        "bead_lifecycle",
        "beads_jsonl",
        json!({
            "bead_id": "bd-no-host-facts",
            "to_status": "open",
            "priority": 2,
            "assignee": "unassigned"
        }),
    )]);
    let replay = replay_swarm_trace(&trace)?;
    let report = evaluate_swarm_replay_baseline_policies(
        &replay,
        &[SwarmReplayBaselinePolicy::ExistingAutopilot],
    )?;
    let comparison = report
        .policy_comparisons
        .iter()
        .find(|row| row.policy_id == "existing_autopilot")
        .ok_or("missing autopilot comparison")?;

    assert_eq!(comparison.metrics.resource_budget.profile_id, None);
    assert_eq!(comparison.metrics.resource_budget.cpu_pressure, "unknown");
    assert!(comparison.missing_data.iter().any(|missing| {
        missing.claim == "resource_budget_claims"
            && missing
                .reasons
                .contains(&"host resource profile missing".to_string())
    }));
    Ok(())
}

#[test]
fn baseline_policies_disagree_when_agent_mail_is_unavailable() -> TestResult {
    let trace = trace_from_events(vec![
        uncertain_replay_event(
            "missing-mail",
            1,
            "agent_message",
            "agent_mail_archive",
            &["source_missing"],
            &["mail_thread_completeness"],
            json!({
                "thread_id": "unknown",
                "sender": "unknown",
                "recipients": [],
                "importance": "unknown",
                "ack_required": false
            }),
        ),
        replay_event(
            "ready-bead",
            2,
            "2026-05-13T18:01:00Z",
            "bead_lifecycle",
            "beads_jsonl",
            json!({
                "bead_id": "bd-mail-red",
                "to_status": "open",
                "priority": 2,
                "assignee": "unassigned"
            }),
        ),
    ]);
    let replay = replay_swarm_trace(&trace)?;
    let policies = [
        SwarmReplayBaselinePolicy::ConservativeManual,
        SwarmReplayBaselinePolicy::ExistingAutopilot,
        SwarmReplayBaselinePolicy::StaleBeadReclaiming,
    ];
    let report = evaluate_swarm_replay_baseline_policies(&replay, &policies)?;

    let manual = decision(&report.decisions, "conservative_manual", "handoff")?;
    assert_eq!(manual.target_id, "agent_mail");
    assert!(
        manual
            .reason_codes
            .contains(&"agent_mail_unavailable_requires_manual_coordination".to_string())
    );

    let autopilot = decision(&report.decisions, "existing_autopilot", "claim_bead")?;
    assert_eq!(autopilot.target_id, "bd-mail-red");
    assert!(autopilot.would_require_live_mutation);
    assert!(
        autopilot
            .reason_codes
            .contains(&"agent_mail_unavailable_continue_via_beads".to_string())
    );

    let reclaiming = decision(
        &report.decisions,
        "stale_bead_reclaiming",
        "refresh_evidence",
    )?;
    assert_eq!(reclaiming.target_id, "agent_mail");
    assert!(!reclaiming.would_require_live_mutation);
    Ok(())
}

#[test]
fn baseline_policies_disagree_under_rch_queue_pressure() -> TestResult {
    let trace = trace_from_events(vec![
        replay_event(
            "ready-bead",
            1,
            "2026-05-13T18:00:00Z",
            "bead_lifecycle",
            "beads_jsonl",
            json!({
                "bead_id": "bd-rch-pressure",
                "to_status": "open",
                "priority": 2,
                "assignee": "unassigned"
            }),
        ),
        replay_event(
            "rch-queued",
            2,
            "2026-05-13T18:01:00Z",
            "rch_job_state",
            "rch_queue_status",
            json!({
                "job_id": "rch-queued",
                "state": "queued",
                "worker": "worker-1",
                "command": "rch exec -- cargo check --all-targets",
                "queue_position": 4
            }),
        ),
    ]);
    let replay = replay_swarm_trace(&trace)?;
    let policies = [
        SwarmReplayBaselinePolicy::ExistingAutopilot,
        SwarmReplayBaselinePolicy::RchFanoutLimited,
        SwarmReplayBaselinePolicy::BuildSlotProtective,
    ];
    let report = evaluate_swarm_replay_baseline_policies(&replay, &policies)?;

    let autopilot = decision(&report.decisions, "existing_autopilot", "claim_bead")?;
    assert_eq!(autopilot.target_id, "bd-rch-pressure");

    let limited = decision(&report.decisions, "rch_fanout_limited", "back_off_cargo")?;
    assert_eq!(limited.target_id, "rch-queued");
    assert!(
        limited
            .reason_codes
            .contains(&"rch_queue_position_positive".to_string())
    );

    let protective = decision(&report.decisions, "build_slot_protective", "back_off_cargo")?;
    assert_eq!(protective.target_id, "rch-queued");
    assert!(
        protective
            .reason_codes
            .contains(&"rch_pressure_protects_build_capacity".to_string())
    );
    Ok(())
}

#[test]
fn baseline_policies_disagree_under_dirty_worktree_contention() -> TestResult {
    let trace = trace_from_events(vec![
        replay_event(
            "dirty-worktree",
            1,
            "2026-05-13T18:00:00Z",
            "worktree_state",
            "git_refs",
            json!({
                "head": "abc123",
                "branch": "main",
                "dirty": true,
                "changed_paths": ["tests/release_evidence_gate.rs"]
            }),
        ),
        replay_event(
            "ready-bead",
            2,
            "2026-05-13T18:01:00Z",
            "bead_lifecycle",
            "beads_jsonl",
            json!({
                "bead_id": "bd-dirty",
                "to_status": "open",
                "priority": 3,
                "assignee": "unassigned"
            }),
        ),
    ]);
    let replay = replay_swarm_trace(&trace)?;
    let policies = default_swarm_replay_baseline_policies();
    let report = evaluate_swarm_replay_baseline_policies(&replay, &policies)?;

    let worktree = replay
        .final_state
        .worktree
        .as_ref()
        .ok_or("missing worktree")?;
    assert!(worktree.dirty);
    assert_eq!(worktree.changed_paths, ["tests/release_evidence_gate.rs"]);

    assert!(
        decision(&report.decisions, "conservative_manual", "wait")?
            .reason_codes
            .contains(&"dirty_worktree_requires_manual_review".to_string())
    );
    assert!(
        decision(&report.decisions, "existing_autopilot", "wait")?
            .reason_codes
            .contains(&"dirty_worktree_contention".to_string())
    );
    assert!(
        decision(&report.decisions, "rch_fanout_limited", "wait")?
            .reason_codes
            .contains(&"dirty_worktree_avoid_validation_fanout".to_string())
    );
    assert_eq!(
        decision(&report.decisions, "stale_bead_reclaiming", "claim_bead")?.target_id,
        "bd-dirty"
    );
    Ok(())
}

#[test]
fn stale_bead_reclaiming_flags_absent_assignee() -> TestResult {
    let trace = trace_from_events(vec![replay_event(
        "stale-bead",
        1,
        "2026-05-13T18:00:00Z",
        "bead_lifecycle",
        "beads_jsonl",
        json!({
            "bead_id": "bd-stale",
            "to_status": "in_progress",
            "priority": 1,
            "assignee": "LongGoneAgent"
        }),
    )]);
    let replay = replay_swarm_trace(&trace)?;
    let report = evaluate_swarm_replay_baseline_policies(
        &replay,
        &[SwarmReplayBaselinePolicy::StaleBeadReclaiming],
    )?;

    let reclaim = decision(
        &report.decisions,
        "stale_bead_reclaiming",
        "reclaim_stale_bead",
    )?;
    assert_eq!(reclaim.target_id, "bd-stale");
    assert!(reclaim.would_require_live_mutation);
    assert!(
        reclaim
            .reason_codes
            .contains(&"in_progress_assignee_absent_from_replay_agents".to_string())
    );
    Ok(())
}
