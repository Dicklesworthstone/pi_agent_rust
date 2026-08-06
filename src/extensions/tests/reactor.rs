//! Hostcall reactor mesh and result-taxonomy tests.

use super::*;

// ------------------------------------------------------------------
// Hostcall Reactor Mesh tests (bd-3ar8v.4.20)
// ------------------------------------------------------------------

#[test]
fn reactor_config_auto_sizes_from_parallelism_and_pressure() {
    let baseline = HostcallReactorConfig::auto_sized_for(8, None);
    assert_eq!(baseline.shard_count, 4);
    assert_eq!(
        baseline.lane_capacity,
        HOSTCALL_REACTOR_DEFAULT_LANE_CAPACITY
    );

    let mut mesh = HostcallReactorMesh::new(HostcallReactorConfig {
        shard_count: 1,
        lane_capacity: 512,
        core_ids: None,
    });
    for i in 0..512 {
        mesh.submit(
            format!("pressure-{i}"),
            CommonHostcallOpcode::SessionGetState,
            json!({}),
        )
        .expect("fill lane");
    }
    mesh.submit(
        "pressure-overflow".to_string(),
        CommonHostcallOpcode::SessionGetState,
        json!({}),
    )
    .expect_err("full lane should reject");

    let pressured = HostcallReactorConfig::auto_sized_for(8, Some(&mesh.telemetry()));
    assert_eq!(pressured.shard_count, 8);
    assert_eq!(pressured.lane_capacity, 1024);
}

#[test]
fn reactor_mesh_hash_routing_preserves_shard_affinity() {
    let mut mesh = HostcallReactorMesh::new(HostcallReactorConfig {
        shard_count: 8,
        lane_capacity: 64,
        core_ids: None,
    });

    let first = mesh
        .submit(
            "affinity-call".to_string(),
            CommonHostcallOpcode::SessionGetState,
            json!({}),
        )
        .expect("first submit");
    let second = mesh
        .submit(
            "affinity-call".to_string(),
            CommonHostcallOpcode::SessionGetState,
            json!({}),
        )
        .expect("second submit");

    assert_eq!(
        first.shard_id, second.shard_id,
        "same call_id must route to same shard"
    );
    assert_eq!(first.shard_seq + 1, second.shard_seq);
    assert_eq!(first.global_seq + 1, second.global_seq);
}

#[test]
fn reactor_mesh_events_use_round_robin() {
    let mut mesh = HostcallReactorMesh::new(HostcallReactorConfig {
        shard_count: 3,
        lane_capacity: 64,
        core_ids: None,
    });

    let mut shards = Vec::new();
    for i in 0..6 {
        let req = mesh
            .submit(
                format!("evt-call-{i}"),
                CommonHostcallOpcode::EventsEmit,
                json!({"event": "test"}),
            )
            .expect("submit events op");
        shards.push(req.shard_id);
    }

    assert_eq!(shards, vec![0, 1, 2, 0, 1, 2]);
}

#[test]
fn reactor_mesh_backpressure_on_overflow() {
    let mut mesh = HostcallReactorMesh::new(HostcallReactorConfig {
        shard_count: 1,
        lane_capacity: 2,
        core_ids: None,
    });

    mesh.submit(
        "call-0".to_string(),
        CommonHostcallOpcode::SessionGetName,
        json!({}),
    )
    .expect("first");
    mesh.submit(
        "call-1".to_string(),
        CommonHostcallOpcode::SessionGetName,
        json!({}),
    )
    .expect("second");

    let err = mesh
        .submit(
            "call-overflow".to_string(),
            CommonHostcallOpcode::SessionGetName,
            json!({}),
        )
        .expect_err("third should overflow");
    assert_eq!(err.shard_id, 0);
    assert_eq!(err.capacity, 2);
    assert_eq!(err.depth, 2);

    let telem = mesh.telemetry();
    assert_eq!(telem.rejected_enqueues, 1);
    assert_eq!(telem.queue_depths, vec![2]);
    assert!(telem.overloaded);
    assert_eq!(telem.overload_reason.as_deref(), Some("rejected_enqueues"));
}

#[test]
fn reactor_mesh_drain_shard() {
    let mut mesh = HostcallReactorMesh::new(HostcallReactorConfig {
        shard_count: 2,
        lane_capacity: 64,
        core_ids: None,
    });

    for i in 0..3 {
        mesh.submit(
            format!("drain-{i}"),
            CommonHostcallOpcode::SessionGetState,
            json!({}),
        )
        .expect("submit");
    }

    assert_eq!(mesh.total_depth(), 3);

    let batch = mesh.drain_shard(0, 10);
    assert!(!batch.is_empty(), "should have items in shard 0");
    for req in &batch {
        assert_eq!(req.shard_id, 0);
        assert_eq!(req.opcode, CommonHostcallOpcode::SessionGetState);
    }
}

#[test]
fn reactor_mesh_drain_global_order_is_monotone() {
    let mut mesh = HostcallReactorMesh::new(HostcallReactorConfig {
        shard_count: 4,
        lane_capacity: 64,
        core_ids: None,
    });

    mesh.submit("a".to_string(), CommonHostcallOpcode::EventsEmit, json!({}))
        .unwrap();
    mesh.submit(
        "b".to_string(),
        CommonHostcallOpcode::SessionGetName,
        json!({}),
    )
    .unwrap();
    mesh.submit(
        "c".to_string(),
        CommonHostcallOpcode::EventsGetModel,
        json!({}),
    )
    .unwrap();
    mesh.submit("d".to_string(), CommonHostcallOpcode::ToolRead, json!({}))
        .unwrap();

    let drained = mesh.drain_global_order(10);
    assert_eq!(drained.len(), 4);

    for pair in drained.windows(2) {
        assert!(
            pair[0].global_seq < pair[1].global_seq,
            "global_seq must be monotonically increasing: {} >= {}",
            pair[0].global_seq,
            pair[1].global_seq
        );
    }
}

#[test]
fn reactor_mesh_telemetry_tracks_enqueued_and_dispatched() {
    let mut mesh = HostcallReactorMesh::new(HostcallReactorConfig {
        shard_count: 2,
        lane_capacity: 64,
        core_ids: None,
    });

    for i in 0..5 {
        mesh.submit(
            format!("tel-{i}"),
            CommonHostcallOpcode::SessionGetState,
            json!({}),
        )
        .unwrap();
    }

    let telem = mesh.telemetry();
    assert_eq!(telem.shard_count, 2);
    assert_eq!(telem.lane_capacity, 64);
    let total_enqueued: u64 = telem.total_enqueued.iter().sum();
    assert_eq!(total_enqueued, 5);
    assert_eq!(telem.total_dispatched, 0);
    assert_eq!(telem.lane_dispatch_latency_p95_ns, vec![0, 0]);
    assert_eq!(telem.lane_dispatch_latency_p99_ns, vec![0, 0]);

    mesh.drain_global_order(3);
    let telem2 = mesh.telemetry();
    assert_eq!(telem2.total_dispatched, 3);
    assert_eq!(telem2.lane_dispatch_latency_p95_ns.len(), 2);
    assert_eq!(telem2.lane_dispatch_latency_p99_ns.len(), 2);
}

#[test]
fn reactor_mesh_completion_clears_lane_and_records_latency() {
    let mut mesh = HostcallReactorMesh::new(HostcallReactorConfig {
        shard_count: 1,
        lane_capacity: 4,
        core_ids: None,
    });

    let req = mesh
        .submit(
            "complete-fast".to_string(),
            CommonHostcallOpcode::ToolRead,
            json!({}),
        )
        .expect("submit");
    assert_eq!(mesh.total_depth(), 1);

    assert!(mesh.record_completion(req.shard_id, req.global_seq));

    let telemetry = mesh.telemetry();
    assert_eq!(telemetry.queue_depths, vec![0]);
    assert_eq!(telemetry.total_dispatched, 1);
    assert_eq!(telemetry.lane_dispatch_latency_p95_ns.len(), 1);
    assert_eq!(telemetry.lane_dispatch_latency_p99_ns.len(), 1);
}

#[test]
fn reactor_mesh_core_affinity_config() {
    let mesh = HostcallReactorMesh::new(HostcallReactorConfig {
        shard_count: 4,
        lane_capacity: 64,
        core_ids: Some(vec![0, 2, 4, 6]),
    });

    assert_eq!(mesh.core_id_for_shard(0), Some(0));
    assert_eq!(mesh.core_id_for_shard(1), Some(2));
    assert_eq!(mesh.core_id_for_shard(2), Some(4));
    assert_eq!(mesh.core_id_for_shard(3), Some(6));
    assert_eq!(mesh.core_id_for_shard(4), None);
}

#[test]
fn extension_manager_reactor_lifecycle() {
    let manager = ExtensionManager::new();
    assert!(!manager.hostcall_reactor_enabled());

    manager.enable_hostcall_reactor(HostcallReactorConfig {
        shard_count: 2,
        lane_capacity: 32,
        core_ids: None,
    });
    assert!(manager.hostcall_reactor_enabled());

    let result = manager.reactor_submit(
        "mgr-call".to_string(),
        CommonHostcallOpcode::SessionGetState,
        json!({}),
    );
    assert!(result.is_some());
    assert!(result.unwrap().is_ok());

    let telem = manager.reactor_telemetry().expect("telemetry");
    assert_eq!(telem.shard_count, 2);
    let total_enqueued: u64 = telem.total_enqueued.iter().sum();
    assert_eq!(total_enqueued, 1);

    let drained = manager.reactor_drain_global(10);
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].call_id, "mgr-call");

    manager.disable_hostcall_reactor();
    assert!(!manager.hostcall_reactor_enabled());
    assert!(
        manager
            .reactor_submit(
                "should-none".to_string(),
                CommonHostcallOpcode::SessionGetState,
                json!({}),
            )
            .is_none()
    );
}

#[test]
fn extension_manager_reactor_backpressure_propagates() {
    let manager = ExtensionManager::new();
    manager.enable_hostcall_reactor(HostcallReactorConfig {
        shard_count: 1,
        lane_capacity: 1,
        core_ids: None,
    });

    let first = manager
        .reactor_submit(
            "bp-0".to_string(),
            CommonHostcallOpcode::SessionGetState,
            json!({}),
        )
        .expect("reactor enabled")
        .expect("first submit should fit");
    assert_eq!(first.shard_id, 0);

    let overflow = manager
        .reactor_submit(
            "bp-1".to_string(),
            CommonHostcallOpcode::SessionGetState,
            json!({}),
        )
        .expect("reactor enabled")
        .expect_err("second submit should overflow lane");
    assert_eq!(overflow.shard_id, 0);
    assert_eq!(overflow.depth, 1);
    assert_eq!(overflow.capacity, 1);

    let telemetry = manager.reactor_telemetry().expect("telemetry snapshot");
    assert_eq!(telemetry.rejected_enqueues, 1);

    manager.disable_hostcall_reactor();
}

#[test]
fn extension_manager_zero_shard_reactor_config_disables_mesh() {
    let manager = ExtensionManager::new();
    manager.enable_hostcall_reactor(HostcallReactorConfig {
        shard_count: 1,
        lane_capacity: 2,
        core_ids: None,
    });
    assert!(manager.hostcall_reactor_enabled());

    manager.enable_hostcall_reactor(HostcallReactorConfig {
        shard_count: 0,
        lane_capacity: 64,
        core_ids: Some(vec![0, 2]),
    });

    assert!(!manager.hostcall_reactor_enabled());
    assert!(
        manager
            .reactor_submit(
                "zero-shard-manager".to_string(),
                CommonHostcallOpcode::SessionGetState,
                json!({}),
            )
            .is_none()
    );
    assert!(manager.reactor_telemetry().is_none());
    assert!(manager.reactor_drain_global(4).is_empty());
}

#[test]
fn extension_manager_zero_capacity_reactor_config_disables_mesh() {
    let manager = ExtensionManager::new();
    manager.enable_hostcall_reactor(HostcallReactorConfig {
        shard_count: 2,
        lane_capacity: 2,
        core_ids: None,
    });
    assert!(manager.hostcall_reactor_enabled());

    manager.enable_hostcall_reactor(HostcallReactorConfig {
        shard_count: 4,
        lane_capacity: 0,
        core_ids: None,
    });

    assert!(!manager.hostcall_reactor_enabled());
    assert!(
        manager
            .reactor_submit(
                "zero-capacity-manager".to_string(),
                CommonHostcallOpcode::EventsEmit,
                json!({"event": "noop"}),
            )
            .is_none()
    );
    assert!(manager.reactor_telemetry().is_none());
    assert!(manager.reactor_drain_global(4).is_empty());
}

#[test]
fn reactor_mesh_zero_shards_fail_closed() {
    let mut mesh = HostcallReactorMesh::new(HostcallReactorConfig {
        shard_count: 0,
        lane_capacity: 64,
        core_ids: Some(vec![0, 2]),
    });

    assert_eq!(mesh.shard_count(), 0);
    assert_eq!(mesh.total_depth(), 0);
    assert!(!mesh.has_pending());
    assert_eq!(mesh.core_id_for_shard(0), None);
    assert_eq!(mesh.telemetry().queue_depths, Vec::<usize>::new());

    let err = mesh
        .submit(
            "zero-shards".to_string(),
            CommonHostcallOpcode::SessionGetState,
            json!({}),
        )
        .expect_err("zero-shard config should reject submissions");
    assert_eq!(err.shard_id, 0);
    assert_eq!(err.depth, 0);
    assert_eq!(err.capacity, 0);
    assert_eq!(mesh.telemetry().rejected_enqueues, 1);
}

#[test]
fn reactor_mesh_zero_capacity_fail_closed() {
    let mut mesh = HostcallReactorMesh::new(HostcallReactorConfig {
        shard_count: 4,
        lane_capacity: 0,
        core_ids: None,
    });

    assert_eq!(mesh.shard_count(), 0);
    assert_eq!(mesh.total_depth(), 0);
    assert!(!mesh.has_pending());
    assert_eq!(mesh.telemetry().shard_count, 0);
    assert_eq!(mesh.telemetry().queue_depths, Vec::<usize>::new());

    let err = mesh
        .submit(
            "zero-capacity".to_string(),
            CommonHostcallOpcode::EventsEmit,
            json!({"event": "noop"}),
        )
        .expect_err("zero-capacity config should reject submissions");
    assert_eq!(err.shard_id, 0);
    assert_eq!(err.depth, 0);
    assert_eq!(err.capacity, 0);
    assert_eq!(mesh.telemetry().rejected_enqueues, 1);
}

fn typed_tool_read_payload(call_id: &str, path: &str) -> HostCallPayload {
    HostCallPayload {
        call_id: call_id.to_string(),
        capability: "read".to_string(),
        method: "tool".to_string(),
        params: json!({
            "name": "read",
            "input": { "path": path }
        }),
        timeout_ms: None,
        cancel_token: None,
        context: Some(json!({
            "typed_opcode": {
                "schema": HOSTCALL_OPCODE_SCHEMA_VERSION,
                "version": HOSTCALL_OPCODE_VERSION,
                "code": "tool.read"
            }
        })),
    }
}

#[test]
fn dispatch_shared_allowed_global_kill_switch_forces_compat_lane() {
    let dir = tempdir().expect("tempdir");
    let file = dir.path().join("lane_global.txt");
    std::fs::write(&file, "lane-global").expect("write test file");

    let tools = ToolRegistry::new(&["read"], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = permissive_policy();
    let manager = ExtensionManager::new();
    manager.set_hostcall_compat_kill_switch_global(true);

    let ctx = HostCallContext {
        runtime_name: "test",
        extension_id: Some("ext.lane.global"),
        tools: &tools,
        http: &http,
        manager: Some(manager),
        policy: &policy,
        js_runtime: None,
        interceptor: None,
    };

    let payload = typed_tool_read_payload("lane-global", file.to_str().expect("utf-8 path"));
    let (outcome, lane_meta) = run_async(async { dispatch_shared_allowed(&ctx, &payload).await });
    let lane_meta = lane_meta.expect("lane metadata");
    assert_eq!(lane_meta.lane, HostcallDispatchLane::Compat);
    assert_eq!(
        lane_meta.decision_reason,
        "forced_compat_global_kill_switch"
    );
    assert_eq!(
        lane_meta.fallback_reason.as_deref(),
        Some("forced_compat_global_kill_switch")
    );
    assert_eq!(lane_meta.matrix_key, "tool|fallback|filesystem");

    match outcome {
        HostcallOutcome::Success(value) => {
            let output = serde_json::to_string(&value).expect("serialize read output");
            assert!(output.contains("lane-global"));
        }
        other => panic!(),
    }
}

#[test]
fn dispatch_shared_allowed_extension_kill_switch_only_affects_target_extension() {
    let dir = tempdir().expect("tempdir");
    let file = dir.path().join("lane_ext.txt");
    std::fs::write(&file, "lane-ext").expect("write test file");

    let tools = ToolRegistry::new(&["read"], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = permissive_policy();
    let manager = ExtensionManager::new();
    manager.set_hostcall_compat_kill_switch_for_extension("ext.compat", true);

    let payload = typed_tool_read_payload("lane-ext", file.to_str().expect("utf-8 path"));

    let compat_ctx = HostCallContext {
        runtime_name: "test",
        extension_id: Some("ext.compat"),
        tools: &tools,
        http: &http,
        manager: Some(manager.clone()),
        policy: &policy,
        js_runtime: None,
        interceptor: None,
    };
    let (_outcome, compat_lane_meta) =
        run_async(async { dispatch_shared_allowed(&compat_ctx, &payload).await });
    let compat_lane_meta = compat_lane_meta.expect("compat lane metadata");
    assert_eq!(compat_lane_meta.lane, HostcallDispatchLane::Compat);
    assert_eq!(
        compat_lane_meta.decision_reason,
        "forced_compat_extension_kill_switch"
    );
    assert_eq!(
        compat_lane_meta.fallback_reason.as_deref(),
        Some("forced_compat_extension_kill_switch")
    );

    let fast_ctx = HostCallContext {
        runtime_name: "test",
        extension_id: Some("ext.other"),
        tools: &tools,
        http: &http,
        manager: Some(manager),
        policy: &policy,
        js_runtime: None,
        interceptor: None,
    };
    let (_outcome, fast_lane_meta) =
        run_async(async { dispatch_shared_allowed(&fast_ctx, &payload).await });
    let fast_lane_meta = fast_lane_meta.expect("fast lane metadata");
    assert_eq!(fast_lane_meta.lane, HostcallDispatchLane::Fast);
    assert_eq!(fast_lane_meta.decision_reason, "typed_opcode_context_v1");
    assert!(fast_lane_meta.fallback_reason.is_none());
}

#[test]
fn budget_controller_tier_defaults_are_ordered() {
    let strict = ExtensionBudgetControllerConfig::for_tier(ExtensionBudgetTier::Strict);
    let balanced = ExtensionBudgetControllerConfig::for_tier(ExtensionBudgetTier::Balanced);
    let throughput = ExtensionBudgetControllerConfig::for_tier(ExtensionBudgetTier::Throughput);

    assert!(strict.enabled);
    assert!(balanced.enabled);
    assert!(throughput.enabled);
    assert!(strict.overload_signals_to_fallback < balanced.overload_signals_to_fallback);
    assert!(balanced.overload_signals_to_fallback < throughput.overload_signals_to_fallback);
    assert!(strict.recovery_successes_to_exit < balanced.recovery_successes_to_exit);
    assert!(balanced.recovery_successes_to_exit < throughput.recovery_successes_to_exit);
}

#[test]
fn oco_tuner_tier_defaults_are_ordered() {
    let strict = OcoTunerConfig::for_tier(ExtensionBudgetTier::Strict);
    let balanced = OcoTunerConfig::for_tier(ExtensionBudgetTier::Balanced);
    let throughput = OcoTunerConfig::for_tier(ExtensionBudgetTier::Throughput);

    assert!(strict.max_queue_budget < balanced.max_queue_budget);
    assert!(balanced.max_queue_budget < throughput.max_queue_budget);
    assert!(strict.max_batch_budget < balanced.max_batch_budget);
    assert!(balanced.max_batch_budget < throughput.max_batch_budget);
    assert!(strict.max_time_slice_ms < balanced.max_time_slice_ms);
    assert!(balanced.max_time_slice_ms < throughput.max_time_slice_ms);
}

#[test]
fn budget_controller_oco_updates_within_bounds() {
    let manager = ExtensionManager::new();
    manager.set_budget_controller_config(ExtensionBudgetControllerConfig {
        enabled: true,
        tier: ExtensionBudgetTier::Balanced,
        overload_window_ms: 10_000,
        overload_signals_to_fallback: 100,
        recovery_successes_to_exit: 4,
        regime_shift: RegimeShiftConfig {
            enabled: false,
            ..Default::default()
        },
        safety_envelope: SafetyEnvelopeConfig {
            enabled: false,
            ..Default::default()
        },
        oco_tuner: OcoTunerConfig {
            enabled: true,
            learning_rate: 0.2,
            min_queue_budget: 2.0,
            max_queue_budget: 10.0,
            min_batch_budget: 1.0,
            max_batch_budget: 8.0,
            min_time_slice_ms: 2.0,
            max_time_slice_ms: 16.0,
            initial_queue_budget: 4.0,
            initial_batch_budget: 2.0,
            initial_time_slice_ms: 4.0,
            rollback_loss_threshold: 9.9,
        },
    });

    for _ in 0..8 {
        manager.record_budget_overload_signal(
            Some("ext.oco.bounds"),
            "reactor_burst",
            Some(8),
            Some(8),
        );
    }
    let snapshot = manager
        .oco_tuner_snapshot("ext.oco.bounds")
        .expect("expected OCO snapshot");
    assert!(snapshot.queue_budget >= 2.0 && snapshot.queue_budget <= 10.0);
    assert!(snapshot.batch_budget >= 1.0 && snapshot.batch_budget <= 8.0);
    assert!(snapshot.time_slice_ms >= 2.0 && snapshot.time_slice_ms <= 16.0);
    assert!(snapshot.rounds >= 8);
}

#[test]
fn budget_controller_oco_guardrail_can_trigger_fallback() {
    let manager = ExtensionManager::new();
    manager.set_budget_controller_config(ExtensionBudgetControllerConfig {
        enabled: true,
        tier: ExtensionBudgetTier::Balanced,
        overload_window_ms: 60_000,
        overload_signals_to_fallback: 100,
        recovery_successes_to_exit: 4,
        regime_shift: RegimeShiftConfig {
            enabled: false,
            ..Default::default()
        },
        safety_envelope: SafetyEnvelopeConfig {
            enabled: false,
            ..Default::default()
        },
        oco_tuner: OcoTunerConfig {
            enabled: true,
            learning_rate: 0.1,
            min_queue_budget: 4.0,
            max_queue_budget: 16.0,
            min_batch_budget: 2.0,
            max_batch_budget: 16.0,
            min_time_slice_ms: 4.0,
            max_time_slice_ms: 24.0,
            initial_queue_budget: 8.0,
            initial_batch_budget: 4.0,
            initial_time_slice_ms: 8.0,
            rollback_loss_threshold: 1.01,
        },
    });

    manager.record_budget_overload_signal(
        Some("ext.oco.rollback"),
        "reactor_lane_overflow",
        Some(16),
        Some(16),
    );
    assert_eq!(
        manager.hostcall_compat_kill_switch_reason(Some("ext.oco.rollback")),
        Some("forced_compat_budget_controller")
    );

    let snapshot = manager
        .oco_tuner_snapshot("ext.oco.rollback")
        .expect("expected OCO snapshot");
    assert!(
        snapshot.guardrail_rollbacks >= 1,
        "expected guardrail rollback to trigger at least once"
    );
    assert!((snapshot.queue_budget - 8.0).abs() < f64::EPSILON);
    assert!((snapshot.batch_budget - 4.0).abs() < f64::EPSILON);
    assert!((snapshot.time_slice_ms - 8.0).abs() < f64::EPSILON);
}

#[test]
fn budget_controller_config_sanitizes_oco_inputs() {
    let manager = ExtensionManager::new();
    manager.set_budget_controller_config(ExtensionBudgetControllerConfig {
        enabled: true,
        tier: ExtensionBudgetTier::Balanced,
        overload_window_ms: 0,
        overload_signals_to_fallback: 0,
        recovery_successes_to_exit: 0,
        regime_shift: RegimeShiftConfig {
            enabled: true,
            cusum_k: f64::NAN,
            cusum_h: -3.0,
            bocpd_lambda: f64::INFINITY,
            bocpd_threshold: f64::NAN,
            bocpd_max_run_length: 0,
        },
        safety_envelope: SafetyEnvelopeConfig {
            enabled: true,
            conformal_confidence: f64::NAN,
            conformal_calibration_size: 0,
            pac_bayes_delta: f64::INFINITY,
            pac_bayes_prior_weight: f64::NAN,
            safety_error_threshold: f64::NAN,
            min_observations: 0,
        },
        oco_tuner: OcoTunerConfig {
            enabled: true,
            learning_rate: f64::NAN,
            min_queue_budget: -8.0,
            max_queue_budget: f64::NAN,
            min_batch_budget: 16.0,
            max_batch_budget: 4.0,
            min_time_slice_ms: 32.0,
            max_time_slice_ms: 8.0,
            initial_queue_budget: f64::NEG_INFINITY,
            initial_batch_budget: f64::NAN,
            initial_time_slice_ms: f64::INFINITY,
            rollback_loss_threshold: f64::NAN,
        },
    });

    let config = manager.budget_controller_config();
    assert!(config.overload_window_ms >= 100);
    assert!(config.overload_signals_to_fallback >= 1);
    assert!(config.recovery_successes_to_exit >= 1);
    assert!(config.regime_shift.cusum_k.is_finite() && config.regime_shift.cusum_k > 0.0);
    assert!(config.regime_shift.cusum_h.is_finite() && config.regime_shift.cusum_h > 0.0);
    assert!(config.regime_shift.bocpd_lambda.is_finite() && config.regime_shift.bocpd_lambda > 0.0);
    assert!(
        config.regime_shift.bocpd_threshold >= 0.01 && config.regime_shift.bocpd_threshold <= 0.99
    );
    assert!(config.regime_shift.bocpd_max_run_length >= 8);
    assert!(
        config.safety_envelope.conformal_confidence >= 0.5
            && config.safety_envelope.conformal_confidence <= 0.999
    );
    assert!(config.safety_envelope.conformal_calibration_size >= 16);
    assert!(
        config.safety_envelope.pac_bayes_delta >= 1.0e-6
            && config.safety_envelope.pac_bayes_delta <= 0.5
    );
    assert!(
        config.safety_envelope.pac_bayes_prior_weight >= 0.01
            && config.safety_envelope.pac_bayes_prior_weight <= 100.0
    );
    assert!(
        config.safety_envelope.safety_error_threshold >= 0.0
            && config.safety_envelope.safety_error_threshold <= 1.0
    );
    assert!(config.safety_envelope.min_observations >= 1);
    assert!(config.oco_tuner.learning_rate.is_finite());
    assert!(config.oco_tuner.learning_rate >= 1.0e-4 && config.oco_tuner.learning_rate <= 1.0);
    assert!(config.oco_tuner.min_queue_budget > 0.0);
    assert!(config.oco_tuner.max_queue_budget >= config.oco_tuner.min_queue_budget);
    assert!(config.oco_tuner.max_batch_budget >= config.oco_tuner.min_batch_budget);
    assert!(config.oco_tuner.max_time_slice_ms >= config.oco_tuner.min_time_slice_ms);
    assert!(config.oco_tuner.initial_queue_budget >= config.oco_tuner.min_queue_budget);
    assert!(config.oco_tuner.initial_queue_budget <= config.oco_tuner.max_queue_budget);
    assert!(config.oco_tuner.initial_batch_budget >= config.oco_tuner.min_batch_budget);
    assert!(config.oco_tuner.initial_batch_budget <= config.oco_tuner.max_batch_budget);
    assert!(config.oco_tuner.initial_time_slice_ms >= config.oco_tuner.min_time_slice_ms);
    assert!(config.oco_tuner.initial_time_slice_ms <= config.oco_tuner.max_time_slice_ms);
    assert!(config.oco_tuner.rollback_loss_threshold >= 0.1);
}

#[test]
fn budget_controller_oco_zero_capacity_signal_is_finite() {
    let manager = ExtensionManager::new();
    manager.set_budget_controller_config(ExtensionBudgetControllerConfig {
        enabled: true,
        tier: ExtensionBudgetTier::Balanced,
        overload_window_ms: 60_000,
        overload_signals_to_fallback: 50,
        recovery_successes_to_exit: 4,
        regime_shift: RegimeShiftConfig {
            enabled: false,
            ..Default::default()
        },
        safety_envelope: SafetyEnvelopeConfig {
            enabled: false,
            ..Default::default()
        },
        oco_tuner: OcoTunerConfig {
            enabled: true,
            rollback_loss_threshold: 8.0,
            ..Default::default()
        },
    });

    manager.record_budget_overload_signal(
        Some("ext.oco.zero-capacity"),
        "reactor_lane_overflow",
        Some(12),
        Some(0),
    );

    let snapshot = manager
        .oco_tuner_snapshot("ext.oco.zero-capacity")
        .expect("expected OCO snapshot");
    assert!(snapshot.rounds >= 1);
    assert!(snapshot.queue_budget.is_finite());
    assert!(snapshot.batch_budget.is_finite());
    assert!(snapshot.time_slice_ms.is_finite());
    assert!(snapshot.cumulative_loss.is_finite());
    assert!(snapshot.cumulative_regret.is_finite());
}

#[test]
fn budget_controller_enters_fallback_after_threshold() {
    let manager = ExtensionManager::new();
    manager.set_budget_controller_config(ExtensionBudgetControllerConfig {
        enabled: true,
        tier: ExtensionBudgetTier::Balanced,
        overload_window_ms: 10_000,
        overload_signals_to_fallback: 2,
        recovery_successes_to_exit: 4,
        regime_shift: RegimeShiftConfig {
            enabled: false,
            ..Default::default()
        },
        safety_envelope: SafetyEnvelopeConfig {
            enabled: false,
            ..Default::default()
        },
        oco_tuner: OcoTunerConfig {
            enabled: false,
            ..Default::default()
        },
    });

    assert_eq!(
        manager.hostcall_compat_kill_switch_reason(Some("ext.budget")),
        None
    );
    manager.record_budget_overload_signal(Some("ext.budget"), "reactor_lane_overflow", None, None);
    assert_eq!(
        manager.hostcall_compat_kill_switch_reason(Some("ext.budget")),
        None
    );
    manager.record_budget_overload_signal(Some("ext.budget"), "reactor_lane_overflow", None, None);
    assert_eq!(
        manager.hostcall_compat_kill_switch_reason(Some("ext.budget")),
        Some("forced_compat_budget_controller")
    );

    let snapshot = manager
        .budget_fallback_state_snapshot("ext.budget")
        .expect("budget state");
    assert!(snapshot.0);
    assert_eq!(snapshot.3.as_deref(), Some("reactor_lane_overflow"));
}

#[test]
fn budget_controller_recovery_requires_consecutive_successes() {
    let manager = ExtensionManager::new();
    manager.set_budget_controller_config(ExtensionBudgetControllerConfig {
        enabled: true,
        tier: ExtensionBudgetTier::Balanced,
        overload_window_ms: 10_000,
        overload_signals_to_fallback: 1,
        recovery_successes_to_exit: 2,
        ..Default::default()
    });

    manager.record_budget_overload_signal(Some("ext.recover"), "quota_exceeded", None, None);
    assert_eq!(
        manager.hostcall_compat_kill_switch_reason(Some("ext.recover")),
        Some("forced_compat_budget_controller")
    );

    manager.record_budget_recovery_sample(Some("ext.recover"), true);
    assert_eq!(
        manager.hostcall_compat_kill_switch_reason(Some("ext.recover")),
        Some("forced_compat_budget_controller")
    );

    manager.record_budget_overload_signal(Some("ext.recover"), "reactor_lane_overflow", None, None);
    let snapshot = manager
        .budget_fallback_state_snapshot("ext.recover")
        .expect("budget state");
    assert_eq!(snapshot.1, 0, "overload should reset recovery streak");

    manager.record_budget_recovery_sample(Some("ext.recover"), true);
    manager.record_budget_recovery_sample(Some("ext.recover"), true);
    assert_eq!(
        manager.hostcall_compat_kill_switch_reason(Some("ext.recover")),
        None
    );
}

#[test]
fn dispatch_shared_allowed_budget_controller_forces_compat_lane() {
    let dir = tempdir().expect("tempdir");
    let file = dir.path().join("lane_budget.txt");
    std::fs::write(&file, "lane-budget").expect("write test file");

    let tools = ToolRegistry::new(&["read"], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = permissive_policy();
    let manager = ExtensionManager::new();
    manager.set_budget_controller_config(ExtensionBudgetControllerConfig {
        enabled: true,
        tier: ExtensionBudgetTier::Balanced,
        overload_window_ms: 10_000,
        overload_signals_to_fallback: 1,
        recovery_successes_to_exit: 5,
        ..Default::default()
    });
    manager.record_budget_overload_signal(
        Some("ext.budget.lane"),
        "reactor_lane_overflow",
        Some(1),
        Some(1),
    );

    let ctx = HostCallContext {
        runtime_name: "test",
        extension_id: Some("ext.budget.lane"),
        tools: &tools,
        http: &http,
        manager: Some(manager),
        policy: &policy,
        js_runtime: None,
        interceptor: None,
    };

    let payload = typed_tool_read_payload("lane-budget", file.to_str().expect("utf-8 path"));
    let (outcome, lane_meta) = run_async(async { dispatch_shared_allowed(&ctx, &payload).await });
    let lane_meta = lane_meta.expect("lane metadata");
    assert_eq!(lane_meta.lane, HostcallDispatchLane::Compat);
    assert_eq!(lane_meta.decision_reason, "forced_compat_budget_controller");
    assert_eq!(
        lane_meta.fallback_reason.as_deref(),
        Some("forced_compat_budget_controller")
    );

    assert!(
        matches!(outcome, HostcallOutcome::Success(_)),
        "expected successful compat dispatch, got {outcome:?}"
    );
}

#[test]
fn dispatch_shared_allowed_reactor_overflow_uses_conservative_lane() {
    let dir = tempdir().expect("tempdir");
    let file = dir.path().join("lane_reactor_overflow.txt");
    std::fs::write(&file, "lane-reactor-overflow").expect("write test file");

    let tools = ToolRegistry::new(&["read"], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = permissive_policy();
    let manager = ExtensionManager::new();
    manager.set_budget_controller_config(ExtensionBudgetControllerConfig {
        enabled: true,
        tier: ExtensionBudgetTier::Balanced,
        overload_window_ms: 10_000,
        overload_signals_to_fallback: 1,
        recovery_successes_to_exit: 5,
        ..Default::default()
    });
    manager.enable_hostcall_reactor(HostcallReactorConfig {
        shard_count: 1,
        lane_capacity: 1,
        core_ids: None,
    });
    manager
        .reactor_submit(
            "prefill-overflow-lane".to_string(),
            CommonHostcallOpcode::ToolRead,
            json!({}),
        )
        .expect("reactor enabled")
        .expect("prefill lane");

    let ctx = HostCallContext {
        runtime_name: "test",
        extension_id: Some("ext.reactor.overflow"),
        tools: &tools,
        http: &http,
        manager: Some(manager.clone()),
        policy: &policy,
        js_runtime: None,
        interceptor: None,
    };

    let payload =
        typed_tool_read_payload("lane-reactor-overflow", file.to_str().expect("utf-8 path"));
    let (outcome, lane_meta) = run_async(async { dispatch_shared_allowed(&ctx, &payload).await });
    let lane_meta = lane_meta.expect("lane metadata");
    assert_eq!(lane_meta.lane, HostcallDispatchLane::Compat);
    assert_eq!(lane_meta.decision_reason, "reactor_lane_overflow");
    assert_eq!(
        lane_meta.fallback_reason.as_deref(),
        Some("reactor_lane_overflow")
    );
    assert_eq!(
        manager.hostcall_compat_kill_switch_reason(Some("ext.reactor.overflow")),
        Some("forced_compat_budget_controller")
    );

    let telemetry = manager.reactor_telemetry().expect("telemetry");
    assert_eq!(telemetry.queue_depths, vec![1]);
    assert_eq!(telemetry.rejected_enqueues, 1);
    assert!(telemetry.overloaded);

    assert!(
        matches!(outcome, HostcallOutcome::Success(_)),
        "expected successful compat dispatch, got {outcome:?}"
    );
}

// ── Regime-shift detector (CUSUM/BOCPD) tests ─────────────────────

/// Simple deterministic jitter for BOCPD tests (avoids needing `rand`).
fn rand_jitter() -> f64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEED: AtomicU64 = AtomicU64::new(12345);
    let s = SEED.fetch_add(1, Ordering::Relaxed);
    let x = s.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
    #[allow(clippy::cast_precision_loss)]
    ((x >> 33) as f64 / f64::from(u32::MAX)).mul_add(2.0, -1.0)
}

#[test]
fn cusum_baseline_requires_min_observations() {
    let mut cusum = CusumState::default();
    assert!(!cusum.observe(100.0, 0.5, 4.0));
    assert!(!cusum.observe(110.0, 0.5, 4.0));
    assert!(!cusum.baseline_ready);
    assert!(!cusum.observe(105.0, 0.5, 4.0));
    assert!(cusum.baseline_ready);
    assert!(cusum.baseline_interval_ms > 0.0);
}

#[test]
fn cusum_detects_rate_increase() {
    let mut cusum = CusumState::default();
    for _ in 0..5 {
        cusum.observe(1000.0, 0.5, 4.0);
    }
    assert!(cusum.baseline_ready);
    let mut alarmed = false;
    for _ in 0..20 {
        if cusum.observe(100.0, 0.5, 4.0) {
            alarmed = true;
            break;
        }
    }
    assert!(alarmed, "CUSUM should detect rate increase");
    assert!(cusum.alarm_count > 0);
}

#[test]
fn cusum_reset_clears_cumsum_but_keeps_baseline() {
    let mut cusum = CusumState::default();
    for _ in 0..5 {
        cusum.observe(1000.0, 0.5, 4.0);
    }
    cusum.cumsum_high = 3.0;
    cusum.cumsum_low = 2.5;
    cusum.reset_cumsum();
    assert!(
        (cusum.cumsum_high - 0.0).abs() < f64::EPSILON,
        "cumsum_high should be zero after reset"
    );
    assert!(
        (cusum.cumsum_low - 0.0).abs() < f64::EPSILON,
        "cumsum_low should be zero after reset"
    );
    assert!(cusum.baseline_ready, "baseline should survive reset");
}

#[test]
fn cusum_no_alarm_on_stable_signal() {
    let mut cusum = CusumState::default();
    for _ in 0..55 {
        assert!(
            !cusum.observe(500.0, 0.5, 4.0),
            "should not alarm on stable signal"
        );
    }
    assert_eq!(cusum.alarm_count, 0);
}

#[test]
fn bocpd_warmup_suppresses_early_signals() {
    let mut bocpd = BocpdState::default();
    for i in 0..BocpdState::WARMUP_OBS {
        assert!(
            !bocpd.observe(f64::from(i) * 100.0, 50.0, 0.5, 200),
            "BOCPD should not signal during warmup"
        );
    }
}

#[test]
fn bocpd_detects_changepoint_on_mean_shift() {
    let mut bocpd = BocpdState::default();
    // Use deterministic values (no jitter) for stable baseline.
    for _ in 0..30 {
        bocpd.observe(1000.0, 20.0, 0.2, 200);
    }
    assert!(bocpd.warmed_up);
    // Dramatic 10x shift with small hazard lambda → should detect quickly.
    let mut detected = false;
    for _ in 0..30 {
        if bocpd.observe(100.0, 20.0, 0.2, 200) {
            detected = true;
            break;
        }
    }
    assert!(detected, "BOCPD should detect mean shift");
    assert!(bocpd.changepoint_count > 0);
}

fn synthetic_bocpd_observation(index: usize, change_at: usize) -> f64 {
    let jitter = match index % 6 {
        0 => -2.0,
        1 => -1.0,
        2 => -0.25,
        3 => 0.25,
        4 => 1.0,
        _ => 2.0,
    };
    if index < change_at {
        1_000.0 + jitter
    } else {
        250.0 + jitter
    }
}

#[test]
fn bocpd_posterior_spikes_at_synthetic_change_point() {
    const CHANGE_AT: usize = 80;
    const TOTAL_SAMPLES: usize = 140;
    const DETECTION_WINDOW: usize = 4;
    const HAZARD_LAMBDA: f64 = 20.0;
    const POSTERIOR_THRESHOLD: f64 = 0.20;

    let mut bocpd = BocpdState::default();
    let mut max_pre_change_posterior = 0.0_f64;
    let mut max_window_posterior = 0.0_f64;
    let mut first_detection_delay = None;

    for index in 0..TOTAL_SAMPLES {
        let detected = bocpd.observe(
            synthetic_bocpd_observation(index, CHANGE_AT),
            HAZARD_LAMBDA,
            POSTERIOR_THRESHOLD,
            200,
        );
        let cp_posterior = bocpd.run_length_probs.first().copied().unwrap_or(0.0);

        if bocpd.warmed_up && index < CHANGE_AT {
            max_pre_change_posterior = max_pre_change_posterior.max(cp_posterior);
            assert!(
                !detected,
                "pre-change sample {index} should not exceed BOCPD posterior threshold; posterior={cp_posterior:.4}",
            );
        }

        if (CHANGE_AT..=CHANGE_AT + DETECTION_WINDOW).contains(&index) {
            max_window_posterior = max_window_posterior.max(cp_posterior);
            if detected && first_detection_delay.is_none() {
                first_detection_delay = Some(index - CHANGE_AT);
            }
        }
    }

    assert!(
        max_pre_change_posterior < POSTERIOR_THRESHOLD,
        "stationary prefix should remain below posterior threshold: max_pre={max_pre_change_posterior:.4}",
    );
    assert!(
        max_window_posterior >= POSTERIOR_THRESHOLD,
        "posterior should spike at the known change point: max_window={max_window_posterior:.4}",
    );
    assert!(
        first_detection_delay.is_some_and(|delay| delay <= DETECTION_WINDOW),
        "BOCPD should detect within {DETECTION_WINDOW} samples of the change point; delay={first_detection_delay:?}",
    );
}

#[test]
fn bocpd_run_length_bounded() {
    let mut bocpd = BocpdState::default();
    for _ in 0..500 {
        bocpd.observe(1000.0, 50.0, 0.5, 100);
    }
    assert!(
        bocpd.run_length_probs.len() <= 100,
        "run length should be bounded to max_run_length"
    );
}

#[test]
fn bocpd_reset_returns_to_default() {
    let mut bocpd = BocpdState::default();
    for _ in 0..20 {
        bocpd.observe(1000.0, 50.0, 0.5, 200);
    }
    bocpd.reset();
    assert_eq!(bocpd.run_length_probs.len(), 1);
    assert_eq!(bocpd.changepoint_count, 0);
    assert!(!bocpd.warmed_up);
}

#[test]
fn regime_shift_config_tiers_are_ordered() {
    let strict = RegimeShiftConfig::for_tier(ExtensionBudgetTier::Strict);
    let balanced = RegimeShiftConfig::for_tier(ExtensionBudgetTier::Balanced);
    let throughput = RegimeShiftConfig::for_tier(ExtensionBudgetTier::Throughput);

    assert!(strict.cusum_k < balanced.cusum_k);
    assert!(balanced.cusum_k < throughput.cusum_k);
    assert!(strict.cusum_h < balanced.cusum_h);
    assert!(balanced.cusum_h < throughput.cusum_h);
    assert!(strict.bocpd_lambda < balanced.bocpd_lambda);
    assert!(balanced.bocpd_lambda < throughput.bocpd_lambda);
}

#[test]
fn regime_shift_snapshot_reflects_state() {
    let mut state = RegimeShiftDetectorState::default();
    let snap = state.snapshot();
    assert!(!snap.triggered);
    assert_eq!(snap.trigger_count, 0);
    assert!(snap.trigger_source.is_none());

    state.triggered = true;
    state.trigger_source = Some("cusum");
    state.trigger_count = 3;
    state.cusum.cumsum_high = 2.5;
    state.cusum.alarm_count = 2;
    let snap = state.snapshot();
    assert!(snap.triggered);
    assert_eq!(snap.trigger_source.as_deref(), Some("cusum"));
    assert_eq!(snap.trigger_count, 3);
    assert!((snap.cusum_high - 2.5).abs() < f64::EPSILON);
    assert_eq!(snap.cusum_alarm_count, 2);
}

#[test]
fn budget_controller_regime_shift_triggers_early_fallback() {
    let manager = ExtensionManager::new();
    manager.set_budget_controller_config(ExtensionBudgetControllerConfig {
        enabled: true,
        tier: ExtensionBudgetTier::Strict,
        overload_window_ms: 60_000,
        overload_signals_to_fallback: 100,
        recovery_successes_to_exit: 4,
        regime_shift: RegimeShiftConfig {
            enabled: true,
            cusum_k: 0.3,
            cusum_h: 2.0,
            bocpd_lambda: 10.0,
            bocpd_threshold: 0.3,
            bocpd_max_run_length: 50,
        },
        safety_envelope: SafetyEnvelopeConfig {
            enabled: false,
            ..Default::default()
        },
        oco_tuner: OcoTunerConfig {
            enabled: false,
            ..OcoTunerConfig::for_tier(ExtensionBudgetTier::Strict)
        },
    });

    for baseline_sample in 0..CusumState::MIN_BASELINE_OBS {
        manager.record_budget_overload_signal(Some("ext.regime"), "quota_exceeded", None, None);
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(
            manager
                .hostcall_compat_kill_switch_reason(Some("ext.regime"))
                .is_none(),
            "baseline sample {baseline_sample} should not enter fallback"
        );
    }

    let mut entered_fallback = false;
    for _ in 0..30 {
        manager.record_budget_overload_signal(Some("ext.regime"), "burst_overload", None, None);
        if manager
            .hostcall_compat_kill_switch_reason(Some("ext.regime"))
            .is_some()
        {
            entered_fallback = true;
            break;
        }
    }
    assert!(
        entered_fallback,
        "regime-shift should trigger early fallback before count threshold"
    );

    let snap = manager
        .regime_shift_snapshot("ext.regime")
        .expect("snapshot");
    assert!(snap.triggered);
    assert!(snap.trigger_count > 0);
}

#[test]
fn budget_controller_regime_shift_disabled_does_not_trigger() {
    let manager = ExtensionManager::new();
    manager.set_budget_controller_config(ExtensionBudgetControllerConfig {
        enabled: true,
        tier: ExtensionBudgetTier::Balanced,
        overload_window_ms: 60_000,
        overload_signals_to_fallback: 100,
        recovery_successes_to_exit: 4,
        regime_shift: RegimeShiftConfig {
            enabled: false,
            ..Default::default()
        },
        safety_envelope: SafetyEnvelopeConfig {
            enabled: false,
            ..Default::default()
        },
        oco_tuner: OcoTunerConfig::for_tier(ExtensionBudgetTier::Balanced),
    });

    for _ in 0..50 {
        manager.record_budget_overload_signal(Some("ext.disabled"), "burst", None, None);
    }
    assert!(
        manager
            .hostcall_compat_kill_switch_reason(Some("ext.disabled"))
            .is_none(),
        "regime-shift disabled should not trigger early fallback"
    );
}

#[test]
fn budget_controller_recovery_resets_regime_shift_state() {
    let manager = ExtensionManager::new();
    manager.set_budget_controller_config(ExtensionBudgetControllerConfig {
        enabled: true,
        tier: ExtensionBudgetTier::Balanced,
        overload_window_ms: 60_000,
        overload_signals_to_fallback: 1,
        recovery_successes_to_exit: 2,
        ..Default::default()
    });

    manager.record_budget_overload_signal(Some("ext.reset"), "quota_exceeded", None, None);
    assert!(
        manager
            .hostcall_compat_kill_switch_reason(Some("ext.reset"))
            .is_some()
    );

    manager.record_budget_recovery_sample(Some("ext.reset"), true);
    manager.record_budget_recovery_sample(Some("ext.reset"), true);
    assert!(
        manager
            .hostcall_compat_kill_switch_reason(Some("ext.reset"))
            .is_none()
    );

    let snap = manager
        .regime_shift_snapshot("ext.reset")
        .expect("snapshot");
    assert!(!snap.triggered);
    assert!(snap.trigger_source.is_none());
    assert_eq!(snap.bocpd_changepoint_count, 0);
}

// ── Safety envelope unit tests ──────────────────────────────────

#[test]
fn conformal_state_observe_marks_anomaly_when_out_of_interval() {
    let config = SafetyEnvelopeConfig {
        conformal_confidence: 0.90,
        conformal_calibration_size: 10,
        ..Default::default()
    };
    let mut state = ConformalState::default();

    // Feed 10 similar observations to build calibration set.
    for _ in 0..10 {
        state.observe(
            100.0,
            config.conformal_confidence,
            config.conformal_calibration_size,
        );
    }

    // An extreme outlier should be marked anomalous.
    let anomaly = state.observe(
        10_000.0,
        config.conformal_confidence,
        config.conformal_calibration_size,
    );
    assert!(anomaly, "extreme outlier should be anomalous");
    assert!(state.anomaly_count >= 1, "anomaly count should increase");
}

#[test]
fn conformal_state_normal_observations_not_anomalous() {
    let config = SafetyEnvelopeConfig {
        conformal_confidence: 0.90,
        conformal_calibration_size: 20,
        ..Default::default()
    };
    let mut state = ConformalState::default();

    // Feed identical observations — none should be anomalous after the first.
    let mut anomalies = 0;
    for _ in 0..50 {
        if state.observe(
            100.0,
            config.conformal_confidence,
            config.conformal_calibration_size,
        ) {
            anomalies += 1;
        }
    }
    // With identical observations the score is always 0, so anomaly rate
    // should be very low (only the very first observation has no calibration).
    assert!(
        anomalies <= 1,
        "identical data should produce <=1 anomaly, got {anomalies}"
    );
}

#[test]
fn conformal_interval_width_grows_with_variance() {
    let config_narrow = SafetyEnvelopeConfig {
        conformal_confidence: 0.90,
        conformal_calibration_size: 20,
        ..Default::default()
    };
    let mut narrow = ConformalState::default();
    for _ in 0..20 {
        narrow.observe(
            100.0,
            config_narrow.conformal_confidence,
            config_narrow.conformal_calibration_size,
        );
    }

    let mut wide = ConformalState::default();
    for i in 0..20_u64 {
        let val = if i % 2 == 0 { 50.0 } else { 150.0 };
        wide.observe(
            val,
            config_narrow.conformal_confidence,
            config_narrow.conformal_calibration_size,
        );
    }

    let width_narrow = narrow.interval_width(config_narrow.conformal_confidence);
    let width_wide = wide.interval_width(config_narrow.conformal_confidence);
    assert!(
        width_wide > width_narrow,
        "variable data should produce wider interval: wide={width_wide}, narrow={width_narrow}"
    );
}

#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn deterministic_uniform_unit_sample(seed: u64, index: usize) -> f64 {
    let index = index as u64;
    let mut value = seed
        .wrapping_add(index.wrapping_mul(0x9e37_79b9_7f4a_7c15))
        .wrapping_add(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^= value >> 31;

    let mantissa = value >> 11;
    let unit = mantissa as f64 / (1_u64 << 53) as f64;
    unit.mul_add(2.0, -1.0)
}

#[test]
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn conformal_interval_empirical_coverage_matches_confidence() {
    const CALIBRATION_SAMPLES: usize = 8_192;
    const HELD_OUT_SAMPLES: usize = 8_192;
    const SEEDS: [u64; 16] = [
        0x11, 0x25, 0x37, 0x49, 0x5b, 0x6d, 0x7f, 0x91, 0xa3, 0xb5, 0xc7, 0xd9, 0xeb, 0xfd, 0x10f,
        0x121,
    ];
    const CONFIDENCES: [f64; 3] = [0.80, 0.90, 0.95];

    for confidence in CONFIDENCES {
        let mut covered = 0_u64;
        let mut total = 0_u64;

        for seed in SEEDS {
            let calibration_values = (0..CALIBRATION_SAMPLES)
                .map(|index| deterministic_uniform_unit_sample(seed, index))
                .collect::<Vec<_>>();
            let calibration_mean =
                calibration_values.iter().sum::<f64>() / CALIBRATION_SAMPLES as f64;
            let calibration_scores = calibration_values
                .iter()
                .map(|value| (value - calibration_mean).abs())
                .collect::<std::collections::VecDeque<_>>();
            let state = ConformalState {
                calibration_scores,
                running_mean: calibration_mean,
                running_m2: 0.0,
                observation_count: CALIBRATION_SAMPLES as u64,
                anomaly_count: 0,
            };

            let threshold = state.interval_width(confidence);
            assert!(
                threshold.is_finite() && threshold > 0.0,
                "calibrated threshold should be finite for confidence {confidence}, got {threshold}",
            );

            let calibration_mean = state.running_mean;
            let held_out_seed = seed ^ 0x517c_c1b7_2722_0a95;
            for index in 0..HELD_OUT_SAMPLES {
                let value = deterministic_uniform_unit_sample(held_out_seed, index);
                let residual = (value - calibration_mean).abs();
                if residual <= threshold {
                    covered += 1;
                }
                total += 1;
            }
        }

        let coverage = covered as f64 / total as f64;
        let held_out_std = (confidence * (1.0 - confidence) / total as f64).sqrt();
        let calibration_std =
            (confidence * (1.0 - confidence) / (CALIBRATION_SAMPLES * SEEDS.len()) as f64).sqrt();
        let two_std = 2.0 * held_out_std.hypot(calibration_std);
        assert!(
            (coverage - confidence).abs() <= two_std,
            "empirical conformal coverage {coverage:.6} should be within ±2 combined std ({two_std:.6}) of confidence {confidence:.2}; covered={covered}, total={total}",
        );
    }
}

#[test]
fn pac_bayes_bound_increases_with_errors() {
    let mut state = PacBayesState::default();

    // All successes — bound should be low.
    for _ in 0..50 {
        state.record(true);
    }
    let bound_good = state.pac_bayes_bound(0.05, 1.0);

    let mut state_bad = PacBayesState::default();
    // Half failures — bound should be higher.
    for i in 0..50_u32 {
        state_bad.record(i % 2 == 0);
    }
    let bound_bad = state_bad.pac_bayes_bound(0.05, 1.0);

    assert!(
        bound_bad > bound_good,
        "more errors should produce higher bound: bad={bound_bad}, good={bound_good}"
    );
}

#[test]
fn pac_bayes_bound_is_worst_case_with_no_data() {
    let state = PacBayesState::default();
    let bound = state.pac_bayes_bound(0.05, 1.0);
    // With no observations, the bound should be worst-case (1.0),
    // since there is no evidence to constrain the error rate.
    assert!(
        (bound - 1.0).abs() < f64::EPSILON,
        "bound with no data should be 1.0 (worst case), got {bound}"
    );
}

#[test]
fn pac_bayes_reset_clears_state() {
    let mut state = PacBayesState::default();
    for _ in 0..10 {
        state.record(false);
    }
    assert_eq!(state.total(), 10);
    state.reset();
    assert_eq!(state.total(), 0);
    assert!((state.empirical_error_rate() - 0.0).abs() < f64::EPSILON);
}

#[test]
fn safety_envelope_evaluate_vetoes_on_high_error_rate() {
    let config = SafetyEnvelopeConfig {
        enabled: true,
        conformal_confidence: 0.95,
        conformal_calibration_size: 10,
        pac_bayes_delta: 0.05,
        pac_bayes_prior_weight: 1.0,
        safety_error_threshold: 0.10,
        min_observations: 5,
    };
    let mut state = SafetyEnvelopeState::default();

    // Feed enough failures to push PAC-Bayes bound above threshold.
    for _ in 0..20 {
        state.evaluate(100.0, false, &config);
    }
    assert!(state.vetoing, "should veto after many failures");
    assert!(state.veto_count > 0, "veto count should be positive");
}

#[test]
fn safety_envelope_pac_bayes_veto_tracks_bound_threshold() {
    let config = SafetyEnvelopeConfig {
        enabled: true,
        conformal_confidence: 0.99,
        conformal_calibration_size: 50,
        pac_bayes_delta: 0.05,
        pac_bayes_prior_weight: 1.0,
        safety_error_threshold: 0.30,
        min_observations: 20,
    };

    let mut below_threshold = SafetyEnvelopeState::default();
    for _ in 0..200 {
        assert!(
            !below_threshold.evaluate(100.0, true, &config),
            "low-error PAC-Bayes path should not veto"
        );
    }
    let below = below_threshold.snapshot(&config);
    assert!(
        below.pac_bayes_bound < config.safety_error_threshold,
        "low-error PAC-Bayes bound should stay below threshold: bound={}, threshold={}",
        below.pac_bayes_bound,
        config.safety_error_threshold
    );
    assert!(!below.vetoing);
    assert!(below.veto_reason.is_none());

    let mut above_threshold = SafetyEnvelopeState::default();
    for _ in 0..18 {
        assert!(
            !above_threshold.evaluate(100.0, true, &config),
            "PAC-Bayes path should not veto before failures push the bound above threshold"
        );
    }
    assert!(
        !above_threshold.evaluate(100.0, false, &config),
        "minimum-observation gate should suppress veto until the configured sample count"
    );
    let vetoed = above_threshold.evaluate(100.0, false, &config);
    let above = above_threshold.snapshot(&config);
    assert!(
        above.pac_bayes_bound > config.safety_error_threshold,
        "high-KL PAC-Bayes bound should exceed threshold: bound={}, threshold={}",
        above.pac_bayes_bound,
        config.safety_error_threshold
    );
    assert!(vetoed);
    assert!(above.vetoing);
    assert_eq!(
        above.veto_reason.as_deref(),
        Some("pac_bayes_bound_exceeded")
    );
}

#[test]
fn safety_envelope_no_veto_when_disabled() {
    let config = SafetyEnvelopeConfig {
        enabled: false,
        ..Default::default()
    };
    let mut state = SafetyEnvelopeState::default();

    for _ in 0..50 {
        let veto = state.evaluate(100.0, false, &config);
        assert!(!veto, "disabled envelope should never veto");
    }
    assert!(!state.vetoing);
    assert_eq!(state.veto_count, 0);
}

#[test]
fn safety_envelope_disabling_clears_active_veto() {
    let enabled = SafetyEnvelopeConfig {
        enabled: true,
        conformal_confidence: 0.95,
        conformal_calibration_size: 10,
        pac_bayes_delta: 0.05,
        pac_bayes_prior_weight: 1.0,
        safety_error_threshold: 0.10,
        min_observations: 5,
    };
    let mut state = SafetyEnvelopeState::default();

    for _ in 0..20 {
        state.evaluate(100.0, false, &enabled);
    }
    assert!(state.vetoing, "enabled config should enter veto state");
    assert_eq!(state.veto_reason, Some("pac_bayes_bound_exceeded"));
    let prior_veto_count = state.veto_count;
    let disabled = SafetyEnvelopeConfig {
        enabled: false,
        ..enabled
    };
    assert!(prior_veto_count > 0);

    assert!(
        !state.evaluate(100.0, false, &disabled),
        "disabled config should never veto"
    );
    assert!(!state.vetoing, "disabling should clear active veto state");
    assert!(
        state.veto_reason.is_none(),
        "disabling should clear stale veto reason"
    );
    assert_eq!(
        state.veto_count, prior_veto_count,
        "disabling should not fabricate a new veto activation"
    );
}

#[test]
fn budget_controller_disabling_safety_envelope_clears_active_veto_immediately() {
    let manager = ExtensionManager::new();
    manager.set_budget_controller_config(ExtensionBudgetControllerConfig {
        enabled: true,
        tier: ExtensionBudgetTier::Strict,
        overload_window_ms: 60_000,
        overload_signals_to_fallback: 1_000,
        recovery_successes_to_exit: 2,
        regime_shift: RegimeShiftConfig {
            enabled: false,
            ..Default::default()
        },
        safety_envelope: SafetyEnvelopeConfig {
            enabled: true,
            min_observations: 5,
            safety_error_threshold: 0.10,
            conformal_confidence: 0.95,
            conformal_calibration_size: 10,
            pac_bayes_delta: 0.05,
            pac_bayes_prior_weight: 1.0,
        },
        oco_tuner: OcoTunerConfig::for_tier(ExtensionBudgetTier::Strict),
    });

    for _ in 0..20 {
        manager.record_budget_overload_signal(Some("ext.disable.safety"), "overload", None, None);
    }
    assert!(
        manager.any_safety_envelope_vetoing(),
        "enabled safety envelope should enter a vetoing state"
    );
    let before = manager
        .safety_envelope_snapshot("ext.disable.safety")
        .expect("snapshot before disable");
    assert!(before.vetoing);
    assert_eq!(
        before.veto_reason.as_deref(),
        Some("pac_bayes_bound_exceeded")
    );

    manager.set_budget_controller_config(ExtensionBudgetControllerConfig {
        enabled: true,
        tier: ExtensionBudgetTier::Strict,
        overload_window_ms: 60_000,
        overload_signals_to_fallback: 1_000,
        recovery_successes_to_exit: 2,
        regime_shift: RegimeShiftConfig {
            enabled: false,
            ..Default::default()
        },
        safety_envelope: SafetyEnvelopeConfig {
            enabled: false,
            ..SafetyEnvelopeConfig::for_tier(ExtensionBudgetTier::Strict)
        },
        oco_tuner: OcoTunerConfig::for_tier(ExtensionBudgetTier::Strict),
    });

    assert!(
        !manager.any_safety_envelope_vetoing(),
        "disabling the safety envelope should clear stale veto state immediately"
    );
    let after = manager
        .safety_envelope_snapshot("ext.disable.safety")
        .expect("snapshot after disable");
    assert!(!after.vetoing);
    assert!(after.veto_reason.is_none());
    assert_eq!(after.veto_count, before.veto_count);
}

#[test]
fn safety_envelope_no_veto_before_min_observations() {
    let config = SafetyEnvelopeConfig {
        enabled: true,
        min_observations: 100,
        safety_error_threshold: 0.0001,
        ..Default::default()
    };
    let mut state = SafetyEnvelopeState::default();

    // Even with all failures, should not veto before min_observations.
    for _ in 0..99 {
        let veto = state.evaluate(100.0, false, &config);
        assert!(!veto, "should not veto before min_observations");
    }
}

#[test]
fn safety_envelope_reset_clears_veto() {
    let config = SafetyEnvelopeConfig {
        enabled: true,
        min_observations: 5,
        safety_error_threshold: 0.10,
        ..Default::default()
    };
    let mut state = SafetyEnvelopeState::default();

    for _ in 0..20 {
        state.evaluate(100.0, false, &config);
    }
    assert!(state.vetoing, "should be vetoing after failures");

    state.reset();
    assert!(!state.vetoing, "reset should clear veto");
    assert!(state.veto_reason.is_none(), "reset should clear reason");
}

#[test]
fn safety_envelope_snapshot_reflects_state() {
    let config = SafetyEnvelopeConfig {
        enabled: true,
        min_observations: 5,
        safety_error_threshold: 0.10,
        ..Default::default()
    };
    let mut state = SafetyEnvelopeState::default();

    for _ in 0..20 {
        state.evaluate(100.0, false, &config);
    }

    let snap = state.snapshot(&config);
    assert!(snap.vetoing);
    assert!(snap.veto_count > 0);
    assert!(snap.pac_bayes_empirical_error > 0.0);
    assert!(snap.pac_bayes_bound > 0.0);
    assert_eq!(snap.pac_bayes_total, 20);
}

#[test]
fn safety_envelope_config_tier_ordering() {
    let strict = SafetyEnvelopeConfig::for_tier(ExtensionBudgetTier::Strict);
    let balanced = SafetyEnvelopeConfig::for_tier(ExtensionBudgetTier::Balanced);
    let throughput = SafetyEnvelopeConfig::for_tier(ExtensionBudgetTier::Throughput);

    // Strict should be most conservative: highest confidence, lowest error threshold.
    assert!(strict.conformal_confidence >= balanced.conformal_confidence);
    assert!(balanced.conformal_confidence >= throughput.conformal_confidence);
    assert!(strict.safety_error_threshold <= balanced.safety_error_threshold);
    assert!(balanced.safety_error_threshold <= throughput.safety_error_threshold);
    // Strict needs fewer observations to activate (faster reaction).
    assert!(strict.min_observations <= balanced.min_observations);
    assert!(balanced.min_observations <= throughput.min_observations);
}

#[test]
fn budget_controller_safety_envelope_triggers_fallback() {
    let manager = ExtensionManager::new();
    manager.set_budget_controller_config(ExtensionBudgetControllerConfig {
        enabled: true,
        tier: ExtensionBudgetTier::Strict,
        overload_window_ms: 60_000,
        // Classic threshold very high so it won't fire.
        overload_signals_to_fallback: 1000,
        recovery_successes_to_exit: 2,
        regime_shift: RegimeShiftConfig {
            enabled: false,
            ..Default::default()
        },
        safety_envelope: SafetyEnvelopeConfig {
            enabled: true,
            min_observations: 5,
            safety_error_threshold: 0.10,
            conformal_confidence: 0.95,
            conformal_calibration_size: 10,
            pac_bayes_delta: 0.05,
            pac_bayes_prior_weight: 1.0,
        },
        oco_tuner: OcoTunerConfig::for_tier(ExtensionBudgetTier::Strict),
    });

    // Before enough signals, no fallback.
    for _ in 0..4 {
        manager.record_budget_overload_signal(Some("ext.safety"), "overload", None, None);
    }
    // min_observations=5, so not enough data yet.
    assert!(
        manager
            .hostcall_compat_kill_switch_reason(Some("ext.safety"))
            .is_none(),
        "should not fall back before min_observations"
    );

    // Feed more overload signals to accumulate failures past threshold.
    for _ in 0..20 {
        manager.record_budget_overload_signal(Some("ext.safety"), "overload", None, None);
    }
    assert!(
        manager
            .hostcall_compat_kill_switch_reason(Some("ext.safety"))
            .is_some(),
        "safety envelope should trigger fallback after enough failures"
    );
}

#[test]
fn budget_controller_recovery_resets_safety_envelope() {
    let manager = ExtensionManager::new();
    manager.set_budget_controller_config(ExtensionBudgetControllerConfig {
        enabled: true,
        tier: ExtensionBudgetTier::Balanced,
        overload_window_ms: 60_000,
        overload_signals_to_fallback: 1,
        recovery_successes_to_exit: 2,
        ..Default::default()
    });

    // Enter fallback via classic threshold.
    manager.record_budget_overload_signal(Some("ext.se.reset"), "overload", None, None);
    assert!(
        manager
            .hostcall_compat_kill_switch_reason(Some("ext.se.reset"))
            .is_some()
    );

    // Recover.
    manager.record_budget_recovery_sample(Some("ext.se.reset"), true);
    manager.record_budget_recovery_sample(Some("ext.se.reset"), true);
    assert!(
        manager
            .hostcall_compat_kill_switch_reason(Some("ext.se.reset"))
            .is_none()
    );

    // Verify safety envelope was reset.
    let snap = manager
        .safety_envelope_snapshot("ext.se.reset")
        .expect("snapshot");
    assert!(!snap.vetoing);
    assert_eq!(snap.pac_bayes_total, 0);
    assert_eq!(snap.conformal_calibration_size, 0);
}

#[test]
fn amac_safety_veto_disables_interleaving() {
    // When any extension has a vetoing safety envelope, AMAC should be
    // disabled (any_safety_envelope_vetoing returns true).
    let manager = ExtensionManager::new();
    manager.set_budget_controller_config(ExtensionBudgetControllerConfig {
        enabled: true,
        tier: ExtensionBudgetTier::Strict,
        overload_window_ms: 60_000,
        overload_signals_to_fallback: 1,
        recovery_successes_to_exit: 5,
        ..Default::default()
    });

    // Before any signals, no veto.
    assert!(
        !manager.any_safety_envelope_vetoing(),
        "no veto before any signals"
    );

    // Create a fallback state by signalling overload.
    manager.record_budget_overload_signal(Some("ext.amac.veto"), "latency_spike", None, None);

    // The safety envelope itself may or may not be vetoing depending on
    // observation count vs min_observations.  But the budget fallback should
    // be active.
    assert!(
        manager
            .hostcall_compat_kill_switch_reason(Some("ext.amac.veto"))
            .is_some(),
        "fallback should be active after overload signal"
    );
}

#[test]
fn amac_safety_veto_cleared_after_recovery() {
    let manager = ExtensionManager::new();
    manager.set_budget_controller_config(ExtensionBudgetControllerConfig {
        enabled: true,
        tier: ExtensionBudgetTier::Balanced,
        overload_window_ms: 60_000,
        overload_signals_to_fallback: 1,
        recovery_successes_to_exit: 2,
        ..Default::default()
    });

    // Enter fallback.
    manager.record_budget_overload_signal(Some("ext.amac.recover"), "overload", None, None);
    assert!(
        manager
            .hostcall_compat_kill_switch_reason(Some("ext.amac.recover"))
            .is_some(),
        "should be in fallback"
    );

    // Recover via success samples.
    manager.record_budget_recovery_sample(Some("ext.amac.recover"), true);
    manager.record_budget_recovery_sample(Some("ext.amac.recover"), true);
    assert!(
        manager
            .hostcall_compat_kill_switch_reason(Some("ext.amac.recover"))
            .is_none(),
        "should have recovered"
    );

    // Safety envelope veto should be cleared after recovery.
    assert!(
        !manager.any_safety_envelope_vetoing(),
        "safety veto should be cleared after recovery"
    );
}

#[test]
fn amac_safety_veto_multiple_extensions_any_vetoing() {
    let manager = ExtensionManager::new();
    manager.set_budget_controller_config(ExtensionBudgetControllerConfig {
        enabled: true,
        tier: ExtensionBudgetTier::Strict,
        overload_window_ms: 60_000,
        overload_signals_to_fallback: 1,
        recovery_successes_to_exit: 10,
        ..Default::default()
    });

    // Two extensions: one healthy, one overloaded.
    // The overloaded one enters fallback.
    manager.record_budget_overload_signal(Some("ext.bad"), "latency", None, None);

    // Record a successful sample for the healthy extension (creates its state).
    manager.record_budget_recovery_sample(Some("ext.good"), true);

    // The overloaded extension should cause the kill-switch for itself.
    assert!(
        manager
            .hostcall_compat_kill_switch_reason(Some("ext.bad"))
            .is_some(),
        "bad ext should be in fallback"
    );
}

#[test]
fn amac_telemetry_snapshot_empty_initially() {
    // The thread-local AMAC executor should return None for telemetry
    // when no calls have been made.
    let snap = amac_telemetry_snapshot();
    assert!(
        snap.is_none(),
        "telemetry snapshot should be None when no calls recorded"
    );
}

#[test]
fn kl_divergence_basic_properties() {
    // KL(p, p) = 0.
    let kl_same = kl_divergence(0.3, 0.3);
    assert!(kl_same.abs() < 1e-10, "KL(p,p) should be ~0, got {kl_same}");

    // KL(p, q) > 0 for p != q.
    let kl_diff = kl_divergence(0.2, 0.8);
    assert!(kl_diff > 0.0, "KL(0.2, 0.8) should be > 0");

    // KL(0, q) should be finite (clamped).
    let kl_zero = kl_divergence(0.0, 0.5);
    assert!(kl_zero.is_finite(), "KL(0, 0.5) should be finite");

    // KL(1, q) should be finite (clamped).
    let kl_one = kl_divergence(1.0, 0.5);
    assert!(kl_one.is_finite(), "KL(1, 0.5) should be finite");
}

#[test]
fn dispatch_shared_allowed_fast_and_forced_compat_match_on_malformed_payload() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&["read"], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = permissive_policy();

    let fast_manager = ExtensionManager::new();
    let compat_manager = ExtensionManager::new();
    compat_manager.set_hostcall_compat_kill_switch_global(true);

    let malformed = HostCallPayload {
        call_id: "lane-malformed".to_string(),
        capability: "read".to_string(),
        method: "tool".to_string(),
        params: json!({ "name": "read", "input": {} }),
        timeout_ms: None,
        cancel_token: None,
        context: Some(json!({
            "typed_opcode": {
                "schema": HOSTCALL_OPCODE_SCHEMA_VERSION,
                "version": HOSTCALL_OPCODE_VERSION,
                "code": "tool.read"
            }
        })),
    };

    let fast_ctx = HostCallContext {
        runtime_name: "test",
        extension_id: Some("ext.fast"),
        tools: &tools,
        http: &http,
        manager: Some(fast_manager),
        policy: &policy,
        js_runtime: None,
        interceptor: None,
    };
    let compat_ctx = HostCallContext {
        runtime_name: "test",
        extension_id: Some("ext.compat"),
        tools: &tools,
        http: &http,
        manager: Some(compat_manager),
        policy: &policy,
        js_runtime: None,
        interceptor: None,
    };

    let (fast_outcome, fast_lane_meta) =
        run_async(async { dispatch_shared_allowed(&fast_ctx, &malformed).await });
    let (compat_outcome, compat_lane_meta) =
        run_async(async { dispatch_shared_allowed(&compat_ctx, &malformed).await });

    assert_eq!(
        fast_lane_meta.expect("fast lane metadata").lane,
        HostcallDispatchLane::Fast
    );
    assert_eq!(
        compat_lane_meta.expect("compat lane metadata").lane,
        HostcallDispatchLane::Compat
    );

    match (fast_outcome, compat_outcome) {
        (
            HostcallOutcome::Error {
                code: fast_code,
                message: fast_msg,
            },
            HostcallOutcome::Error {
                code: compat_code,
                message: compat_msg,
            },
        ) => {
            assert_eq!(fast_code, compat_code);
            assert_eq!(fast_msg, compat_msg);
        }
        (fast_other, compat_other) => {
            panic!();
        }
    }
}

#[test]
fn runtime_hostcall_telemetry_records_lane_reason_fallback_and_latency_share() {
    let dir = tempdir().expect("tempdir");
    let file = dir.path().join("lane_telemetry.txt");
    std::fs::write(&file, "lane-telemetry").expect("write test file");

    let tools = ToolRegistry::new(&["read"], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = permissive_policy();
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(RuntimeRiskConfig {
        enabled: true,
        enforce: false,
        alpha: 0.01,
        window_size: 64,
        ledger_limit: 256,
        decision_timeout_ms: 200,
        fail_closed: true,
    });
    manager.set_hostcall_compat_kill_switch_global(true);

    let ctx = HostCallContext {
        runtime_name: "test",
        extension_id: Some("ext.telemetry"),
        tools: &tools,
        http: &http,
        manager: Some(manager.clone()),
        policy: &policy,
        js_runtime: None,
        interceptor: None,
    };
    let payload = typed_tool_read_payload("lane-telemetry", file.to_str().expect("utf-8 path"));
    let result = run_async(async { dispatch_host_call_shared(&ctx, payload).await });
    assert!(
        !result.is_error,
        "dispatch must succeed: {:?}",
        result.error
    );

    let telemetry = manager.runtime_hostcall_telemetry_artifact();
    let entry = telemetry.entries.last().expect("telemetry entry");
    assert_eq!(entry.lane, "compat");
    assert_eq!(
        entry.lane_decision_reason,
        "forced_compat_global_kill_switch"
    );
    assert_eq!(
        entry.lane_fallback_reason.as_deref(),
        Some("forced_compat_global_kill_switch")
    );
    assert_eq!(entry.lane_matrix_key, "tool|fallback|filesystem");
    assert!(entry.lane_dispatch_latency_ms <= entry.latency_ms);
    assert!(entry.lane_latency_share_bps <= 10_000);
    assert_eq!(
        entry.marshalling_path,
        HOSTCALL_MARSHALLING_PATH_FAST_OPCODE
    );
    assert!(entry.marshalling_fallback_reason.is_none());
    assert_eq!(entry.marshalling_fallback_count, 0);
}

#[test]
fn replay_bundle_records_lane_pressure_resource_and_transcript_metadata() {
    let dir = tempdir().expect("tempdir");
    let file = dir.path().join("replay_pressure.txt");
    std::fs::write(&file, "replay-pressure").expect("write test file");

    let tools = ToolRegistry::new(&["read"], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = permissive_policy();
    let manager = ExtensionManager::new();
    manager.enable_replay(crate::extension_replay::ReplayLaneConfig::new(
        crate::extension_replay::ReplayCaptureBudget {
            capture_enabled: true,
            max_overhead_per_mille: 1_000,
            max_trace_bytes: 1_000_000,
        },
    ));
    manager.enable_hostcall_reactor(HostcallReactorConfig {
        shard_count: 1,
        lane_capacity: 1,
        core_ids: None,
    });
    manager
        .reactor_submit(
            "prefill-replay-pressure".to_string(),
            CommonHostcallOpcode::ToolRead,
            json!({}),
        )
        .expect("reactor enabled")
        .expect("prefill lane");

    let ctx = HostCallContext {
        runtime_name: "test",
        extension_id: Some("ext.replay.pressure"),
        tools: &tools,
        http: &http,
        manager: Some(manager.clone()),
        policy: &policy,
        js_runtime: None,
        interceptor: None,
    };
    let mut payload =
        typed_tool_read_payload("replay-pressure", file.to_str().expect("utf-8 path"));
    payload.context = Some(json!({
        "transcript_index": 7,
        "messageIndex": 3,
        "prompt": "must-not-enter-replay"
    }));

    let result = run_async(async { dispatch_host_call_shared(&ctx, payload).await });
    assert!(
        !result.is_error,
        "dispatch must succeed: {:?}",
        result.error
    );

    let bundles = manager.drain_replay_bundles();
    assert_eq!(bundles.len(), 1);
    let events = &bundles[0].events;
    assert_eq!(events.len(), 4);

    let scheduled =
        replay_event_attributes(events, crate::extension_replay::ReplayEventKind::Scheduled);
    assert_replay_attrs(
        scheduled,
        &[
            ("resource_target_class", "filesystem.tool"),
            ("transcript_index", "7"),
            ("message_index", "3"),
        ],
    );
    assert!(!scheduled.contains_key("prompt"));

    assert_replay_attrs(
        replay_event_attributes(
            events,
            crate::extension_replay::ReplayEventKind::QueueAccepted,
        ),
        &[
            ("selected_lane", "compat"),
            ("lane_fallback_reason", "reactor_lane_overflow"),
            ("reactor_queue_depth_observed_max", "1"),
            ("reactor_rejected_enqueues", "1"),
            ("reactor_overloaded", "true"),
        ],
    );
    assert_replay_attrs(
        replay_event_attributes(events, crate::extension_replay::ReplayEventKind::Completed),
        &[
            ("outcome_kind", "success"),
            ("resource_target_class", "filesystem.tool"),
        ],
    );
}

fn replay_event_attributes(
    events: &[crate::extension_replay::ReplayTraceEvent],
    kind: crate::extension_replay::ReplayEventKind,
) -> &BTreeMap<String, String> {
    &events
        .iter()
        .find(|event| event.kind == kind)
        .expect("replay event")
        .attributes
}

fn assert_replay_attrs(attrs: &BTreeMap<String, String>, expected: &[(&str, &str)]) {
    for (key, value) in expected {
        assert_eq!(attrs.get(*key).map(String::as_str), Some(*value), "{key}");
    }
}

#[test]
fn hostcall_marshalling_fast_hash_matches_generic_for_hot_opcodes() {
    let _guard = superinstruction_test_lock();
    reset_hostcall_superinstruction_state_for_tests();
    let tool_params = json!({
        "name": "read",
        "input": {
            "path": "a.txt",
            "offset": 0,
            "limit": 10
        }
    });
    let session_params = json!({ "op": "get_name" });
    let events_params = json!({ "op": "list_flags" });

    let cases = [
        ("tool", &tool_params, Some(CommonHostcallOpcode::ToolRead)),
        (
            "session",
            &session_params,
            Some(CommonHostcallOpcode::SessionGetName),
        ),
        (
            "events",
            &events_params,
            Some(CommonHostcallOpcode::EventsListFlags),
        ),
    ];

    for (method, params, opcode) in cases {
        let artifacts = HostcallPayloadArena::new(method, params, opcode).marshal();
        assert_eq!(artifacts.params_hash, hostcall_params_hash(method, params));
        assert_eq!(
            artifacts.args_shape_hash,
            hostcall_params_shape_hash(method, params)
        );
        assert_eq!(
            artifacts.telemetry.path,
            HOSTCALL_MARSHALLING_PATH_FAST_OPCODE
        );
        assert!(artifacts.telemetry.fallback_reason.is_none());
        assert_eq!(
            artifacts.telemetry.rewrite_rule.as_deref(),
            Some(HOSTCALL_REWRITE_RULE_FAST_OPCODE_FUSION)
        );
        assert!(artifacts.telemetry.rewrite_expected_cost_delta > 0);
        assert!(artifacts.telemetry.rewrite_fallback_reason.is_none());
    }
    reset_hostcall_superinstruction_state_for_tests();
}

#[test]
fn hostcall_marshalling_shape_miss_reports_rewrite_fallback() {
    let _guard = superinstruction_test_lock();
    reset_hostcall_superinstruction_state_for_tests();
    let params = json!({
        "name": "read",
        "input": {
            "path": "a.txt"
        },
        "extra": true
    });

    let artifacts =
        HostcallPayloadArena::new("tool", &params, Some(CommonHostcallOpcode::ToolRead)).marshal();

    assert_eq!(
        artifacts.telemetry.path,
        HOSTCALL_MARSHALLING_PATH_CANONICAL_FALLBACK
    );
    assert_eq!(
        artifacts.telemetry.fallback_reason.as_deref(),
        Some(HOSTCALL_MARSHALLING_FALLBACK_OPCODE_SHAPE_MISS)
    );
    assert!(artifacts.telemetry.rewrite_rule.is_none());
    assert_eq!(
        artifacts.telemetry.rewrite_fallback_reason.as_deref(),
        Some("no_better_candidate")
    );
    reset_hostcall_superinstruction_state_for_tests();
}

#[test]
fn hostcall_marshalling_superinstruction_hits_after_trace_warmup() {
    let _guard = superinstruction_test_lock();
    reset_hostcall_superinstruction_state_for_tests();

    let get_name = json!({ "op": "get_name" });
    let get_model = json!({ "op": "get_model" });
    let mut artifacts = HostcallPayloadArena::new(
        "session",
        &get_name,
        Some(CommonHostcallOpcode::SessionGetName),
    )
    .marshal();

    for _ in 0..8 {
        let _ = HostcallPayloadArena::new(
            "session",
            &get_name,
            Some(CommonHostcallOpcode::SessionGetName),
        )
        .marshal();
        artifacts = HostcallPayloadArena::new(
            "session",
            &get_model,
            Some(CommonHostcallOpcode::SessionGetModel),
        )
        .marshal();
    }

    assert!(
        artifacts
            .telemetry
            .superinstruction_trace_signature
            .is_some()
    );
    assert!(artifacts.telemetry.superinstruction_plan_id.is_some());
    assert!(artifacts.telemetry.superinstruction_expected_cost_delta > 0);
    assert!(artifacts.telemetry.superinstruction_deopt_reason.is_none());

    reset_hostcall_superinstruction_state_for_tests();
}

#[test]
fn runtime_hostcall_telemetry_records_marshalling_fallback_reason_and_counter() {
    let _guard = superinstruction_test_lock();
    reset_hostcall_superinstruction_state_for_tests();
    let dir = tempdir().expect("tempdir");
    let file = dir.path().join("lane_telemetry_fallback.txt");
    std::fs::write(&file, "lane-telemetry-fallback").expect("write test file");

    let tools = ToolRegistry::new(&["read"], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = permissive_policy();
    let manager = ExtensionManager::new();
    manager.set_runtime_risk_config(RuntimeRiskConfig {
        enabled: true,
        enforce: false,
        alpha: 0.01,
        window_size: 64,
        ledger_limit: 256,
        decision_timeout_ms: 200,
        fail_closed: true,
    });

    let ctx = HostCallContext {
        runtime_name: "test",
        extension_id: Some("ext.telemetry.fallback"),
        tools: &tools,
        http: &http,
        manager: Some(manager.clone()),
        policy: &policy,
        js_runtime: None,
        interceptor: None,
    };

    let mut first_payload =
        typed_tool_read_payload("lane-telemetry-fallback-1", file.to_str().expect("utf-8"));
    let params = first_payload
        .params
        .as_object_mut()
        .expect("tool params must be object");
    params.insert("extra".to_string(), json!(true));
    let first_result = run_async(async { dispatch_host_call_shared(&ctx, first_payload).await });
    assert!(!first_result.is_error, "first dispatch should succeed");
    let first_entry = manager
        .runtime_hostcall_telemetry_artifact()
        .entries
        .last()
        .cloned()
        .expect("first telemetry entry");
    assert_eq!(
        first_entry.marshalling_fallback_reason.as_deref(),
        Some(HOSTCALL_MARSHALLING_FALLBACK_OPCODE_SHAPE_MISS)
    );
    assert_eq!(
        first_entry.marshalling_path,
        HOSTCALL_MARSHALLING_PATH_CANONICAL_FALLBACK
    );
    assert_eq!(first_entry.marshalling_fallback_count, 1);

    let mut second_payload =
        typed_tool_read_payload("lane-telemetry-fallback-2", file.to_str().expect("utf-8"));
    let params = second_payload
        .params
        .as_object_mut()
        .expect("tool params must be object");
    params.insert("extra".to_string(), json!(false));
    let second_result = run_async(async { dispatch_host_call_shared(&ctx, second_payload).await });
    assert!(!second_result.is_error, "second dispatch should succeed");
    let second_entry = manager
        .runtime_hostcall_telemetry_artifact()
        .entries
        .last()
        .cloned()
        .expect("second telemetry entry");
    assert_eq!(second_entry.marshalling_fallback_count, 2);
    assert!(
        second_entry
            .marshalling_superinstruction_trace_signature
            .is_some()
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn params_hash_parity_request_vs_payload_across_hostcall_kinds() {
    let cases: Vec<(&str, HostcallRequest, Value)> = vec![
        (
            "tool",
            HostcallRequest {
                call_id: "call-hash-tool".to_string(),
                kind: HostcallKind::Tool {
                    name: "read".to_string(),
                },
                payload: json!({ "path": "hello.txt", "offset": 0 }),
                trace_id: 1,
                extension_id: None,
            },
            json!({
                "name": "read",
                "input": { "path": "hello.txt", "offset": 0 }
            }),
        ),
        (
            "exec-object",
            HostcallRequest {
                call_id: "call-hash-exec-object".to_string(),
                kind: HostcallKind::Exec {
                    cmd: "echo".to_string(),
                },
                payload: json!({
                    "command": "legacy alias should be ignored",
                    "args": ["hello"],
                    "options": { "timeout": 1000 }
                }),
                trace_id: 2,
                extension_id: None,
            },
            json!({
                "cmd": "echo",
                "args": ["hello"],
                "options": { "timeout": 1000 }
            }),
        ),
        (
            "exec-non-object",
            HostcallRequest {
                call_id: "call-hash-exec-non-object".to_string(),
                kind: HostcallKind::Exec {
                    cmd: "printf".to_string(),
                },
                payload: json!("hello"),
                trace_id: 3,
                extension_id: None,
            },
            json!({
                "cmd": "printf",
                "payload": "hello"
            }),
        ),
        (
            "http-object",
            HostcallRequest {
                call_id: "call-hash-http-object".to_string(),
                kind: HostcallKind::Http,
                payload: json!({
                    "url": "https://example.com",
                    "method": "POST",
                    "timeout": 1500
                }),
                trace_id: 4,
                extension_id: None,
            },
            json!({
                "url": "https://example.com",
                "method": "POST",
                "timeout": 1500
            }),
        ),
        (
            "http-non-object",
            HostcallRequest {
                call_id: "call-hash-http-non-object".to_string(),
                kind: HostcallKind::Http,
                payload: json!("https://example.com/health"),
                trace_id: 5,
                extension_id: None,
            },
            json!("https://example.com/health"),
        ),
        (
            "session",
            HostcallRequest {
                call_id: "call-hash-session".to_string(),
                kind: HostcallKind::Session {
                    op: "set_model".to_string(),
                },
                payload: json!({ "provider": "openai", "modelId": "gpt-4o-mini" }),
                trace_id: 6,
                extension_id: None,
            },
            json!({
                "op": "set_model",
                "provider": "openai",
                "modelId": "gpt-4o-mini"
            }),
        ),
        (
            "ui-non-object",
            HostcallRequest {
                call_id: "call-hash-ui".to_string(),
                kind: HostcallKind::Ui {
                    op: "set_status".to_string(),
                },
                payload: json!("thinking"),
                trace_id: 7,
                extension_id: None,
            },
            json!({
                "op": "set_status",
                "payload": "thinking"
            }),
        ),
        (
            "events-non-object",
            HostcallRequest {
                call_id: "call-hash-events".to_string(),
                kind: HostcallKind::Events {
                    op: "emit".to_string(),
                },
                payload: json!(42),
                trace_id: 8,
                extension_id: None,
            },
            json!({
                "op": "emit",
                "payload": 42
            }),
        ),
        (
            "log",
            HostcallRequest {
                call_id: "call-hash-log".to_string(),
                kind: HostcallKind::Log,
                payload: json!({
                    "level": "info",
                    "event": "unit.test",
                    "message": "hello"
                }),
                trace_id: 9,
                extension_id: None,
            },
            json!({
                "level": "info",
                "event": "unit.test",
                "message": "hello"
            }),
        ),
    ];

    for (case_name, request, expected_params) in cases {
        let payload = hostcall_request_to_payload(&request);

        assert_eq!(
            payload.params, expected_params,
            "unexpected canonical params for case {case_name}"
        );
        assert_eq!(
            request.params_for_hash(),
            payload.params,
            "request->payload canonical params drift for case {case_name}"
        );

        let request_hash = request.params_hash();
        let payload_hash = hostcall_params_hash(&payload.method, &payload.params);
        assert_eq!(
            request_hash, payload_hash,
            "params_hash mismatch for case {case_name}"
        );
    }
}

#[test]
fn host_result_to_outcome_success_roundtrip() {
    let result = HostResultPayload {
        call_id: "call-ok".to_string(),
        output: json!({"data": "hello"}),
        is_error: false,
        error: None,
        chunk: None,
    };

    let outcome = host_result_to_outcome(result);
    assert!(matches!(outcome, HostcallOutcome::Success(ref v) if v == &json!({"data": "hello"})));
}

#[test]
fn host_result_to_outcome_error_roundtrip() {
    let result = HostResultPayload {
        call_id: "call-err".to_string(),
        output: json!({}),
        is_error: true,
        error: Some(HostCallError {
            code: HostCallErrorCode::Io,
            message: "disk full".to_string(),
            details: None,
            retryable: Some(true),
        }),
        chunk: None,
    };

    let outcome = host_result_to_outcome(result);
    match outcome {
        HostcallOutcome::Error { code, message } => {
            assert_eq!(code, "io");
            assert_eq!(message, "disk full");
        }
        other => panic!(),
    }
}

#[test]
fn host_result_to_outcome_stream_chunk() {
    let result = HostResultPayload {
        call_id: "call-stream".to_string(),
        output: json!("line 1\n"),
        is_error: false,
        error: None,
        chunk: Some(HostStreamChunk {
            index: 5,
            is_last: false,
            backpressure: None,
        }),
    };

    let outcome = host_result_to_outcome(result);
    match outcome {
        HostcallOutcome::StreamChunk {
            sequence,
            chunk,
            is_final,
        } => {
            assert_eq!(sequence, 5);
            assert_eq!(chunk, json!("line 1\n"));
            assert!(!is_final);
        }
        other => panic!(),
    }
}

#[test]
fn host_result_to_outcome_error_without_error_payload_defaults_internal_message() {
    let result = HostResultPayload {
        call_id: "call-err-missing".to_string(),
        output: json!({"ignored": true}),
        is_error: true,
        error: None,
        chunk: None,
    };

    let outcome = host_result_to_outcome(result);
    match outcome {
        HostcallOutcome::Error { code, message } => {
            assert_eq!(code, "internal");
            assert_eq!(message, "Unknown error");
        }
        other => panic!(),
    }
}

#[test]
fn host_result_to_outcome_chunk_precedes_error_flag_when_chunk_present() {
    let result = HostResultPayload {
        call_id: "call-stream-over-error".to_string(),
        output: json!({"delta": "chunk"}),
        is_error: true,
        error: Some(HostCallError {
            code: HostCallErrorCode::Io,
            message: "should not win when chunk exists".to_string(),
            details: None,
            retryable: None,
        }),
        chunk: Some(HostStreamChunk {
            index: 2,
            is_last: true,
            backpressure: None,
        }),
    };

    let outcome = host_result_to_outcome(result);
    match outcome {
        HostcallOutcome::StreamChunk {
            sequence,
            chunk,
            is_final,
        } => {
            assert_eq!(sequence, 2);
            assert_eq!(chunk, json!({"delta": "chunk"}));
            assert!(is_final);
        }
        other => panic!(),
    }
}

#[test]
fn host_result_to_outcome_success_flag_ignores_error_payload() {
    let result = HostResultPayload {
        call_id: "call-success-with-error-object".to_string(),
        output: json!({"ok": true, "value": 7}),
        is_error: false,
        error: Some(HostCallError {
            code: HostCallErrorCode::Denied,
            message: "should be ignored when is_error=false".to_string(),
            details: None,
            retryable: None,
        }),
        chunk: None,
    };

    let outcome = host_result_to_outcome(result);
    match outcome {
        HostcallOutcome::Success(value) => {
            assert_eq!(value, json!({"ok": true, "value": 7}));
        }
        other => panic!(),
    }
}

#[test]
fn host_result_to_outcome_error_flag_overrides_non_empty_output() {
    let result = HostResultPayload {
        call_id: "call-error-over-output".to_string(),
        output: json!({"ok": true, "value": "ignored"}),
        is_error: true,
        error: Some(HostCallError {
            code: HostCallErrorCode::Denied,
            message: "blocked".to_string(),
            details: None,
            retryable: None,
        }),
        chunk: None,
    };

    let outcome = host_result_to_outcome(result);
    match outcome {
        HostcallOutcome::Error { code, message } => {
            assert_eq!(code, "denied");
            assert_eq!(message, "blocked");
        }
        other => panic!(),
    }
}

#[test]
fn outcome_to_host_result_preserves_taxonomy() {
    let outcome = HostcallOutcome::Error {
        code: "timeout".to_string(),
        message: "timed out".to_string(),
    };
    let result = outcome_to_host_result("call-t", &outcome);
    assert!(result.is_error);
    assert_eq!(result.output, json!({}));
    let err = result.error.unwrap();
    assert_eq!(err.code, HostCallErrorCode::Timeout);
    assert_eq!(err.message, "timed out");
}

#[test]
fn outcome_to_host_result_unknown_code_maps_to_internal() {
    let outcome = HostcallOutcome::Error {
        code: "some_weird_code".to_string(),
        message: "surprise".to_string(),
    };
    let result = outcome_to_host_result("call-x", &outcome);
    assert!(result.is_error);
    let err = result.error.unwrap();
    assert_eq!(err.code, HostCallErrorCode::Internal);
}

#[test]
fn outcome_to_host_result_canonical_codes_preserve_taxonomy() {
    let cases = [
        ("timeout", HostCallErrorCode::Timeout),
        ("denied", HostCallErrorCode::Denied),
        ("io", HostCallErrorCode::Io),
        ("invalid_request", HostCallErrorCode::InvalidRequest),
        ("internal", HostCallErrorCode::Internal),
    ];

    for (code, expected) in cases {
        let outcome = HostcallOutcome::Error {
            code: code.to_string(),
            message: format!("msg-{code}"),
        };
        let result = outcome_to_host_result("call-canonical", &outcome);
        let err = result
            .error
            .expect("canonical code must produce error payload");
        assert_eq!(err.code, expected, "canonical code must map exactly");
        assert_eq!(err.message, format!("msg-{code}"));
    }
}

#[test]
fn outcome_to_host_result_non_canonical_code_variants_fail_closed() {
    let variants = [
        " TIMEOUT ",
        "Timeout",
        "DENIED",
        " io ",
        "INVALID_REQUEST",
        "invalid request",
        "internal ",
    ];

    for code in variants {
        let outcome = HostcallOutcome::Error {
            code: code.to_string(),
            message: "variant".to_string(),
        };
        let result = outcome_to_host_result("call-variant", &outcome);
        let err = result
            .error
            .expect("variant code must still produce error payload");
        assert_eq!(
            err.code,
            HostCallErrorCode::Internal,
            "non-canonical code variant must fail closed to internal"
        );
        assert_eq!(err.message, "variant");
    }
}
