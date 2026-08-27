//! E2E: Runtime hostcall telemetry and feature extraction validation (bd-2a9ll).
//!
//! Validates:
//! - deterministic telemetry artifact schema/export
//! - benign + adversarial + recovery hostcall flows
//! - structured JSONL logging with required runtime-risk fields

mod common;

use common::TestHarness;
use pi::connectors::http::HttpConnector;
use pi::extensions::{
    ExtensionManager, ExtensionPolicy, ExtensionPolicyMode, HostCallContext, HostCallPayload,
    RuntimeRiskConfig, dispatch_host_call_shared,
};
use pi::tools::ToolRegistry;
use serde_json::json;
use std::fs;

fn permissive_policy() -> ExtensionPolicy {
    ExtensionPolicy {
        mode: ExtensionPolicyMode::Permissive,
        max_memory_mb: 256,
        default_caps: Vec::new(),
        deny_caps: Vec::new(),
        ..Default::default()
    }
}

fn scenario_id(call_id: &str) -> &'static str {
    if call_id.starts_with("benign-") {
        "benign"
    } else if call_id.starts_with("adversarial-") {
        "adversarial"
    } else {
        "recovery"
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_runtime_hostcall_telemetry_logs_required_fields() {
    let harness = TestHarness::new("e2e_runtime_hostcall_telemetry_logs_required_fields");
    let correlation_id = format!("corr-{:016x}", harness.deterministic_seed());

    let tools = ToolRegistry::new(&[], harness.temp_dir(), None);
    let http = HttpConnector::with_defaults();
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(RuntimeRiskConfig {
        enabled: true,
        enforce: true,
        alpha: 0.01,
        window_size: 64,
        ledger_limit: 1024,
        decision_timeout_ms: 50,
        fail_closed: true,
    });

    let policy = permissive_policy();
    let ctx = HostCallContext {
        runtime_name: "e2e",
        extension_id: Some("ext.e2e.runtime-risk"),
        tools: &tools,
        http: &http,
        manager: Some(manager.clone()),
        policy: &policy,
        js_runtime: None,
        session_action_origin: None,
        interceptor: None,
    };

    futures::executor::block_on(async {
        // Benign flow
        for idx in 0..3 {
            let call = HostCallPayload {
                call_id: format!("benign-{idx}"),
                capability: "log".to_string(),
                method: "log".to_string(),
                params: json!({ "level": "info", "message": format!("benign-{idx}") }),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            };
            let _ = dispatch_host_call_shared(&ctx, call).await;
        }

        // Adversarial flow
        for idx in 0..6 {
            let call = HostCallPayload {
                call_id: format!("adversarial-{idx}"),
                capability: "exec".to_string(),
                method: "exec".to_string(),
                params: json!({ "cmd": "echo", "args": [idx.to_string()] }),
                timeout_ms: Some(25),
                cancel_token: None,
                context: None,
            };
            let _ = dispatch_host_call_shared(&ctx, call).await;
        }

        // Recovery flow
        for idx in 0..2 {
            let call = HostCallPayload {
                call_id: format!("recovery-{idx}"),
                capability: "log".to_string(),
                method: "log".to_string(),
                params: json!({ "level": "info", "message": format!("recovery-{idx}") }),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            };
            let _ = dispatch_host_call_shared(&ctx, call).await;
        }
    });

    let telemetry = manager.runtime_hostcall_telemetry_artifact();
    assert!(!telemetry.entries.is_empty(), "telemetry must not be empty");
    assert_eq!(telemetry.entry_count, telemetry.entries.len());

    for event in &telemetry.entries {
        assert!(
            matches!(event.lane.as_str(), "fast" | "compat" | "unknown"),
            "unexpected lane value for {}: {}",
            event.call_id,
            event.lane
        );
        assert!(
            !event.lane_matrix_key.trim().is_empty(),
            "lane matrix key must be populated for {}",
            event.call_id
        );
        assert!(
            event.lane_latency_share_bps <= 10_000,
            "lane latency share must be basis points for {}",
            event.call_id
        );
        assert!(
            !event.marshalling_path.trim().is_empty(),
            "marshalling path must be populated for {}",
            event.call_id
        );
        if let Some(trace_signature) = event
            .marshalling_superinstruction_trace_signature
            .as_deref()
        {
            assert!(
                !trace_signature.trim().is_empty(),
                "superinstruction trace signature must not be blank for {}",
                event.call_id
            );
        }
        if let Some(plan_id) = event.marshalling_superinstruction_plan_id.as_deref() {
            assert!(
                !plan_id.trim().is_empty(),
                "superinstruction plan id must not be blank for {}",
                event.call_id
            );
            assert!(
                event.marshalling_superinstruction_trace_signature.is_some(),
                "superinstruction plan id requires trace signature for {}",
                event.call_id
            );
        }
        if let Some(deopt_reason) = event.marshalling_superinstruction_deopt_reason.as_deref() {
            assert!(
                !deopt_reason.trim().is_empty(),
                "superinstruction deopt reason must not be blank for {}",
                event.call_id
            );
        }
        harness
            .log()
            .info_ctx("runtime_risk_telemetry", "runtime telemetry event", |ctx| {
                ctx.push(("issue_id".into(), "bd-2a9ll".into()));
                ctx.push((
                    "scenario_id".into(),
                    scenario_id(&event.call_id).to_string(),
                ));
                ctx.push(("extension_id".into(), event.extension_id.clone()));
                ctx.push(("capability".into(), event.capability.clone()));
                ctx.push(("policy_profile".into(), event.policy_profile.clone()));
                ctx.push(("score".into(), format!("{:.6}", event.risk_score)));
                ctx.push(("reason_codes".into(), event.reason_codes.join("|")));
                ctx.push((
                    "action".into(),
                    format!("{:?}", event.selected_action).to_lowercase(),
                ));
                ctx.push(("latency_ms".into(), event.latency_ms.to_string()));
                ctx.push(("lane".into(), event.lane.clone()));
                ctx.push(("lane_matrix_key".into(), event.lane_matrix_key.clone()));
                ctx.push((
                    "lane_dispatch_latency_ms".into(),
                    event.lane_dispatch_latency_ms.to_string(),
                ));
                ctx.push((
                    "lane_latency_share_bps".into(),
                    event.lane_latency_share_bps.to_string(),
                ));
                ctx.push(("marshalling_path".into(), event.marshalling_path.clone()));
                ctx.push((
                    "marshalling_latency_us".into(),
                    event.marshalling_latency_us.to_string(),
                ));
                ctx.push((
                    "marshalling_fallback_count".into(),
                    event.marshalling_fallback_count.to_string(),
                ));
                ctx.push((
                    "marshalling_superinstruction_plan_id".into(),
                    event
                        .marshalling_superinstruction_plan_id
                        .clone()
                        .unwrap_or_default(),
                ));
                ctx.push(("correlation_id".into(), correlation_id.clone()));
                ctx.push(("redaction_summary".into(), event.redaction_summary.clone()));
                ctx.push((
                    "explanation_level".into(),
                    format!("{:?}", event.explanation_level).to_lowercase(),
                ));
                ctx.push((
                    "top_contributors".into(),
                    event
                        .top_contributors
                        .iter()
                        .map(|item| item.code.clone())
                        .collect::<Vec<_>>()
                        .join("|"),
                ));
                ctx.push((
                    "budget_state".into(),
                    serde_json::to_string(&event.budget_state).expect("serialize budget_state"),
                ));
            });
    }

    let jsonl_path = harness.temp_path("runtime-risk-telemetry.log.jsonl");
    harness
        .write_jsonl_logs(&jsonl_path)
        .expect("write runtime telemetry jsonl");
    harness.record_artifact("runtime-risk-telemetry.log.jsonl", &jsonl_path);

    let raw = fs::read_to_string(&jsonl_path).expect("read telemetry jsonl");
    let mut matched_rows = 0usize;
    for line in raw.lines() {
        let value: serde_json::Value = serde_json::from_str(line).expect("valid jsonl line");
        if value.get("type").and_then(serde_json::Value::as_str) != Some("log") {
            continue;
        }
        if value.get("category").and_then(serde_json::Value::as_str)
            != Some("runtime_risk_telemetry")
        {
            continue;
        }

        let context = value
            .get("context")
            .and_then(serde_json::Value::as_object)
            .expect("context object");
        for key in [
            "issue_id",
            "scenario_id",
            "extension_id",
            "capability",
            "policy_profile",
            "score",
            "reason_codes",
            "action",
            "latency_ms",
            "lane",
            "lane_matrix_key",
            "lane_dispatch_latency_ms",
            "lane_latency_share_bps",
            "marshalling_path",
            "marshalling_latency_us",
            "marshalling_fallback_count",
            "marshalling_superinstruction_plan_id",
            "correlation_id",
            "redaction_summary",
            "explanation_level",
            "top_contributors",
            "budget_state",
        ] {
            assert!(context.contains_key(key), "missing context key: {key}");
        }
        assert!(
            value
                .get("ts")
                .and_then(serde_json::Value::as_str)
                .is_some(),
            "missing timestamp"
        );
        matched_rows = matched_rows.saturating_add(1);
    }

    assert!(
        matched_rows >= telemetry.entry_count,
        "expected >= {} telemetry rows, got {}",
        telemetry.entry_count,
        matched_rows
    );
}
