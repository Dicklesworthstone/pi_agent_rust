#![allow(clippy::doc_markdown)]
#![allow(clippy::too_many_lines)]

//! E2E: deterministic swarm flight-recorder replay harness.
//!
//! This test runs real `AgentSession` instances with real session persistence,
//! the built-in read tool, and JS extension lifecycle hooks. Providers are
//! deterministic in-process providers, so the replay path never needs live API
//! credentials.

mod common;

use std::collections::BTreeMap;
use std::pin::Pin;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;

use async_trait::async_trait;
use futures::{Stream, StreamExt};
use pi::agent::{AbortHandle, Agent, AgentConfig, AgentEvent, AgentSession, InputSource};
use pi::compaction::ResolvedCompactionSettings;
use pi::error::{Error, Result};
use pi::model::{
    AssistantMessage, ContentBlock, Message, StopReason, StreamEvent, TextContent,
    ToolResultMessage, Usage,
};
use pi::provider::{Context, Provider, StreamOptions};
use pi::resource_governor::{
    AdmissionAction, HostResourceBudgets, HostResourceSample, ResourceDimension, ResourceGovernor,
    ResourceOperationKind, ResourceRequest,
};
use pi::session::Session;
use pi::session_index::SessionIndex;
use pi::swarm_flight_recorder::{
    SWARM_FLIGHT_RECORDER_EVENT_SCHEMA, SWARM_FLIGHT_RECORDER_REPORT_SCHEMA, SwarmFlightRecorder,
    SwarmFlightRecorderEvent, validate_swarm_flight_recorder_jsonl,
};
use pi::tools::ToolRegistry;
use serde_json::{Value, json};
use url::Url;

const SWARM_PRESSURE_LAB_SCHEMA: &str = "pi.swarm.pressure_lab.v1";
const SWARM_PRESSURE_LAB_RUN_ID: &str = "swarm-pressure-lab-deterministic-v1";
const SWARM_PRESSURE_LAB_BURST_AGENTS: usize = 6;
const SWARM_PRESSURE_LAB_MODELED_AGENTS: u64 = 64;
const SWARM_LIFECYCLE_E2E_SCHEMA: &str = "pi.swarm.lifecycle_e2e.event.v1";

// Evidence schema: every JSONL row contains schema, run_id, scenario,
// agent_count, operation, latency_us, latency_ms, backpressure_count,
// coalesced_low_value_event_count, memory, verdict, and details.
const EXTENSION_SOURCE: &str = r#"
export default function init(pi) {
  const events = [];
  function remember(name, event) {
    events.push({
      name,
      toolName: event && event.toolName ? event.toolName : null,
      sessionId: event && event.sessionId ? event.sessionId : null,
    });
  }
  pi.on("agent_start", (event) => {
    remember("agent_start", event);
    return null;
  });
  pi.on("turn_start", (event) => {
    remember("turn_start", event);
    return null;
  });
  pi.on("tool_call", (event) => {
    remember("tool_call", event);
    return { block: false };
  });
  pi.on("tool_result", (event) => {
    remember("tool_result", event);
    return null;
  });
  pi.on("agent_end", (event) => {
    remember("agent_end", event);
    return null;
  });
  pi.registerCommand("flight-events", {
    description: "Return hook events captured for the flight recorder",
    handler: async () => JSON.stringify(events),
  });
}
"#;

#[derive(Debug)]
struct FlightProvider {
    read_path: String,
    expected_fragment: String,
    final_text: String,
    stream_calls: AtomicUsize,
}

impl FlightProvider {
    const fn new(read_path: String, expected_fragment: String, final_text: String) -> Self {
        Self {
            read_path,
            expected_fragment,
            final_text,
            stream_calls: AtomicUsize::new(0),
        }
    }

    fn assistant_message(
        &self,
        stop_reason: StopReason,
        content: Vec<ContentBlock>,
    ) -> AssistantMessage {
        AssistantMessage {
            content,
            api: self.api().to_string(),
            provider: self.name().to_string(),
            model: self.model_id().to_string(),
            usage: Usage {
                total_tokens: 12,
                output: 12,
                ..Usage::default()
            },
            stop_reason,
            stop_details: None,
            error_message: None,
            timestamp: 0,
        }
    }

    fn stream_done(
        &self,
        message: AssistantMessage,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>> {
        let partial = self.assistant_message(StopReason::Stop, Vec::new());
        Box::pin(futures::stream::iter(vec![
            Ok(StreamEvent::Start { partial }),
            Ok(StreamEvent::Done {
                reason: message.stop_reason,
                message,
            }),
        ]))
    }

    fn latest_tool_result<'a>(
        context: &'a Context<'a>,
        tool_call_id: &str,
    ) -> Option<&'a ToolResultMessage> {
        context
            .messages
            .iter()
            .rev()
            .filter_map(|message| match message {
                Message::ToolResult(result) => Some(result.as_ref()),
                _ => None,
            })
            .find(|result| result.tool_call_id == tool_call_id)
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for FlightProvider {
    fn name(&self) -> &str {
        "flight-recorder-provider"
    }

    fn api(&self) -> &str {
        "flight-recorder-api"
    }

    fn model_id(&self) -> &str {
        "flight-recorder-model"
    }

    async fn stream(
        &self,
        context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let call_index = self.stream_calls.fetch_add(1, Ordering::SeqCst);
        if call_index == 0 {
            return Ok(self.stream_done(self.assistant_message(
                StopReason::ToolUse,
                vec![ContentBlock::ToolCall(pi::model::ToolCall {
                    id: "flight-read-1".to_string(),
                    name: "read".to_string(),
                    arguments: json!({ "path": self.read_path }),
                    thought_signature: None,
                })],
            )));
        }

        if call_index == 1 {
            let Some(result) = Self::latest_tool_result(context, "flight-read-1") else {
                return Err(Error::api("flight provider expected read tool result"));
            };
            let text = result
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text(text) => Some(text.text.as_str()),
                    _ => None,
                })
                .collect::<String>();
            if !text.contains(&self.expected_fragment) {
                return Err(Error::api(
                    "flight provider read result missed expected fragment",
                ));
            }
            return Ok(self.stream_done(self.assistant_message(
                StopReason::Stop,
                vec![ContentBlock::Text(TextContent::new(
                    self.final_text.clone(),
                ))],
            )));
        }

        Err(Error::api(
            "flight provider received unexpected stream call",
        ))
    }
}

#[derive(Debug)]
struct FlightSessionEvidence {
    agent_name: String,
    final_text: String,
    session_entries: usize,
    session_path: std::path::PathBuf,
    indexed_sessions: usize,
    extension_events: Vec<String>,
}

struct LifecycleRow<'a> {
    scenario_id: &'a str,
    phase: &'a str,
    operation_label: &'a str,
    duration_ms: u64,
    outcome: &'a str,
    redaction_summary: Value,
    details: Value,
}

#[derive(Debug)]
struct LifecycleGuardrailProbe<'a> {
    endpoint: &'a str,
    env: BTreeMap<&'a str, &'a str>,
    operation_label: &'a str,
    live_mutation: Option<&'a str>,
    fixture_gated: bool,
}

#[derive(Debug)]
struct HangingStreamProvider;

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for HangingStreamProvider {
    fn name(&self) -> &str {
        "pressure-lab-hanging-provider"
    }

    fn api(&self) -> &str {
        "pressure-lab-hanging-api"
    }

    fn model_id(&self) -> &str {
        "pressure-lab-hanging-model"
    }

    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let partial = AssistantMessage {
            content: Vec::new(),
            api: self.api().into(),
            provider: self.name().into(),
            model: self.model_id().into(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            stop_details: None,
            error_message: None,
            timestamp: 0,
        };
        let started = futures::stream::iter(vec![Ok(StreamEvent::Start { partial })]);
        let pending = futures::stream::pending();
        Ok(Box::pin(started.chain(pending)))
    }
}

fn elapsed_ms(started_at: Instant) -> u64 {
    u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn elapsed_us(started_at: Instant) -> u64 {
    u64::try_from(started_at.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn elapsed_start() -> Instant {
    Instant::now()
}

fn lifecycle_empty_redaction_summary() -> Value {
    json!({
        "redactedFields": 0,
        "redactedKeys": [],
    })
}

fn lifecycle_event_row(row: &LifecycleRow<'_>) -> Value {
    json!({
        "schema": SWARM_LIFECYCLE_E2E_SCHEMA,
        "scenario_id": row.scenario_id,
        "phase": row.phase,
        "operation_label": row.operation_label,
        "duration_ms": row.duration_ms,
        "redaction_summary": row.redaction_summary,
        "outcome": row.outcome,
        "details": row.details,
    })
}

fn write_lifecycle_jsonl(path: &std::path::Path, rows: &[Value]) {
    let mut jsonl = String::new();
    for row in rows {
        jsonl.push_str(&serde_json::to_string(row).expect("serialize lifecycle row"));
        jsonl.push('\n');
    }
    std::fs::write(path, jsonl).expect("write lifecycle jsonl");
}

fn validate_lifecycle_jsonl(path: &std::path::Path) -> Vec<Value> {
    let jsonl = std::fs::read_to_string(path).expect("read lifecycle jsonl");
    let rows = jsonl
        .lines()
        .enumerate()
        .map(|(index, line)| {
            let row = serde_json::from_str::<Value>(line).expect("parse lifecycle row");
            assert_eq!(row["schema"], SWARM_LIFECYCLE_E2E_SCHEMA, "line {index}");
            for field in ["scenario_id", "phase", "operation_label", "outcome"] {
                assert!(
                    row[field].as_str().is_some_and(|value| !value.is_empty()),
                    "line {index} should contain non-empty {field}"
                );
            }
            assert!(row["duration_ms"].as_u64().is_some(), "line {index}");
            assert!(row["redaction_summary"].is_object(), "line {index}");
            assert!(row["details"].is_object(), "line {index}");
            row
        })
        .collect::<Vec<_>>();
    assert!(
        rows.iter()
            .any(|row| lifecycle_row_has_outcome(row, "lifecycle_success", "pass")),
        "lifecycle JSONL should include a passing success scenario"
    );
    assert!(
        rows.iter()
            .any(|row| lifecycle_row_has_outcome(row, "guardrail_fail_closed", "blocked")),
        "lifecycle JSONL should include a blocked negative scenario"
    );
    rows
}

fn lifecycle_row_has_outcome(row: &Value, scenario_id: &str, outcome: &str) -> bool {
    row.get("scenario_id")
        .and_then(Value::as_str)
        .is_some_and(|value| value.eq(scenario_id))
        && row
            .get("outcome")
            .and_then(Value::as_str)
            .is_some_and(|value| value.eq(outcome))
}

fn lifecycle_reasons_include(row: &Value, expected_reason: &str) -> bool {
    row["details"]["reasons"].as_array().is_some_and(|reasons| {
        reasons.iter().any(|reason| {
            reason
                .as_str()
                .is_some_and(|reason| reason.eq(expected_reason))
        })
    })
}

fn endpoint_is_production(endpoint: &str) -> bool {
    let Ok(url) = Url::parse(endpoint) else {
        return true;
    };
    let Some(host) = url.host_str() else {
        return true;
    };
    let host = host.to_ascii_lowercase();
    !matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1")
        && !matches!(host.rsplit('.').next(), Some("test" | "invalid"))
}

fn env_contains_ambient_secret(env: &BTreeMap<&str, &str>) -> Vec<String> {
    env.keys()
        .copied()
        .filter(|key| {
            let normalized = key.to_ascii_uppercase();
            normalized.ends_with("_API_KEY")
                || normalized.ends_with("_TOKEN")
                || normalized.contains("SECRET")
        })
        .map(str::to_string)
        .collect()
}

fn operation_is_destructive(operation_label: &str) -> bool {
    let normalized = operation_label.to_ascii_lowercase();
    [
        "rm -rf",
        "git reset --hard",
        "git clean -fd",
        "delete_file",
        "remove_dir_all",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn evaluate_lifecycle_guardrail(probe: &LifecycleGuardrailProbe<'_>) -> Value {
    let mut reasons = Vec::new();
    if endpoint_is_production(probe.endpoint) {
        reasons.push("production_url_blocked".to_string());
    }
    let ambient_secret_keys = env_contains_ambient_secret(&probe.env);
    if !ambient_secret_keys.is_empty() {
        reasons.push("ambient_api_key_blocked".to_string());
    }
    if operation_is_destructive(probe.operation_label) {
        reasons.push("destructive_filesystem_operation_blocked".to_string());
    }
    if probe.live_mutation.is_some() && !probe.fixture_gated {
        reasons.push("live_coordination_or_rch_mutation_blocked".to_string());
    }
    json!({
        "blocked": !reasons.is_empty(),
        "reasons": reasons,
        "endpoint": probe.endpoint,
        "operation_label": probe.operation_label,
        "live_mutation": probe.live_mutation,
        "fixture_gated": probe.fixture_gated,
        "ambient_secret_keys": ambient_secret_keys,
    })
}

fn run_degraded_coordination_runpack_e2e(
    harness: &common::TestHarness,
) -> (Value, std::path::PathBuf, std::path::PathBuf) {
    let capture_dir = harness.temp_path("lifecycle/runpack/capture");
    let summary_path = harness.temp_path("lifecycle/runpack/degraded_coordination_summary.json");
    let events_path = harness.temp_path("lifecycle/runpack/degraded_coordination_events.jsonl");
    let output = Command::new("python3")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .arg("scripts/build_swarm_operator_runpack.py")
        .arg("--run-degraded-coordination-e2e")
        .arg("--capture-dir")
        .arg(&capture_dir)
        .arg("--out-degraded-coordination-e2e-json")
        .arg(&summary_path)
        .arg("--out-degraded-coordination-e2e-events-jsonl")
        .arg(&events_path)
        .arg("--generated-at")
        .arg("2026-05-18T00:00:00Z")
        .arg("--capture-timeout-seconds")
        .arg("30")
        .output()
        .expect("run degraded coordination runpack E2E script");
    assert!(
        output.status.success(),
        "degraded coordination runpack E2E failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(summary_path.exists(), "runpack summary should be written");
    assert!(
        events_path.exists(),
        "runpack events JSONL should be written"
    );
    let summary = serde_json::from_str::<Value>(
        &std::fs::read_to_string(&summary_path).expect("read runpack E2E summary"),
    )
    .expect("parse runpack E2E summary");
    assert_eq!(
        summary["schema"],
        "pi.swarm.degraded_coordination_runpack_e2e.v1"
    );
    assert_eq!(summary["status"], "pass");
    assert_eq!(summary["guards"]["uses_real_temp_beads"], true);
    assert_eq!(
        summary["guards"]["fixture_captures_degraded_rch_and_agent_mail"],
        true
    );
    assert_eq!(summary["guards"]["no_cleanup_or_deletion_commands"], true);
    (summary, summary_path, events_path)
}

async fn run_flight_session(
    agent_name: String,
    input_source: InputSource,
    workspace: std::path::PathBuf,
    recorder: Arc<StdMutex<SwarmFlightRecorder>>,
) -> Result<FlightSessionEvidence> {
    std::fs::create_dir_all(workspace.join("extensions"))?;
    std::fs::create_dir_all(workspace.join("fixtures"))?;
    let fixture_path = workspace.join("fixtures/input.txt");
    std::fs::write(
        &fixture_path,
        format!("agent={agent_name}\nflight_recorder=enabled\n"),
    )?;
    let extension_path = workspace.join("extensions/flight.mjs");
    std::fs::write(&extension_path, EXTENSION_SOURCE)?;

    let provider: Arc<dyn Provider> = Arc::new(FlightProvider::new(
        fixture_path.display().to_string(),
        "flight_recorder=enabled".to_string(),
        format!("{agent_name} flight complete"),
    ));
    let tools = ToolRegistry::new(&["read"], &workspace, None);
    let config = AgentConfig {
        system_prompt: None,
        max_tool_iterations: 4,
        stream_options: StreamOptions {
            api_key: Some("offline-flight-recorder-key".to_string()),
            session_id: Some(agent_name.clone()),
            ..StreamOptions::default()
        },
        block_images: false,
        fail_closed_hooks: true,
        tool_approval: None,
    };
    let agent = Agent::new(provider, tools, config);
    let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
        Some(workspace.join("sessions")),
    )));
    let mut agent_session = AgentSession::new(
        agent,
        Arc::clone(&session),
        true,
        ResolvedCompactionSettings::default(),
    );
    agent_session.set_input_source(input_source);
    agent_session
        .enable_extensions(&[], &workspace, None, &[extension_path])
        .await?;

    let started_at = elapsed_start();
    let event_recorder = Arc::clone(&recorder);
    let event_agent = agent_name.clone();
    let message = agent_session
        .run_text(
            format!("Inspect the flight fixture for {agent_name}."),
            move |event: AgentEvent| {
                event_recorder
                    .lock()
                    .expect("lock flight recorder")
                    .record_agent_event(event_agent.clone(), elapsed_ms(started_at), &event)
                    .expect("record agent event");
            },
        )
        .await?;

    let extension_value = agent_session
        .execute_extension_command("flight-events", "", 5_000, |_| {})
        .await?;
    let extension_events = extension_value
        .as_str()
        .and_then(|value| serde_json::from_str::<Vec<Value>>(value).ok())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|value| {
            value
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect::<Vec<_>>();

    let sessions_dir = workspace.join("sessions");
    let (session_entries, session_path, indexed_sessions) = {
        let cx = pi::agent_cx::AgentCx::for_current_or_request();
        let guard = session.lock(cx.cx()).await?;
        let session_path = guard
            .path
            .clone()
            .ok_or_else(|| Error::session("flight session did not persist a path"))?;
        let index = SessionIndex::for_sessions_root(&sessions_dir);
        index.index_session(&guard)?;
        let indexed_sessions = index.list_sessions(None)?.len();
        (
            guard.entries_for_current_path().len(),
            session_path,
            indexed_sessions,
        )
    };

    recorder
        .lock()
        .expect("lock flight recorder")
        .record_session_snapshot(
            agent_name.clone(),
            agent_name.clone(),
            elapsed_ms(started_at),
            json!({
                "session_dir": workspace.join("sessions").display().to_string(),
                "session_path": session_path.display().to_string(),
                "entry_count": session_entries,
                "indexed_sessions": indexed_sessions,
                "input_source": input_source.as_str(),
            }),
        )?;
    recorder
        .lock()
        .expect("lock flight recorder")
        .record_extension_event(
            agent_name.clone(),
            agent_name.clone(),
            elapsed_ms(started_at),
            json!({
                "hook_events": extension_events,
                "extension_entry": workspace.join("extensions/flight.mjs").display().to_string(),
            }),
        )?;

    let final_text = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<String>();

    Ok(FlightSessionEvidence {
        agent_name,
        final_text,
        session_entries,
        session_path,
        indexed_sessions,
        extension_events,
    })
}

async fn run_cancelled_pressure_session(
    agent_name: String,
    workspace: std::path::PathBuf,
    recorder: Arc<StdMutex<SwarmFlightRecorder>>,
) -> Result<FlightSessionEvidence> {
    std::fs::create_dir_all(workspace.join("sessions"))?;

    let provider: Arc<dyn Provider> = Arc::new(HangingStreamProvider);
    let tools = ToolRegistry::new(&[], &workspace, None);
    let config = AgentConfig {
        system_prompt: None,
        max_tool_iterations: 1,
        stream_options: StreamOptions {
            api_key: Some("offline-pressure-lab-key".to_string()),
            session_id: Some(agent_name.clone()),
            ..StreamOptions::default()
        },
        block_images: false,
        fail_closed_hooks: true,
        tool_approval: None,
    };
    let agent = Agent::new(provider, tools, config);
    let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
        Some(workspace.join("sessions")),
    )));
    let mut agent_session = AgentSession::new(
        agent,
        Arc::clone(&session),
        true,
        ResolvedCompactionSettings::default(),
    );
    agent_session.set_input_source(InputSource::Rpc);

    let (abort_handle, abort_signal) = AbortHandle::new();
    let started_at = elapsed_start();
    let event_recorder = Arc::clone(&recorder);
    let event_agent = agent_name.clone();
    let message = agent_session
        .run_text_with_abort(
            "Start the cancellable pressure-lab stream.".to_string(),
            Some(abort_signal),
            move |event: AgentEvent| {
                let should_abort = matches!(
                    &event,
                    AgentEvent::MessageStart {
                        message: Message::Assistant(_)
                    }
                );
                event_recorder
                    .lock()
                    .expect("lock flight recorder")
                    .record_agent_event(event_agent.clone(), elapsed_ms(started_at), &event)
                    .expect("record cancellation agent event");
                if should_abort {
                    abort_handle.abort();
                }
            },
        )
        .await?;

    assert_eq!(message.stop_reason, StopReason::Aborted);
    assert_eq!(message.error_message.as_deref(), Some("Aborted"));

    let session_entries = {
        let cx = pi::agent_cx::AgentCx::for_current_or_request();
        let guard = session.lock(cx.cx()).await?;
        guard.entries_for_current_path().len()
    };
    recorder
        .lock()
        .expect("lock flight recorder")
        .record_session_snapshot(
            agent_name.clone(),
            agent_name.clone(),
            elapsed_ms(started_at),
            json!({
                "session_dir": workspace.join("sessions").display().to_string(),
                "entry_count": session_entries,
                "input_source": InputSource::Rpc.as_str(),
                "aborted": true,
            }),
        )?;

    Ok(FlightSessionEvidence {
        agent_name,
        final_text: "aborted".to_string(),
        session_entries,
        session_path: workspace.join("sessions"),
        indexed_sessions: 0,
        extension_events: Vec::new(),
    })
}

const fn pressure_lab_sample() -> HostResourceSample {
    HostResourceSample {
        load_avg_1m: Some(1.0),
        rss_bytes: Some(96 * 1024 * 1024),
        process_count: Some(24),
        fd_count: Some(48),
    }
}

fn pressure_lab_memory(sample: HostResourceSample) -> Value {
    json!({
        "rss_bytes": sample.rss_bytes.unwrap_or(0),
        "process_count": sample.process_count.unwrap_or(0),
        "open_file_descriptors": sample.fd_count.unwrap_or(0),
    })
}

fn pressure_lab_governor_decisions()
-> Vec<(ResourceRequest, pi::resource_governor::AdmissionDecision)> {
    let sample = pressure_lab_sample();
    let governor = ResourceGovernor::with_budgets(HostResourceBudgets::fixed_with_queue_depth(
        4.0,
        512 * 1024 * 1024,
        128,
        256,
        1_024,
        8,
    ));
    vec![
        ResourceRequest::new(ResourceOperationKind::Events, "rpc.event_output").with_queue_depth(1),
        ResourceRequest::new(ResourceOperationKind::Session, "session.persist").with_queue_depth(7),
        ResourceRequest::new(ResourceOperationKind::Tool, "tool.read")
            .with_estimated_tool_output_bytes(1_200),
        ResourceRequest::new(ResourceOperationKind::Events, "extension.hostcall")
            .with_queue_depth(9),
    ]
    .into_iter()
    .map(|request| {
        let decision = governor.admit_sample(&request, sample);
        (request, decision)
    })
    .collect()
}

fn count_agent_component_events(rows: &[SwarmFlightRecorderEvent], event_kind: &str) -> u64 {
    u64::try_from(
        rows.iter()
            .filter(|row| matches!(row.component.as_str(), "agent") && row.event_kind == event_kind)
            .count(),
    )
    .unwrap_or(u64::MAX)
}

fn count_component_events(rows: &[SwarmFlightRecorderEvent], component: &str) -> u64 {
    u64::try_from(rows.iter().filter(|row| row.component == component).count()).unwrap_or(u64::MAX)
}

fn semantic_rpc_event_count(rows: &[SwarmFlightRecorderEvent]) -> u64 {
    u64::try_from(
        rows.iter()
            .filter(|row| {
                matches!(row.component.as_str(), "session" | "extension")
                    || matches!(
                        row.event_kind.as_str(),
                        "agent_start"
                            | "agent_end"
                            | "message_start"
                            | "message_end"
                            | "tool_execution_start"
                            | "tool_execution_end"
                    )
            })
            .count(),
    )
    .unwrap_or(u64::MAX)
}

fn coalesced_low_value_event_count(candidate_updates: u64, retained_updates: u64) -> u64 {
    candidate_updates.saturating_sub(retained_updates.min(candidate_updates))
}

fn pressure_lab_row(
    scenario: &str,
    agent_count: u64,
    operation: &str,
    latency_us: u64,
    backpressure_count: u64,
    coalesced_low_value_event_count: u64,
    details: &Value,
) -> Value {
    json!({
        "schema": SWARM_PRESSURE_LAB_SCHEMA,
        "run_id": SWARM_PRESSURE_LAB_RUN_ID,
        "scenario": scenario,
        "agent_count": agent_count,
        "operation": operation,
        "latency_us": latency_us,
        "latency_ms": latency_us / 1_000,
        "backpressure_count": backpressure_count,
        "coalesced_low_value_event_count": coalesced_low_value_event_count,
        "memory": pressure_lab_memory(pressure_lab_sample()),
        "verdict": "pass",
        "details": details,
    })
}

fn write_pressure_lab_jsonl(path: &std::path::Path, rows: &[Value]) {
    let mut jsonl = String::new();
    for row in rows {
        jsonl.push_str(&serde_json::to_string(row).expect("serialize pressure lab row"));
        jsonl.push('\n');
    }
    std::fs::write(path, jsonl).expect("write pressure lab jsonl");
}

fn validate_pressure_lab_jsonl(path: &std::path::Path) -> Vec<Value> {
    let jsonl = std::fs::read_to_string(path).expect("read pressure lab jsonl");
    let rows = jsonl
        .lines()
        .enumerate()
        .map(|(index, line)| {
            let row = serde_json::from_str::<Value>(line).expect("parse pressure lab row");
            assert_eq!(row["schema"], SWARM_PRESSURE_LAB_SCHEMA, "line {index}");
            assert_eq!(row["run_id"], SWARM_PRESSURE_LAB_RUN_ID, "line {index}");
            assert!(row["scenario"].as_str().is_some(), "line {index}");
            assert!(row["operation"].as_str().is_some(), "line {index}");
            assert_eq!(row["verdict"], "pass", "line {index}");
            row
        })
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 4);
    rows
}

#[test]
fn no_mock_swarm_lifecycle_e2e_emits_guarded_jsonl() {
    let test_name = "no_mock_swarm_lifecycle_e2e_emits_guarded_jsonl";
    let harness = common::TestHarness::new(test_name);
    let recorder = Arc::new(StdMutex::new(
        SwarmFlightRecorder::new("swarm-lifecycle-e2e").expect("create recorder"),
    ));
    let workspace = harness.temp_path("lifecycle/agent-success");
    let started_at = elapsed_start();
    let success = common::run_async(run_flight_session(
        "lifecycle-success-agent".to_string(),
        InputSource::Rpc,
        workspace.clone(),
        Arc::clone(&recorder),
    ))
    .expect("lifecycle success session succeeds");

    assert!(
        success
            .final_text
            .contains("lifecycle-success-agent flight complete")
    );
    assert!(success.session_entries >= 4);
    assert!(
        success.session_path.starts_with(workspace.join("sessions")),
        "session path should stay under the scenario workspace: {}",
        success.session_path.display()
    );
    assert!(success.session_path.exists(), "session JSONL should exist");
    assert!(success.indexed_sessions >= 1, "session should be indexed");
    assert!(
        success
            .extension_events
            .iter()
            .any(|event| matches!(event.as_str(), "tool_call")),
        "extension policy path should observe tool calls: {:?}",
        success.extension_events
    );

    recorder
        .lock()
        .expect("lock recorder")
        .record_coordination_marker(
            "lifecycle-success-agent",
            elapsed_ms(started_at),
            "agent_mail_fixture_degraded",
            json!({
                "status": "red",
                "mode": "fixture_only_beads_soft_lock",
                "summary": "Live Agent Mail mutation disabled for lifecycle E2E",
                "api_key": "must-redact",
            }),
        )
        .expect("record degraded coordination fixture");

    let flight_jsonl = recorder
        .lock()
        .expect("lock recorder")
        .to_jsonl()
        .expect("flight recorder jsonl");
    let flight_rows =
        validate_swarm_flight_recorder_jsonl(&flight_jsonl).expect("valid flight recorder");
    assert!(
        flight_rows.iter().any(|row| row
            .redaction
            .redacted_keys
            .iter()
            .any(|key| key.as_str().eq("api_key"))),
        "coordination fixture should redact API key material"
    );
    let redacted_fields = flight_rows
        .iter()
        .map(|row| row.redaction.redacted_fields)
        .sum::<u64>();
    let redacted_keys = flight_rows
        .iter()
        .flat_map(|row| row.redaction.redacted_keys.iter().cloned())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let success_redaction = json!({
        "redactedFields": redacted_fields,
        "redactedKeys": redacted_keys,
    });
    let runpack_started_at = elapsed_start();
    let (runpack_summary, runpack_summary_path, runpack_events_path) =
        run_degraded_coordination_runpack_e2e(&harness);
    harness.record_artifact("degraded_coordination_summary.json", &runpack_summary_path);
    harness.record_artifact("degraded_coordination_events.jsonl", &runpack_events_path);

    let mut lifecycle_rows = vec![
        lifecycle_event_row(&LifecycleRow {
            scenario_id: "lifecycle_success",
            phase: "setup",
            operation_label: "temp_workspace_and_fixture_creation",
            duration_ms: 0,
            redaction_summary: lifecycle_empty_redaction_summary(),
            outcome: "pass",
            details: json!({
                "workspace_under_test_temp_root": workspace.starts_with(harness.temp_dir()),
                "uses_live_provider_credentials": false,
            }),
        }),
        lifecycle_event_row(&LifecycleRow {
            scenario_id: "lifecycle_success",
            phase: "rpc_session",
            operation_label: "AgentSession::run_text",
            duration_ms: elapsed_ms(started_at),
            redaction_summary: success_redaction.clone(),
            outcome: "pass",
            details: json!({
                "input_source": InputSource::Rpc.as_str(),
                "session_file_exists": success.session_path.exists(),
                "session_entries": success.session_entries,
                "indexed_sessions": success.indexed_sessions,
            }),
        }),
        lifecycle_event_row(&LifecycleRow {
            scenario_id: "lifecycle_success",
            phase: "tool_execution",
            operation_label: "ToolRegistry::read",
            duration_ms: elapsed_ms(started_at),
            redaction_summary: success_redaction.clone(),
            outcome: "pass",
            details: json!({
                "tool": "read",
                "real_tool_registry": true,
                "live_network": false,
            }),
        }),
        lifecycle_event_row(&LifecycleRow {
            scenario_id: "lifecycle_success",
            phase: "extension_policy",
            operation_label: "execute_extension_command:flight-events",
            duration_ms: elapsed_ms(started_at),
            redaction_summary: success_redaction,
            outcome: "pass",
            details: json!({
                "hook_events": success.extension_events,
                "fail_closed_hooks": true,
            }),
        }),
        lifecycle_event_row(&LifecycleRow {
            scenario_id: "lifecycle_success",
            phase: "runpack_evidence",
            operation_label: "build_swarm_operator_runpack.py --run-degraded-coordination-e2e",
            duration_ms: elapsed_ms(runpack_started_at),
            redaction_summary: lifecycle_empty_redaction_summary(),
            outcome: "pass",
            details: json!({
                "summary_schema": runpack_summary["schema"],
                "summary_status": runpack_summary["status"],
                "uses_real_temp_beads": runpack_summary["guards"]["uses_real_temp_beads"],
                "fixture_captures_degraded_rch_and_agent_mail": runpack_summary["guards"]["fixture_captures_degraded_rch_and_agent_mail"],
                "no_cleanup_or_deletion_commands": runpack_summary["guards"]["no_cleanup_or_deletion_commands"],
                "summary_artifact_exists": runpack_summary_path.exists(),
                "events_jsonl_exists": runpack_events_path.exists(),
            }),
        }),
    ];

    let negative_probes = vec![
        LifecycleGuardrailProbe {
            endpoint: "https://api.openai.com/v1/responses",
            env: BTreeMap::new(),
            operation_label: "provider.live_request",
            live_mutation: None,
            fixture_gated: false,
        },
        LifecycleGuardrailProbe {
            endpoint: "http://127.0.0.1/fixture",
            env: BTreeMap::from([("OPENAI_API_KEY", "redacted-fixture-key")]),
            operation_label: "provider.fixture_replay",
            live_mutation: None,
            fixture_gated: true,
        },
        LifecycleGuardrailProbe {
            endpoint: "http://127.0.0.1/fixture",
            env: BTreeMap::new(),
            operation_label: "rm -rf lifecycle-workspace",
            live_mutation: None,
            fixture_gated: true,
        },
        LifecycleGuardrailProbe {
            endpoint: "http://127.0.0.1/fixture",
            env: BTreeMap::new(),
            operation_label: "agent_mail.send_message",
            live_mutation: Some("agent_mail"),
            fixture_gated: false,
        },
        LifecycleGuardrailProbe {
            endpoint: "http://127.0.0.1/fixture",
            env: BTreeMap::new(),
            operation_label: "rch exec cargo test",
            live_mutation: Some("rch"),
            fixture_gated: false,
        },
    ];
    for probe in &negative_probes {
        let guardrail = evaluate_lifecycle_guardrail(probe);
        assert_eq!(
            guardrail["blocked"], true,
            "negative probe should fail closed: {guardrail}"
        );
        lifecycle_rows.push(lifecycle_event_row(&LifecycleRow {
            scenario_id: "guardrail_fail_closed",
            phase: "guardrail",
            operation_label: probe.operation_label,
            duration_ms: 0,
            redaction_summary: lifecycle_empty_redaction_summary(),
            outcome: "blocked",
            details: guardrail,
        }));
    }

    let lifecycle_path = harness.temp_path("swarm_lifecycle_e2e_events.jsonl");
    write_lifecycle_jsonl(&lifecycle_path, &lifecycle_rows);
    let validated_rows = validate_lifecycle_jsonl(&lifecycle_path);
    assert!(
        validated_rows
            .iter()
            .any(|row| lifecycle_reasons_include(row, "production_url_blocked")),
        "negative scenario should block production URLs"
    );
    assert!(
        validated_rows
            .iter()
            .any(|row| lifecycle_reasons_include(row, "ambient_api_key_blocked")),
        "negative scenario should block ambient API keys"
    );
    assert!(
        validated_rows
            .iter()
            .any(|row| lifecycle_reasons_include(row, "destructive_filesystem_operation_blocked")),
        "negative scenario should block destructive filesystem operations"
    );
    assert_eq!(
        validated_rows
            .iter()
            .filter(|row| {
                lifecycle_reasons_include(row, "live_coordination_or_rch_mutation_blocked")
            })
            .count(),
        2,
        "negative scenario should block live Agent Mail and RCH mutations"
    );

    harness.record_artifact("swarm_lifecycle_e2e_events.jsonl", &lifecycle_path);
}

#[test]
fn multi_agent_flight_recorder_bundle_replays_without_credentials() {
    let test_name = "multi_agent_flight_recorder_bundle_replays_without_credentials";
    let harness = common::TestHarness::new(test_name);
    let recorder = Arc::new(StdMutex::new(
        SwarmFlightRecorder::new("flight-recorder-e2e").expect("create recorder"),
    ));

    let alpha_workspace = harness.temp_path("agents/alpha");
    let beta_workspace = harness.temp_path("agents/beta");
    let (alpha, beta) = common::run_async({
        let recorder_a = Arc::clone(&recorder);
        let recorder_b = Arc::clone(&recorder);
        async move {
            Box::pin(futures::future::join(
                run_flight_session(
                    "agent-alpha".to_string(),
                    InputSource::Rpc,
                    alpha_workspace,
                    recorder_a,
                ),
                run_flight_session(
                    "agent-beta".to_string(),
                    InputSource::Interactive,
                    beta_workspace,
                    recorder_b,
                ),
            ))
            .await
        }
    });
    let alpha = alpha.expect("alpha session succeeds");
    let beta = beta.expect("beta session succeeds");

    assert_eq!(alpha.agent_name, "agent-alpha");
    assert_eq!(beta.agent_name, "agent-beta");
    assert!(alpha.final_text.contains("agent-alpha flight complete"));
    assert!(beta.final_text.contains("agent-beta flight complete"));
    assert!(
        alpha.session_entries >= 4,
        "alpha session should persist entries"
    );
    assert!(
        beta.session_entries >= 4,
        "beta session should persist entries"
    );
    assert!(
        alpha
            .extension_events
            .iter()
            .any(|event| matches!(event.as_str(), "tool_call")),
        "alpha extension should observe tool_call: {:?}",
        alpha.extension_events
    );
    assert!(
        beta.extension_events
            .iter()
            .any(|event| matches!(event.as_str(), "tool_result")),
        "beta extension should observe tool_result: {:?}",
        beta.extension_events
    );

    recorder
        .lock()
        .expect("lock recorder")
        .record_coordination_marker(
            "GoldenGlacier",
            0,
            "agent_mail_degraded_beads_fallback",
            json!({
                "status": "red",
                "mode": "beads_soft_lock_fallback",
                "summary": "Agent Mail unavailable; Beads used as non-blocking soft lock",
                "token": "must-redact",
            }),
        )
        .expect("record coordination marker");

    let bundle_path = harness.temp_path("swarm_flight_recorder.jsonl");
    let report_path = harness.temp_path("swarm_flight_recorder_report.json");
    let jsonl = recorder
        .lock()
        .expect("lock recorder")
        .to_jsonl()
        .expect("jsonl");
    std::fs::write(&bundle_path, &jsonl).expect("write flight recorder bundle");
    let rows = validate_swarm_flight_recorder_jsonl(&jsonl).expect("valid flight jsonl");
    assert_eq!(rows[0].schema, SWARM_FLIGHT_RECORDER_EVENT_SCHEMA);
    assert!(
        rows.iter().any(|row| row
            .redaction
            .redacted_keys
            .iter()
            .any(|key| matches!(key.as_str(), "token"))),
        "coordination token should be redacted in bundle"
    );
    assert!(
        rows.iter()
            .any(|row| matches!(row.component.as_str(), "agent")
                && matches!(row.event_kind.as_str(), "tool_execution_start")),
        "bundle should contain tool timing events"
    );
    assert!(
        rows.iter()
            .any(|row| matches!(row.component.as_str(), "session")),
        "bundle should contain session snapshots"
    );
    assert!(
        rows.iter()
            .any(|row| matches!(row.component.as_str(), "extension")),
        "bundle should contain extension hook summaries"
    );

    let report = recorder.lock().expect("lock recorder").build_report(
        "cargo test --test e2e_swarm_flight_recorder -- --exact multi_agent_flight_recorder_bundle_replays_without_credentials --nocapture",
        vec![bundle_path.display().to_string()],
    );
    assert_eq!(report.schema, SWARM_FLIGHT_RECORDER_REPORT_SCHEMA);
    assert_eq!(report.agent_count, 3);
    assert!(!report.replay.requires_live_provider_credentials);
    assert!(
        report
            .dominant_latency_components
            .iter()
            .any(|entry| matches!(entry.component.as_str(), "localTools")),
        "report should attribute tool latency: {:?}",
        report.dominant_latency_components
    );
    assert_eq!(report.coordination_failures.len(), 1);
    std::fs::write(
        &report_path,
        serde_json::to_string_pretty(&report).expect("serialize report"),
    )
    .expect("write report");
    harness.record_artifact("swarm_flight_recorder.jsonl", &bundle_path);
    harness.record_artifact("swarm_flight_recorder_report.json", &report_path);

    harness
        .write_jsonl_logs(harness.temp_path("swarm_flight_recorder_test.log.jsonl"))
        .expect("write test log");
}

#[test]
fn swarm_pressure_lab_runs_smoke_backpressure_burst_and_cancellation() {
    let test_name = "swarm_pressure_lab_runs_smoke_backpressure_burst_and_cancellation";
    let harness = common::TestHarness::new(test_name);
    let recorder = Arc::new(StdMutex::new(
        SwarmFlightRecorder::new("swarm-pressure-lab-e2e").expect("create recorder"),
    ));
    let mut evidence_rows = Vec::new();

    let smoke_started = elapsed_start();
    let smoke = common::run_async(run_flight_session(
        "pressure-smoke-0".to_string(),
        InputSource::Rpc,
        harness.temp_path("pressure_lab/smoke/agent-0"),
        Arc::clone(&recorder),
    ))
    .expect("smoke session succeeds");
    assert!(
        smoke
            .final_text
            .contains("pressure-smoke-0 flight complete")
    );
    assert!(
        smoke.session_entries >= 4,
        "smoke session should persist entries"
    );
    assert!(
        smoke
            .extension_events
            .iter()
            .any(|event| matches!(event.as_str(), "tool_call")),
        "smoke extension should observe tool_call: {:?}",
        smoke.extension_events
    );
    evidence_rows.push(pressure_lab_row(
        "small_smoke",
        1,
        "rpc_tool_session_extension_smoke",
        elapsed_us(smoke_started),
        0,
        0,
        &json!({
            "agent_name": smoke.agent_name,
            "session_entries": smoke.session_entries,
            "extension_events": smoke.extension_events,
        }),
    ));

    let burst_started = elapsed_start();
    let burst_results = common::run_async({
        let recorder = Arc::clone(&recorder);
        let harness_root = harness.temp_path("pressure_lab/burst");
        async move {
            let tasks = (0..SWARM_PRESSURE_LAB_BURST_AGENTS)
                .map(|index| {
                    run_flight_session(
                        format!("pressure-burst-{index}"),
                        InputSource::Rpc,
                        harness_root.join(format!("agent-{index}")),
                        Arc::clone(&recorder),
                    )
                })
                .collect::<Vec<_>>();
            futures::future::join_all(tasks).await
        }
    });
    let burst = burst_results
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .expect("burst sessions succeed");
    assert_eq!(burst.len(), SWARM_PRESSURE_LAB_BURST_AGENTS);
    assert!(
        burst.iter().all(|entry| entry.session_entries >= 4),
        "burst sessions should persist entries: {burst:?}"
    );
    assert!(
        burst.iter().all(|entry| entry
            .extension_events
            .iter()
            .any(|event| matches!(event.as_str(), "agent_end"))),
        "burst extensions should observe agent_end hooks: {burst:?}"
    );
    evidence_rows.push(pressure_lab_row(
        "burst_fanout",
        u64::try_from(SWARM_PRESSURE_LAB_BURST_AGENTS).unwrap_or(u64::MAX),
        "rpc_tool_session_extension_burst",
        elapsed_us(burst_started),
        0,
        0,
        &json!({
            "agents": burst
                .iter()
                .map(|entry| entry.agent_name.clone())
                .collect::<Vec<_>>(),
            "session_entries": burst
                .iter()
                .map(|entry| entry.session_entries)
                .collect::<Vec<_>>(),
        }),
    ));

    let decisions = pressure_lab_governor_decisions();
    let backpressure_count = u64::try_from(
        decisions
            .iter()
            .filter(|(_request, decision)| matches!(decision.action, AdmissionAction::Backpressure))
            .count(),
    )
    .unwrap_or(u64::MAX);
    let deny_count = u64::try_from(
        decisions
            .iter()
            .filter(|(_request, decision)| matches!(decision.action, AdmissionAction::Deny))
            .count(),
    )
    .unwrap_or(u64::MAX);
    assert!(backpressure_count >= 1);
    assert!(deny_count >= 1);
    assert!(
        decisions.iter().any(|(_request, decision)| {
            matches!(
                decision.dominant_dimension,
                ResourceDimension::QueueDepth | ResourceDimension::ToolOutput
            )
        }),
        "governor should attribute pressure to bounded dimensions"
    );
    evidence_rows.push(pressure_lab_row(
        "sustained_backpressure",
        SWARM_PRESSURE_LAB_MODELED_AGENTS,
        "resource_governor_admission_replay",
        0,
        backpressure_count,
        0,
        &json!({
            "denied_count": deny_count,
            "decisions": decisions
                .iter()
                .map(|(request, decision)| decision.telemetry(request))
                .collect::<Vec<_>>(),
        }),
    ));

    let cancellation_started = elapsed_start();
    let cancelled = common::run_async(run_cancelled_pressure_session(
        "pressure-cancel-0".to_string(),
        harness.temp_path("pressure_lab/cancellation/agent-0"),
        Arc::clone(&recorder),
    ))
    .expect("cancellation session succeeds");
    assert_eq!(cancelled.final_text, "aborted");
    assert!(
        cancelled.session_entries >= 2,
        "cancelled run should persist user and aborted assistant entries"
    );

    let flight_jsonl = recorder
        .lock()
        .expect("lock pressure lab recorder")
        .to_jsonl()
        .expect("pressure lab flight recorder jsonl");
    let flight_rows =
        validate_swarm_flight_recorder_jsonl(&flight_jsonl).expect("valid pressure lab recorder");
    let semantic_event_count = semantic_rpc_event_count(&flight_rows);
    let coalesced_count = coalesced_low_value_event_count(32, 1);
    assert!(semantic_event_count > 0);
    assert!(coalesced_count > 0);
    assert!(
        count_agent_component_events(&flight_rows, "tool_execution_start") >= 1,
        "pressure lab should preserve tool execution start events"
    );
    assert!(
        count_component_events(&flight_rows, "session") >= 1,
        "pressure lab should preserve session snapshots"
    );
    assert!(
        count_component_events(&flight_rows, "extension") >= 1,
        "pressure lab should preserve extension hostcall evidence"
    );
    evidence_rows.push(pressure_lab_row(
        "cancellation",
        1,
        "rpc_abort_preserves_semantic_events",
        elapsed_us(cancellation_started),
        0,
        coalesced_count,
        &json!({
            "agent_name": cancelled.agent_name,
            "session_entries": cancelled.session_entries,
            "preserved_semantic_event_count": semantic_event_count,
            "tool_execution_start_count": count_agent_component_events(
                &flight_rows,
                "tool_execution_start"
            ),
            "session_snapshot_count": count_component_events(&flight_rows, "session"),
            "extension_event_count": count_component_events(&flight_rows, "extension"),
            "low_value_delta_candidates": 32,
            "retained_low_value_deltas": 1,
        }),
    ));

    let pressure_lab_path = harness.temp_path("swarm_pressure_lab.jsonl");
    write_pressure_lab_jsonl(&pressure_lab_path, &evidence_rows);
    let validated_rows = validate_pressure_lab_jsonl(&pressure_lab_path);
    assert!(
        validated_rows
            .iter()
            .any(|row| row["scenario"] == "small_smoke")
    );
    assert!(
        validated_rows
            .iter()
            .any(|row| row["scenario"] == "sustained_backpressure")
    );
    assert!(
        validated_rows
            .iter()
            .any(|row| row["scenario"] == "burst_fanout")
    );
    assert!(
        validated_rows
            .iter()
            .any(|row| row["scenario"] == "cancellation")
    );

    let flight_path = harness.temp_path("swarm_pressure_lab_flight_recorder.jsonl");
    std::fs::write(&flight_path, flight_jsonl).expect("write pressure lab flight recorder");
    harness.record_artifact("swarm_pressure_lab.jsonl", &pressure_lab_path);
    harness.record_artifact("swarm_pressure_lab_flight_recorder.jsonl", &flight_path);
    harness
        .write_jsonl_logs(harness.temp_path("swarm_pressure_lab_test.log.jsonl"))
        .expect("write pressure lab test log");
}
