//! Budget, lifecycle, cancellation, and property-based dispatch tests.

use super::*;

// ========================================================================
// Budget / structured concurrency tests (bd-2vie)
// ========================================================================

#[test]
fn extension_manager_default_budget_is_infinite() {
    let manager = ExtensionManager::new();
    let budget = manager.budget();
    assert!(budget.deadline.is_none());
    assert_eq!(budget.poll_quota, u32::MAX);
    assert!(budget.cost_quota.is_none());
}

#[test]
fn extension_manager_with_budget_stores_it() {
    let budget = Budget::with_deadline_at_secs(30);
    let manager = ExtensionManager::with_budget(budget);
    let stored = manager.budget();
    assert!(stored.deadline.is_some());
}

#[test]
fn extension_manager_set_budget_updates() {
    let manager = ExtensionManager::new();
    assert!(manager.budget().deadline.is_none());

    manager.set_budget(Budget::with_deadline_at_secs(10));
    assert!(manager.budget().deadline.is_some());
}

#[test]
fn extension_cx_returns_unbounded_by_default() {
    let manager = ExtensionManager::new();
    let cx = manager.extension_cx();
    // Default budget is infinite, so Cx should be unbounded.
    assert!(cx.budget().deadline.is_none());
}

#[test]
fn extension_cx_applies_configured_budget() {
    let manager = ExtensionManager::with_budget(Budget::with_deadline_at_secs(30));
    let cx = manager.extension_cx();
    assert!(cx.budget().deadline.is_some());
}

#[test]
fn effective_timeout_no_budget_returns_operation_timeout() {
    let manager = ExtensionManager::new();
    assert_eq!(manager.effective_timeout(5_000), 5_000);
    assert_eq!(manager.effective_timeout(30_000), 30_000);
}

#[test]
fn effective_timeout_with_tight_budget_caps_timeout() {
    // Set a budget that expires 2 seconds from now.
    let budget = Budget {
        deadline: Some(wall_now() + Duration::from_secs(2)),
        ..Budget::INFINITE
    };
    let manager = ExtensionManager::with_budget(budget);
    // A 30s operation timeout should be capped to ~2s.
    let effective = manager.effective_timeout(30_000);
    assert!(effective <= 2_100, "expected <=2100ms, got {effective}");
    assert!(effective >= 1_000, "expected >=1000ms, got {effective}");
}

#[test]
fn effective_timeout_with_expired_budget_returns_zero() {
    // Set a budget with a deadline in the past.
    let budget = Budget {
        deadline: Some(wall_now()),
        ..Budget::INFINITE
    };
    let manager = ExtensionManager::with_budget(budget);
    // Should return 0 (or close to it) since the deadline has passed.
    let effective = manager.effective_timeout(30_000);
    assert!(effective <= 1, "expected ~0ms, got {effective}");
}

#[test]
fn effective_timeout_takes_min_of_budget_and_operation() {
    // Budget with a far-off deadline (60s) — operation timeout (5s) wins.
    let budget = Budget {
        deadline: Some(wall_now() + Duration::from_secs(60)),
        ..Budget::INFINITE
    };
    let manager = ExtensionManager::with_budget(budget);
    let effective = manager.effective_timeout(5_000);
    assert_eq!(effective, 5_000);
}

#[test]
fn extension_manager_shutdown_without_runtime_is_noop() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let ok = manager.shutdown(Duration::from_secs(1)).await;
        assert!(ok, "shutdown without runtime should succeed");
    });
}

// ========================================================================
// LabRuntime deterministic testing (bd-48tv)
// ========================================================================

mod lab_runtime_tests {
    use super::*;
    use asupersync::{LabConfig, LabRuntime};
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Create a LabRuntime configured for extension testing.
    fn ext_lab(seed: u64) -> LabRuntime {
        LabRuntime::new(LabConfig::new(seed).trace_capacity(4096))
    }

    #[test]
    fn lab_oneshot_recv_completes_under_virtual_time() {
        let mut runtime = ext_lab(42);
        let root = runtime.state.create_root_region(Budget::INFINITE);

        let (tx, mut rx) = oneshot::channel::<String>();
        let received = Arc::new(std::sync::Mutex::new(None));
        let received_clone = received.clone();

        // Sender task: send a value immediately.
        let (send_task, _) = runtime
            .state
            .create_task(root, Budget::INFINITE, async move {
                let cx = Cx::current().expect("cx");
                tx.send(&cx, "hello".to_string()).expect("send");
            })
            .expect("create send task");
        runtime.scheduler.lock().schedule(send_task, 0);

        // Receiver task: receive with infinite budget.
        let (recv_task, _) = runtime
            .state
            .create_task(root, Budget::INFINITE, async move {
                let cx = Cx::current().expect("cx");
                if let Ok(val) = rx.recv(&cx).await {
                    *received_clone
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(val);
                }
            })
            .expect("create recv task");
        runtime.scheduler.lock().schedule(recv_task, 0);

        runtime.run_until_quiescent();

        let val = received
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        assert_eq!(val.as_deref(), Some("hello"));
    }

    #[test]
    fn lab_sender_drop_unblocks_receiver() {
        // Simulates extension runtime shutdown: when the JS runtime
        // thread exits, it drops the reply sender. The ExtensionManager
        // method (receiver) should see an error, not hang.
        let mut runtime = ext_lab(0xDEAD);
        let root = runtime.state.create_root_region(Budget::INFINITE);

        let (tx, mut rx) = oneshot::channel::<String>();
        let got_error = Arc::new(AtomicBool::new(false));
        let got_error_clone = got_error.clone();

        // Task 1: drop the sender (simulates runtime exit).
        let (drop_task, _) = runtime
            .state
            .create_task(root, Budget::INFINITE, async move {
                drop(tx);
            })
            .expect("create drop task");
        runtime.scheduler.lock().schedule(drop_task, 0);

        // Task 2: try to recv (should fail because sender was dropped).
        let (recv_task, _) = runtime
            .state
            .create_task(root, Budget::INFINITE, async move {
                let cx = Cx::current().expect("cx");
                if rx.recv(&cx).await.is_err() {
                    got_error_clone.store(true, Ordering::SeqCst);
                }
            })
            .expect("create recv task");
        runtime.scheduler.lock().schedule(recv_task, 0);

        runtime.run_until_quiescent();

        assert!(
            got_error.load(Ordering::SeqCst),
            "recv should fail when sender is dropped (runtime shutdown)"
        );
    }

    #[test]
    fn lab_extension_dispatch_deterministic_across_runs() {
        // Running the same scenario with the same seed must produce
        // identical results — verifying deterministic scheduling.
        fn run_once(seed: u64) -> Vec<String> {
            let mut runtime = ext_lab(seed);
            let root = runtime.state.create_root_region(Budget::INFINITE);

            let log = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));

            for i in 0..5 {
                let log = Arc::clone(&log);
                let (task_id, _) = runtime
                    .state
                    .create_task(root, Budget::INFINITE, async move {
                        let cx = Cx::current().expect("cx");
                        // Simulate extension dispatch: send/recv on a channel.
                        let (tx, mut rx) = oneshot::channel::<u32>();
                        tx.send(&cx, i).expect("send");
                        let val = rx.recv(&cx).await.expect("recv");
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("task-{val}"));
                    })
                    .expect("create task");
                runtime.scheduler.lock().schedule(task_id, 0);
            }

            runtime.run_until_quiescent();
            log.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        let run_a = run_once(0xCAFE);
        let run_b = run_once(0xCAFE);
        assert_eq!(run_a, run_b, "same seed must produce same execution order");
    }

    #[test]
    fn lab_multiworker_extension_dispatch_deterministic() {
        // Under multi-worker scheduling, same seed must still produce
        // deterministic results.
        fn run_multi(seed: u64) -> Vec<String> {
            let config = LabConfig::new(seed).worker_count(4).trace_capacity(4096);
            let mut runtime = LabRuntime::new(config);
            let root = runtime.state.create_root_region(Budget::INFINITE);

            let log = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));

            for i in 0..8 {
                let log = Arc::clone(&log);
                let (task_id, _) = runtime
                    .state
                    .create_task(root, Budget::INFINITE, async move {
                        // Yield to interleave with other tasks.
                        asupersync::runtime::yield_now().await;
                        log.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("w-{i}"));
                    })
                    .expect("create task");
                runtime.scheduler.lock().schedule(task_id, 0);
            }

            runtime.run_until_quiescent();
            log.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        let run_a = run_multi(0xF00D);
        let run_b = run_multi(0xF00D);
        assert_eq!(
            run_a, run_b,
            "multi-worker execution must be deterministic with same seed"
        );
    }
}

// ========================================================================
// Extension lifecycle / structured concurrency tests (bd-2vie)
// ========================================================================

mod lifecycle {
    use super::*;

    #[test]
    fn region_shutdown_returns_true_when_no_runtime() {
        asupersync::test_utils::run_test(|| async {
            let manager = ExtensionManager::new();
            let region = ExtensionRegion::new(manager);
            let ok = region.shutdown().await;
            assert!(ok, "shutdown should succeed when no JS runtime is running");
        });
    }

    #[test]
    fn region_shutdown_is_idempotent() {
        asupersync::test_utils::run_test(|| async {
            let manager = ExtensionManager::new();
            let region = ExtensionRegion::new(manager);
            assert!(region.shutdown().await);
            assert!(region.shutdown().await, "second shutdown should be no-op");
            assert!(region.shutdown().await, "third shutdown should be no-op");
        });
    }

    #[test]
    fn manager_shutdown_clears_js_runtime_handle() {
        asupersync::test_utils::run_test(|| async {
            let manager = ExtensionManager::new();
            let tools = Arc::new(crate::tools::ToolRegistry::new(
                &[],
                Path::new("/tmp"),
                None,
            ));

            let runtime = JsExtensionRuntimeHandle::start(
                PiJsRuntimeConfig {
                    cwd: "/tmp".to_string(),
                    ..Default::default()
                },
                Arc::clone(&tools),
                manager.clone(),
            )
            .await
            .expect("start js runtime");
            manager.set_js_runtime(runtime);

            assert!(
                manager.js_runtime().is_some(),
                "runtime should be set before shutdown"
            );

            let ok = manager.shutdown(Duration::from_secs(5)).await;
            assert!(ok, "shutdown should succeed");
            assert!(
                manager.js_runtime().is_none(),
                "runtime should be cleared after shutdown"
            );
        });
    }

    #[test]
    fn runtime_shutdown_treats_closed_exit_signal_as_success() {
        asupersync::test_utils::run_test(|| async {
            let (sender, _rx) = mpsc::channel(1);
            let (exit_tx, exit_rx) = oneshot::channel::<()>();
            drop(exit_tx);

            let runtime = JsExtensionRuntimeHandle {
                sender,
                compat_scan_mode: false,
                exit_signal: Arc::new(Mutex::new(Some(exit_rx))),
            };

            let ok = runtime.shutdown(Duration::from_secs(1)).await;
            assert!(
                ok,
                "closed exit signal means runtime is already gone; shutdown should succeed"
            );
        });
    }

    #[test]
    fn region_with_runtime_shuts_down_cleanly() {
        asupersync::test_utils::run_test(|| async {
            let manager = ExtensionManager::new();
            let tools = Arc::new(crate::tools::ToolRegistry::new(
                &[],
                Path::new("/tmp"),
                None,
            ));

            let runtime = JsExtensionRuntimeHandle::start(
                PiJsRuntimeConfig {
                    cwd: "/tmp".to_string(),
                    ..Default::default()
                },
                Arc::clone(&tools),
                manager.clone(),
            )
            .await
            .expect("start js runtime");
            manager.set_js_runtime(runtime);

            let region = ExtensionRegion::new(manager);
            let ok = region.shutdown().await;
            assert!(ok, "region shutdown with active runtime should succeed");
            assert!(
                region.manager().js_runtime().is_none(),
                "runtime should be cleared after region shutdown"
            );
        });
    }

    #[test]
    fn region_with_custom_budget() {
        asupersync::test_utils::run_test(|| async {
            let manager = ExtensionManager::new();
            let region = ExtensionRegion::with_budget(manager, Duration::from_millis(100));
            assert!(region.shutdown().await);
        });
    }

    #[test]
    fn region_drop_after_explicit_shutdown_is_silent() {
        asupersync::test_utils::run_test(|| async {
            let manager = ExtensionManager::new();
            let tools = Arc::new(crate::tools::ToolRegistry::new(
                &[],
                Path::new("/tmp"),
                None,
            ));

            let runtime = JsExtensionRuntimeHandle::start(
                PiJsRuntimeConfig {
                    cwd: "/tmp".to_string(),
                    ..Default::default()
                },
                Arc::clone(&tools),
                manager.clone(),
            )
            .await
            .expect("start js runtime");
            manager.set_js_runtime(runtime);

            let region = ExtensionRegion::new(manager);
            region.shutdown().await;
            // Drop should be silent (no warning) since shutdown was called.
            drop(region);
        });
    }

    #[test]
    fn region_into_inner_prevents_drop_shutdown() {
        asupersync::test_utils::run_test(|| async {
            let manager = ExtensionManager::new();
            let region = ExtensionRegion::new(manager);
            let _manager = region.into_inner();
            // into_inner marks shutdown_done=true, so drop is silent.
        });
    }

    #[test]
    fn weak_ref_breaks_arc_cycle() {
        asupersync::test_utils::run_test(|| async {
            let manager = ExtensionManager::new();
            let weak = Arc::downgrade(&manager.inner);
            let tools = Arc::new(crate::tools::ToolRegistry::new(
                &[],
                Path::new("/tmp"),
                None,
            ));

            let runtime = JsExtensionRuntimeHandle::start(
                PiJsRuntimeConfig {
                    cwd: "/tmp".to_string(),
                    ..Default::default()
                },
                Arc::clone(&tools),
                manager.clone(),
            )
            .await
            .expect("start js runtime");
            manager.set_js_runtime(runtime.clone());

            // Shut down the runtime so the thread exits
            // and drops its host (which held a Weak, not Arc).
            let ok = runtime.shutdown(Duration::from_secs(5)).await;
            assert!(ok, "shutdown should succeed");

            // Give the thread a moment to fully exit.
            asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(50)).await;

            // Now drop the manager — Arc should be the only strong ref.
            drop(manager);
            assert!(
                weak.upgrade().is_none(),
                "After shutdown + drop, the inner Arc should be deallocated \
                     (Weak breaks the cycle)"
            );
        });
    }

    #[test]
    fn runtime_processes_commands_before_shutdown() {
        asupersync::test_utils::run_test(|| async {
            let manager = ExtensionManager::new();
            let tools = Arc::new(crate::tools::ToolRegistry::new(
                &[],
                Path::new("/tmp"),
                None,
            ));

            let runtime = JsExtensionRuntimeHandle::start(
                PiJsRuntimeConfig {
                    cwd: "/tmp".to_string(),
                    ..Default::default()
                },
                Arc::clone(&tools),
                manager.clone(),
            )
            .await
            .expect("start js runtime");
            manager.set_js_runtime(runtime.clone());

            // Send a command (get_registered_tools) that the runtime
            // thread must process before we shut down.
            let tool_defs = runtime.get_registered_tools().await;
            assert!(
                tool_defs.is_ok(),
                "get_registered_tools should succeed on a fresh runtime"
            );
            assert!(tool_defs.unwrap().is_empty(), "no tools registered yet");

            // Pump the runtime to verify it's responsive.
            let pump = runtime.pump_once().await;
            assert!(pump.is_ok(), "pump_once should succeed");

            // Now shut down.
            let ok = runtime.shutdown(Duration::from_secs(5)).await;
            assert!(ok, "shutdown after command processing should succeed");
        });
    }
}

// ========================================================================
// Cancellation budget tests (bd-1yr1)
// ========================================================================

mod budget_tests {
    use super::*;
    use asupersync::channel::oneshot;

    #[test]
    fn cx_with_deadline_has_finite_budget() {
        asupersync::test_utils::run_test(|| async {
            let before = wall_now();
            let cx = cx_with_deadline(500);
            let budget = cx.budget();
            assert!(
                budget.deadline.is_some(),
                "cx_with_deadline should set a deadline"
            );
            let deadline = budget.deadline.unwrap();
            let expected = before + Duration::from_millis(500);
            // Deadline should be within 100ms of expected (accounting for wall clock drift).
            let delta_ns = if deadline >= expected {
                deadline.duration_since(expected)
            } else {
                expected.duration_since(deadline)
            };
            assert!(
                u128::from(delta_ns) <= Duration::from_millis(100).as_nanos(),
                "deadline {deadline:?} should be ~500ms after {before:?}"
            );
        });
    }

    #[test]
    fn budget_constants_are_reasonable() {
        const _: () = {
            assert!(EXTENSION_EVENT_TIMEOUT_MS >= 1_000);
            assert!(EXTENSION_EVENT_TIMEOUT_MS <= 60_000);
            assert!(EXTENSION_TOOL_BUDGET_MS >= 5_000);
            assert!(EXTENSION_TOOL_BUDGET_MS <= 300_000);
            assert!(EXTENSION_COMMAND_BUDGET_MS >= 5_000);
            assert!(EXTENSION_SHORTCUT_BUDGET_MS >= 5_000);
            assert!(EXTENSION_UI_BUDGET_MS >= 100);
            assert!(EXTENSION_UI_BUDGET_MS <= 10_000);
            assert!(EXTENSION_PROVIDER_BUDGET_MS >= 30_000);
            assert!(EXTENSION_QUERY_BUDGET_MS >= 1_000);
            assert!(EXTENSION_LOAD_BUDGET_MS >= 10_000);
        };
    }

    #[test]
    fn tight_deadline_cancels_blocked_recv() {
        asupersync::test_utils::run_test(|| async {
            // Create a oneshot where nobody will send.
            let (_tx, mut rx) = oneshot::channel::<()>();
            let cx = cx_with_deadline(50); // 50ms deadline
            let start = wall_now();
            let result = timeout(wall_now(), Duration::from_millis(50), rx.recv(&cx)).await;
            let elapsed = Duration::from_nanos(wall_now().duration_since(start));
            assert!(
                result.is_err() || matches!(result, Ok(Err(_))),
                "recv should fail when the deadline is exceeded; got: {result:?}"
            );
            // Should not hang forever.
            assert!(
                elapsed < Duration::from_secs(1),
                "recv should be cancelled quickly, took {elapsed:?}"
            );
        });
    }

    #[test]
    fn tight_deadline_cancels_runtime_send() {
        asupersync::test_utils::run_test(|| async {
            let manager = ExtensionManager::new();
            let tools = Arc::new(crate::tools::ToolRegistry::new(
                &[],
                Path::new("/tmp"),
                None,
            ));

            let runtime = JsExtensionRuntimeHandle::start(
                PiJsRuntimeConfig {
                    cwd: "/tmp".to_string(),
                    ..Default::default()
                },
                Arc::clone(&tools),
                manager.clone(),
            )
            .await
            .expect("start js runtime");
            manager.set_js_runtime(runtime.clone());

            // Shut down the runtime first so channels close.
            runtime.shutdown(Duration::from_secs(2)).await;

            // Now try get_registered_tools — the send should fail
            // because the channel is closed, regardless of budget.
            let result = runtime.get_registered_tools().await;
            assert!(result.is_err(), "send to shut-down runtime should fail");
        });
    }
}

fn coalescer_test_manager_with_hooks(hooks: &[ExtensionEventName]) -> ExtensionManager {
    let manager = ExtensionManager::new();
    manager.register(RegisterPayload {
        name: "event-coalescer-characterization".to_string(),
        version: "1.0.0".to_string(),
        api_version: PROTOCOL_VERSION.to_string(),
        capabilities: Vec::new(),
        capability_manifest: None,
        tools: Vec::new(),
        slash_commands: Vec::new(),
        shortcuts: Vec::new(),
        flags: Vec::new(),
        event_hooks: hooks.iter().map(ToString::to_string).collect(),
    });
    manager
}

fn coalescer_recording_payload(
    sequence: usize,
    resolved: &Arc<Mutex<Vec<usize>>>,
) -> CoalescedPayload {
    let resolved = Arc::clone(resolved);
    CoalescedPayload::Lazy(Box::new(move || {
        resolved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(sequence);
        Some(json!({ "sequence": sequence }))
    }))
}

fn assert_coalescer_idle(coalescer: &EventCoalescer) {
    assert!(
        coalescer
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty(),
        "completed dispatch must not leave a pending payload"
    );
    assert!(
        coalescer
            .in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty(),
        "completed dispatch must release its in-flight marker"
    );
}

#[test]
fn event_coalescer_characterization_replacement_keeps_first_and_latest_payload() {
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("runtime build");
    let handle = runtime.handle();
    let manager = coalescer_test_manager_with_hooks(&[ExtensionEventName::MessageUpdate]);
    let coalescer = EventCoalescer::new(manager);
    let resolved = Arc::new(Mutex::new(Vec::new()));

    for sequence in 1..=3 {
        coalescer.dispatch_fire_and_forget(
            ExtensionEventName::MessageUpdate,
            coalescer_recording_payload(sequence, &resolved),
            &handle,
        );
    }

    assert!(
        resolved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty(),
        "lazy payloads must not resolve before the runtime drives the task"
    );
    assert_eq!(
        coalescer
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        1,
        "only the latest replacement may remain pending"
    );

    runtime.block_on(async {
        for _ in 0..256 {
            if coalescer
                .in_flight
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
            {
                return;
            }
            asupersync::runtime::yield_now().await;
        }
        panic!("coalesced dispatch did not become idle");
    });

    assert_eq!(
        *resolved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        vec![1, 3],
        "the superseded middle payload must never be resolved"
    );
    assert_coalescer_idle(&coalescer);
}

#[test]
fn event_coalescer_characterization_batch_drain_resolves_every_payload_in_order() {
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("runtime build");
    let handle = runtime.handle();
    let manager = coalescer_test_manager_with_hooks(&[
        ExtensionEventName::MessageStart,
        ExtensionEventName::MessageEnd,
    ]);
    let coalescer = EventCoalescer::new(manager);
    let resolved = Arc::new(Mutex::new(Vec::new()));

    for (event, sequence) in [
        (ExtensionEventName::MessageStart, 1),
        (ExtensionEventName::MessageEnd, 2),
        (ExtensionEventName::MessageStart, 3),
    ] {
        coalescer.dispatch_fire_and_forget(
            event,
            coalescer_recording_payload(sequence, &resolved),
            &handle,
        );
    }

    assert_eq!(
        coalescer
            .batch_buffer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        3,
        "all non-coalescable events must share the scheduled batch"
    );
    assert!(
        coalescer
            .batch_drain_scheduled
            .load(std::sync::atomic::Ordering::Acquire),
        "exactly one drain task should be scheduled for the buffered batch"
    );

    runtime.block_on(async {
        for _ in 0..256 {
            let buffer_empty = coalescer
                .batch_buffer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty();
            let drain_scheduled = coalescer
                .batch_drain_scheduled
                .load(std::sync::atomic::Ordering::Acquire);
            if buffer_empty && !drain_scheduled {
                return;
            }
            asupersync::runtime::yield_now().await;
        }
        panic!("batch drain did not become idle");
    });

    assert_eq!(
        *resolved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        vec![1, 2, 3],
        "batch drain must preserve arrival order and resolve every payload"
    );
}

#[test]
fn event_coalescer_characterization_handoff_does_not_strand_payload() {
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("runtime build");
    let handle = runtime.handle();
    let manager = coalescer_test_manager_with_hooks(&[ExtensionEventName::MessageUpdate]);
    let coalescer = Arc::new(EventCoalescer::new(manager));
    let resolved = Arc::new(Mutex::new(Vec::new()));
    let resolver_entered = Arc::new(std::sync::Barrier::new(2));
    let replacement_queued = Arc::new(std::sync::Barrier::new(2));

    let first_resolved = Arc::clone(&resolved);
    let first_entered = Arc::clone(&resolver_entered);
    let first_release = Arc::clone(&replacement_queued);
    coalescer.dispatch_fire_and_forget(
        ExtensionEventName::MessageUpdate,
        CoalescedPayload::Lazy(Box::new(move || {
            first_resolved
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(1);
            first_entered.wait();
            first_release.wait();
            Some(json!({ "sequence": 1 }))
        })),
        &handle,
    );

    let producer_coalescer = Arc::clone(&coalescer);
    let producer_resolved = Arc::clone(&resolved);
    let producer_entered = Arc::clone(&resolver_entered);
    let producer_release = Arc::clone(&replacement_queued);
    let producer_handle = handle;
    let producer = std::thread::spawn(move || {
        producer_entered.wait();
        producer_coalescer.dispatch_fire_and_forget(
            ExtensionEventName::MessageUpdate,
            coalescer_recording_payload(2, &producer_resolved),
            &producer_handle,
        );
        producer_release.wait();
    });

    runtime.block_on(async {
        for _ in 0..256 {
            if coalescer
                .in_flight
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
            {
                return;
            }
            asupersync::runtime::yield_now().await;
        }
        panic!("replacement handoff did not become idle");
    });
    producer.join().expect("replacement producer thread");

    assert_eq!(
        *resolved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        vec![1, 2],
        "a replacement queued during active resolution must be handed off"
    );
    assert_coalescer_idle(&coalescer);
}

// ========================================================================
// Property-based tests for hostcall dispatch (bd-3pcw)
// ========================================================================

mod proptest_dispatch {
    use super::*;
    use proptest::prelude::*;

    fn op_strategy() -> impl Strategy<Value = String> {
        prop_oneof![
            Just("getActiveTools".to_string()),
            Just("getAllTools".to_string()),
            Just("setActiveTools".to_string()),
            Just("appendEntry".to_string()),
            Just("sendMessage".to_string()),
            Just("sendUserMessage".to_string()),
            Just("registerCommand".to_string()),
            Just("registerProvider".to_string()),
            Just("registerFlag".to_string()),
            Just("getModel".to_string()),
            Just("setModel".to_string()),
            Just("getThinkingLevel".to_string()),
            Just("setThinkingLevel".to_string()),
            Just("getFlag".to_string()),
            Just("listFlags".to_string()),
            Just("get_state".to_string()),
            Just("get_name".to_string()),
            Just("set_name".to_string()),
            Just("set_label".to_string()),
            Just("append_entry".to_string()),
            Just("get_messages".to_string()),
            "[a-zA-Z_]{0,30}".prop_map(|s| s),
        ]
    }

    fn json_leaf() -> impl Strategy<Value = Value> {
        prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            any::<i64>().prop_map(|n| json!(n)),
            ".{0,64}".prop_map(|s| json!(s)),
        ]
    }

    fn json_value() -> impl Strategy<Value = Value> {
        json_leaf().prop_recursive(3, 64, 8, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
                prop::collection::btree_map("[a-zA-Z0-9_]{1,10}", inner, 0..4).prop_map(|map| {
                    let mut out = serde_json::Map::new();
                    for (key, value) in map {
                        out.insert(key, value);
                    }
                    Value::Object(out)
                }),
            ]
        })
    }

    fn unicode_string() -> impl Strategy<Value = String> {
        prop_oneof![
            Just(String::new()),
            Just("\u{0}".to_string()),
            Just("\u{FEFF}BOM-prefixed".to_string()),
            Just("café résumé naïve".to_string()),
            Just("\u{200B}zero-width\u{200B}".to_string()),
            Just("\u{1F600}\u{1F4A9}\u{1F680}".to_string()),
            Just("日本語テスト".to_string()),
            Just("مرحبا".to_string()),
            "\\PC{1,100}".prop_map(|s| s),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 512,
            max_shrink_iters: 0,
            .. ProptestConfig::default()
        })]

        #[test]
        fn events_dispatch_never_panics(op in op_strategy(), payload in json_value()) {
            asupersync::test_utils::run_test(|| async move {
                let manager = ExtensionManager::new();
                let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);
                let _outcome = dispatch_hostcall_events(
                    "prop-call", &manager, &tools, &op, payload,
                ).await;
            });
        }

        #[test]
        fn session_dispatch_never_panics(op in op_strategy(), payload in json_value()) {
            asupersync::test_utils::run_test(|| async move {
                let manager = ExtensionManager::new();
                let session = Arc::new(MockSession::new());
                manager.set_session(session);
                let _outcome = dispatch_hostcall_session(
                    "prop-call", &manager, &op, payload,
                ).await;
            });
        }

        #[test]
        fn events_unknown_op_returns_error(
            op in "[a-z]{1,20}".prop_filter("not a known op", |s| {
                let norm = s.trim().to_ascii_lowercase();
                !matches!(
                    norm.as_str(),
                    "getactivetools" | "get_active_tools"
                        | "getalltools" | "get_all_tools"
                        | "setactivetools" | "set_active_tools"
                        | "appendentry" | "append_entry"
                        | "sendmessage" | "send_message"
                        | "sendusermessage" | "send_user_message"
                        | "registercommand" | "register_command"
                        | "registershortcut" | "register_shortcut"
                        | "registerprovider" | "register_provider"
                        | "registerflag" | "register_flag"
                        | "getmodel" | "get_model"
                        | "setmodel" | "set_model"
                        | "getthinkinglevel" | "get_thinking_level"
                        | "setthinkinglevel" | "set_thinking_level"
                        | "getflag" | "get_flag"
                        | "listflags" | "list_flags"
                        | "emit"
                )
            }),
            payload in json_value(),
        ) {
            asupersync::test_utils::run_test(|| async move {
                let manager = ExtensionManager::new();
                let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);
                let outcome = dispatch_hostcall_events(
                    "prop-call", &manager, &tools, &op, payload,
                ).await;
                assert!(
                    matches!(outcome, HostcallOutcome::Error { .. }),
                    "unknown op '{op}' should produce error, got: {outcome:?}"
                );
            });
        }

        #[test]
        fn session_unknown_op_returns_error(
            op in "[a-z]{1,20}".prop_filter("not a known session op", |s| {
                let norm = s.trim().to_ascii_lowercase();
                !matches!(
                    norm.as_str(),
                    "get_state" | "getstate"
                        | "get_messages" | "getmessages"
                        | "get_entries" | "getentries"
                        | "get_branch" | "getbranch"
                        | "get_file" | "getfile"
                        | "get_name" | "getname"
                        | "set_name" | "setname"
                        | "append_message" | "appendmessage"
                        | "append_entry" | "appendentry"
                        | "set_label" | "setlabel"
                )
            }),
            payload in json_value(),
        ) {
            asupersync::test_utils::run_test(|| async move {
                let manager = ExtensionManager::new();
                let session = Arc::new(MockSession::new());
                manager.set_session(session);
                let outcome = dispatch_hostcall_session(
                    "prop-call", &manager, &op, payload,
                ).await;
                assert!(
                    matches!(outcome, HostcallOutcome::Error { .. }),
                    "unknown session op '{op}' should produce error, got: {outcome:?}"
                );
            });
        }

        #[test]
        fn events_unicode_payloads_safe(
            op in op_strategy(),
            key in unicode_string(),
            value in unicode_string(),
        ) {
            asupersync::test_utils::run_test(|| async move {
                let manager = ExtensionManager::new();
                let tools = crate::tools::ToolRegistry::new(&["read"], Path::new("."), None);
                let actions = Arc::new(MockHostActions::new());
                manager.set_host_actions(actions);
                let session = Arc::new(MockSession::new());
                manager.set_session(session);
                let payload = json!({ key: value });
                let _outcome = dispatch_hostcall_events(
                    "prop-call", &manager, &tools, &op, payload,
                ).await;
            });
        }

        #[test]
        fn session_unicode_payloads_safe(
            op in op_strategy(),
            key in unicode_string(),
            value in unicode_string(),
        ) {
            asupersync::test_utils::run_test(|| async move {
                let manager = ExtensionManager::new();
                let session = Arc::new(MockSession::new());
                manager.set_session(session);
                let payload = json!({ key: value });
                let _outcome = dispatch_hostcall_session(
                    "prop-call", &manager, &op, payload,
                ).await;
            });
        }

        #[test]
        fn events_send_message_requires_custom_type(payload in json_value()) {
            asupersync::test_utils::run_test(|| async move {
                let manager = ExtensionManager::new();
                let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);
                let actions = Arc::new(MockHostActions::new());
                manager.set_host_actions(actions.clone());
                let message = match payload {
                    Value::Object(map) => {
                        let mut filtered = map;
                        filtered.remove("customType");
                        filtered.remove("custom_type");
                        Value::Object(filtered)
                    }
                    other => other,
                };
                let mut obj = serde_json::Map::new();
                obj.insert("message".to_string(), message);
                let outcome = dispatch_hostcall_events(
                    "prop-call", &manager, &tools, "sendMessage",
                    Value::Object(obj),
                ).await;
                assert!(
                    matches!(outcome, HostcallOutcome::Error { .. }),
                    "sendMessage without customType should error, got: {outcome:?}"
                );
                assert_eq!(actions.messages.lock().unwrap_or_else(std::sync::PoisonError::into_inner).len(), 0);
            });
        }

        #[test]
        fn session_dispatch_without_session_returns_error(
            op in op_strategy(),
            payload in json_value(),
        ) {
            asupersync::test_utils::run_test(|| async move {
                let manager = ExtensionManager::new();
                let outcome = dispatch_hostcall_session(
                    "prop-call", &manager, &op, payload,
                ).await;
                assert!(
                    matches!(outcome, HostcallOutcome::Error { .. }),
                    "session dispatch without session should error, got: {outcome:?}"
                );
            });
        }

        #[test]
        fn events_model_state_consistent(
            providers in prop::collection::vec("[a-z]{1,10}", 1..8),
            models in prop::collection::vec("[a-z]{1,10}", 1..8),
        ) {
            asupersync::test_utils::run_test(|| async move {
                let manager = ExtensionManager::new();
                let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);
                let count = providers.len().min(models.len());
                for i in 0..count {
                    let _ = dispatch_hostcall_events(
                        "prop-call", &manager, &tools, "setModel",
                        json!({ "provider": providers[i], "modelId": models[i] }),
                    ).await;
                }
                let outcome = dispatch_hostcall_events(
                    "prop-call", &manager, &tools, "getModel", json!({}),
                ).await;
                if let HostcallOutcome::Success(value) = outcome {
                    let last = count - 1;
                    assert_eq!(
                        value.get("provider").and_then(Value::as_str),
                        Some(providers[last].as_str())
                    );
                    assert_eq!(
                        value.get("modelId").and_then(Value::as_str),
                        Some(models[last].as_str())
                    );
                }
            });
        }

        #[test]
        fn events_thinking_level_state_consistent(
            levels in prop::collection::vec(
                prop_oneof![
                    Just("low".to_string()),
                    Just("medium".to_string()),
                    Just("high".to_string()),
                    Just("xhigh".to_string()),
                    "[a-z]{1,10}".prop_map(|s| s),
                ],
                1..10,
            )
        ) {
            asupersync::test_utils::run_test(|| async move {
                let manager = ExtensionManager::new();
                let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);
                for level in &levels {
                    let _ = dispatch_hostcall_events(
                        "prop-call", &manager, &tools, "setThinkingLevel",
                        json!({ "thinkingLevel": level }),
                    ).await;
                }
                let outcome = dispatch_hostcall_events(
                    "prop-call", &manager, &tools, "getThinkingLevel", json!({}),
                ).await;
                if let HostcallOutcome::Success(value) = outcome {
                    assert_eq!(
                        value.get("thinkingLevel").and_then(Value::as_str),
                        Some(levels.last().unwrap().as_str())
                    );
                }
            });
        }

        #[test]
        fn events_active_tools_roundtrip(
            tools_list in prop::collection::vec("[a-z]{1,10}", 0..8)
        ) {
            asupersync::test_utils::run_test(|| async move {
                let manager = ExtensionManager::new();
                let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);
                let _ = dispatch_hostcall_events(
                    "prop-call", &manager, &tools, "setActiveTools",
                    json!({ "tools": tools_list }),
                ).await;
                let expected = manager.active_tools();
                assert_eq!(expected, Some(tools_list));
            });
        }

        #[test]
        fn session_set_label_requires_target_id(label in ".*") {
            asupersync::test_utils::run_test(|| async move {
                let manager = ExtensionManager::new();
                let session = Arc::new(MockSession::new());
                manager.set_session(session);
                let outcome = dispatch_hostcall_session(
                    "prop-call", &manager, "set_label",
                    json!({ "targetId": "", "label": label }),
                ).await;
                assert!(
                    matches!(outcome, HostcallOutcome::Error { .. }),
                    "set_label with empty targetId should error"
                );
                let outcome2 = dispatch_hostcall_session(
                    "prop-call", &manager, "set_label",
                    json!({ "label": label }),
                ).await;
                assert!(
                    matches!(outcome2, HostcallOutcome::Error { .. }),
                    "set_label without targetId should error"
                );
            });
        }

        #[test]
        fn session_name_roundtrip(name in unicode_string()) {
            asupersync::test_utils::run_test(|| async move {
                let manager = ExtensionManager::new();
                let session = Arc::new(MockSession::new());
                manager.set_session(session);
                let _ = dispatch_hostcall_session(
                    "prop-call", &manager, "set_name",
                    json!({ "name": name }),
                ).await;
                let outcome = dispatch_hostcall_session(
                    "prop-call", &manager, "get_name", json!({}),
                ).await;
                if let HostcallOutcome::Success(value) = outcome {
                    let got = value.as_str().unwrap_or_default();
                    assert_eq!(got, &name);
                }
            });
        }

        #[test]
        fn typed_opcode_context_routes_to_fast_lane(
            case_idx in 0usize..4,
            call_seed in "[a-z0-9]{1,12}",
        ) {
            let (capability, method, params, expected_opcode) = match case_idx {
                0 => (
                    "read",
                    "tool",
                    json!({
                        "name": "read",
                        "input": { "path": "README.md", "offset": 0, "limit": 16 }
                    }),
                    CommonHostcallOpcode::ToolRead,
                ),
                1 => (
                    "session",
                    "session",
                    json!({ "op": "get_name" }),
                    CommonHostcallOpcode::SessionGetName,
                ),
                2 => (
                    "session",
                    "session",
                    json!({ "op": "get_state" }),
                    CommonHostcallOpcode::SessionGetState,
                ),
                _ => (
                    "events",
                    "events",
                    json!({ "op": "list_flags" }),
                    CommonHostcallOpcode::EventsListFlags,
                ),
            };

            let context = hostcall_opcode_context_for_params(method, &params);
            prop_assert!(context.is_some(), "context must exist for supported opcode case");

            let payload = HostCallPayload {
                call_id: format!("prop-fast-{call_seed}-{case_idx}"),
                capability: capability.to_string(),
                method: method.to_string(),
                params,
                timeout_ms: None,
                cancel_token: None,
                context,
            };

            prop_assert!(
                validate_host_call(&payload).is_ok(),
                "typed context payload must validate"
            );
            let lane = select_hostcall_lane(&payload)
                .expect("typed opcode payload should select lane successfully");
            prop_assert_eq!(lane.lane, HostcallDispatchLane::Fast);
            prop_assert_eq!(lane.reason, "typed_opcode_context_v1");
            prop_assert_eq!(lane.opcode, Some(expected_opcode));
        }

        #[test]
        fn tool_read_marshalling_hash_is_invariant_to_key_order(
            path in "[a-zA-Z0-9_./-]{1,24}",
            offset in 0u32..4096,
            limit in 1u32..2048,
        ) {
            let mut input_a = serde_json::Map::new();
            input_a.insert("path".to_string(), json!(path));
            input_a.insert("offset".to_string(), json!(offset));
            input_a.insert("limit".to_string(), json!(limit));

            let mut input_b = serde_json::Map::new();
            input_b.insert("limit".to_string(), json!(limit));
            input_b.insert("path".to_string(), json!(path));
            input_b.insert("offset".to_string(), json!(offset));

            let mut params_a_obj = serde_json::Map::new();
            params_a_obj.insert("name".to_string(), json!("read"));
            params_a_obj.insert("input".to_string(), Value::Object(input_a));
            let params_a = Value::Object(params_a_obj);

            let mut params_b_obj = serde_json::Map::new();
            params_b_obj.insert("name".to_string(), json!("read"));
            params_b_obj.insert("input".to_string(), Value::Object(input_b));
            let params_b = Value::Object(params_b_obj);

            let generic_a = hostcall_params_hash("tool", &params_a);
            let generic_b = hostcall_params_hash("tool", &params_b);
            prop_assert_eq!(&generic_a, &generic_b);

            let shape_a = hostcall_params_shape_hash("tool", &params_a);
            let shape_b = hostcall_params_shape_hash("tool", &params_b);
            prop_assert_eq!(shape_a, shape_b);

            let artifacts_a =
                HostcallPayloadArena::new("tool", &params_a, Some(CommonHostcallOpcode::ToolRead))
                    .marshal();
            let artifacts_b =
                HostcallPayloadArena::new("tool", &params_b, Some(CommonHostcallOpcode::ToolRead))
                    .marshal();

            prop_assert_eq!(&artifacts_a.params_hash, &artifacts_b.params_hash);
            prop_assert_eq!(&artifacts_a.params_hash, &generic_a);
            prop_assert_eq!(&artifacts_a.args_shape_hash, &artifacts_b.args_shape_hash);
            prop_assert_eq!(
                artifacts_a.telemetry.path,
                HOSTCALL_MARSHALLING_PATH_FAST_OPCODE
            );
            prop_assert_eq!(
                artifacts_b.telemetry.path,
                HOSTCALL_MARSHALLING_PATH_FAST_OPCODE
            );
            prop_assert!(artifacts_a.telemetry.fallback_reason.is_none());
            prop_assert!(artifacts_b.telemetry.fallback_reason.is_none());
        }

        #[test]
        fn mismatched_typed_opcode_context_is_rejected(
            use_get_name_payload in proptest::bool::ANY,
            call_seed in "[a-z0-9]{1,12}",
        ) {
            let (params, mismatched_code) = if use_get_name_payload {
                (json!({ "op": "get_name" }), "session.set_name")
            } else {
                (json!({ "op": "set_name", "name": "x" }), "session.get_name")
            };

            let payload = HostCallPayload {
                call_id: format!("prop-mismatch-{call_seed}"),
                capability: "session".to_string(),
                method: "session".to_string(),
                params,
                timeout_ms: None,
                cancel_token: None,
                context: Some(json!({
                    "typed_opcode": {
                        "schema": HOSTCALL_OPCODE_SCHEMA_VERSION,
                        "version": HOSTCALL_OPCODE_VERSION,
                        "code": mismatched_code
                    }
                })),
            };

            let err = validate_host_call(&payload)
                .expect_err("mismatched typed opcode context must fail validation");
            prop_assert!(
                err.to_string()
                    .contains("does not match payload-derived opcode"),
                "unexpected error: {err}"
            );
            prop_assert!(select_hostcall_lane(&payload).is_err());
        }
    }

    // ------------------------------------------------------------------
    // FUZZ-P1.9: Extended proptest coverage (bd-388hn)
    // ------------------------------------------------------------------

    /// Generate a malformed `registerProvider` payload.
    fn malformed_provider_payload() -> impl Strategy<Value = Value> {
        prop_oneof![
            // Missing id
            Just(json!({"api": "openai-completions"})),
            // Missing api
            Just(json!({"id": "test-provider"})),
            // Empty id
            Just(json!({"id": "", "api": "anthropic-messages"})),
            // Empty api
            Just(json!({"id": "test-provider", "api": ""})),
            // Invalid api type
            "[a-z]{3,15}".prop_map(|api| json!({"id": "test-provider", "api": api})),
            // Whitespace-only id
            Just(json!({"id": "   ", "api": "anthropic-messages"})),
            // Null values
            Just(json!({"id": null, "api": null})),
            // Wrong types for fields
            Just(json!({"id": 42, "api": true})),
        ]
    }

    /// Generate malformed OAuth objects for provider registration fuzzing.
    fn malformed_oauth_object() -> impl Strategy<Value = Value> {
        prop_oneof![
            // Missing required fields.
            Just(json!({})),
            Just(json!({"authUrl": "https://auth.example.com/authorize"})),
            Just(json!({"tokenUrl": "https://auth.example.com/token"})),
            Just(json!({"clientId": "client-123"})),
            // Wrong field types.
            Just(
                json!({"authUrl": 123, "tokenUrl": "https://auth.example.com/token", "clientId": "client-123"})
            ),
            Just(
                json!({"authUrl": "https://auth.example.com/authorize", "tokenUrl": false, "clientId": "client-123"})
            ),
            Just(
                json!({"authUrl": "https://auth.example.com/authorize", "tokenUrl": "https://auth.example.com/token", "clientId": null})
            ),
            // Arrays/objects where strings are expected.
            Just(
                json!({"authUrl": ["https://auth.example.com/authorize"], "tokenUrl": "https://auth.example.com/token", "clientId": "client-123"})
            ),
            Just(
                json!({"authUrl": "https://auth.example.com/authorize", "tokenUrl": {"href": "https://auth.example.com/token"}, "clientId": "client-123"})
            ),
            // Non-array scopes and wrong redirect type.
            Just(json!({
                "authUrl": "https://auth.example.com/authorize",
                "tokenUrl": "https://auth.example.com/token",
                "clientId": "client-123",
                "scopes": "read write",
                "redirectUri": 42
            })),
        ]
    }

    /// Generate a large JSON payload for stress testing.
    fn large_json_payload(size: usize) -> impl Strategy<Value = Value> {
        prop::collection::vec("[a-zA-Z0-9]{1,10}", size..size + 1).prop_map(|items| {
            let mut map = serde_json::Map::new();
            for (i, item) in items.into_iter().enumerate() {
                map.insert(format!("key_{i}"), json!(item));
            }
            Value::Object(map)
        })
    }

    /// Generate a deeply nested JSON value.
    fn deeply_nested_json(depth: u32) -> Value {
        let mut v = json!("leaf");
        for i in 0..depth {
            v = json!({ format!("level_{i}"): v });
        }
        v
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 256,
            max_shrink_iters: 0,
            .. ProptestConfig::default()
        })]

        /// Test `registerProvider` with malformed payloads — must not panic.
        #[test]
        fn register_provider_malformed_never_panics(
            payload in malformed_provider_payload()
        ) {
            asupersync::test_utils::run_test(|| async move {
                let manager = ExtensionManager::new();
                let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);
                let _outcome = dispatch_hostcall_events(
                    "prop-call", &manager, &tools, "registerProvider", payload,
                ).await;
            });
        }

        /// Malformed OAuth objects must not panic and must not produce oauth_config.
        #[test]
        fn register_provider_malformed_oauth_is_ignored(
            provider_id in "[a-z][a-z0-9\\-]{0,24}",
            oauth in malformed_oauth_object()
        ) {
            asupersync::test_utils::run_test(|| async move {
                let manager = ExtensionManager::new();
                let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);
                let outcome = dispatch_hostcall_events(
                    "prop-call",
                    &manager,
                    &tools,
                    "registerProvider",
                    json!({
                        "id": provider_id,
                        "api": "openai-completions",
                        "baseUrl": "https://api.example.com/v1",
                        "oauth": oauth,
                        "models": [{ "id": "oauth-model", "name": "OAuth Model" }]
                    }),
                )
                .await;
                assert!(
                    matches!(outcome, HostcallOutcome::Success(_)),
                    "registerProvider should accept malformed oauth payload shape without panic"
                );

                let entries = manager.extension_model_entries();
                assert_eq!(entries.len(), 1, "expected exactly one model entry");
                let has_required_oauth_strings = oauth
                    .get("authUrl")
                    .and_then(Value::as_str)
                    .is_some()
                    && oauth
                        .get("tokenUrl")
                        .and_then(Value::as_str)
                        .is_some()
                    && oauth
                        .get("clientId")
                        .and_then(Value::as_str)
                        .is_some();
                if has_required_oauth_strings {
                    assert!(
                        entries[0].oauth_config.is_some(),
                        "oauth_config should be extracted when required oauth fields are strings"
                    );
                } else {
                    assert!(
                        entries[0].oauth_config.is_none(),
                        "oauth_config should be omitted when required oauth fields are malformed"
                    );
                }
            });
        }

        /// Test `registerCommand` with arbitrary payloads — must not panic.
        #[test]
        fn register_command_arbitrary_never_panics(
            name in "\\PC{0,50}",
            description in prop::option::of("\\PC{0,100}"),
        ) {
            asupersync::test_utils::run_test(|| async move {
                let manager = ExtensionManager::new();
                let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);
                let mut payload = json!({"name": name});
                if let Some(desc) = description {
                    payload["description"] = json!(desc);
                }
                let _outcome = dispatch_hostcall_events(
                    "prop-call", &manager, &tools, "registerCommand", payload,
                ).await;
            });
        }

        /// Test `registerFlag` with arbitrary payloads — must not panic.
        #[test]
        fn register_flag_arbitrary_never_panics(
            name in "\\PC{0,50}",
            description in prop::option::of("\\PC{0,100}"),
            default_val in prop::option::of(json_leaf()),
        ) {
            asupersync::test_utils::run_test(|| async move {
                let manager = ExtensionManager::new();
                let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);
                let mut payload = json!({"name": name});
                if let Some(desc) = description {
                    payload["description"] = json!(desc);
                }
                if let Some(dv) = default_val {
                    payload["default"] = dv;
                }
                let _outcome = dispatch_hostcall_events(
                    "prop-call", &manager, &tools, "registerFlag", payload,
                ).await;
            });
        }

        /// Test session `append_entry` with arbitrary custom types and data.
        #[test]
        fn session_append_entry_never_panics(
            custom_type in "\\PC{0,30}",
            data in json_value(),
        ) {
            asupersync::test_utils::run_test(|| async move {
                let manager = ExtensionManager::new();
                let session = Arc::new(MockSession::new());
                manager.set_session(session);
                let payload = json!({
                    "customType": custom_type,
                    "data": data,
                });
                let _outcome = dispatch_hostcall_session(
                    "prop-call", &manager, "append_entry", payload,
                ).await;
            });
        }

        /// Test session `set_label` with arbitrary target IDs and labels.
        #[test]
        fn session_set_label_arbitrary_never_panics(
            target_id in "\\PC{0,50}",
            label in prop::option::of("\\PC{0,50}"),
        ) {
            asupersync::test_utils::run_test(|| async move {
                let manager = ExtensionManager::new();
                let session = Arc::new(MockSession::new());
                manager.set_session(session);
                let mut payload = json!({"targetId": target_id});
                if let Some(l) = label {
                    payload["label"] = json!(l);
                }
                let _outcome = dispatch_hostcall_session(
                    "prop-call", &manager, "set_label", payload,
                ).await;
            });
        }

        /// Test session `set_model` round-trip with arbitrary provider/model.
        /// When either provider or `model_id` is empty, the handler returns an
        /// error and does NOT update the session, so we only assert the
        /// round-trip when both are non-empty.
        #[test]
        fn session_set_model_roundtrip(
            provider in "\\PC{0,30}",
            model_id in "\\PC{0,30}",
        ) {
            asupersync::test_utils::run_test(|| async move {
                let manager = ExtensionManager::new();
                let session = Arc::new(MockSession::new());
                manager.set_session(session.clone());
                let outcome = dispatch_hostcall_session(
                    "prop-call", &manager, "set_model",
                    json!({"provider": provider, "modelId": model_id}),
                ).await;
                if provider.is_empty() || model_id.is_empty() {
                    // Handler validates that both are non-empty.
                    assert!(
                        matches!(outcome, HostcallOutcome::Error { .. }),
                        "set_model with empty provider/model should error"
                    );
                } else {
                    let (got_provider, got_model) = session.get_model().await;
                    assert_eq!(got_provider.as_deref(), Some(provider.as_str()));
                    assert_eq!(got_model.as_deref(), Some(model_id.as_str()));
                }
            });
        }

        /// Stress test: large payload dispatch must not panic.
        #[test]
        fn dispatch_large_payload_stress(
            op in op_strategy(),
            payload in large_json_payload(200),
        ) {
            asupersync::test_utils::run_test(|| async move {
                let manager = ExtensionManager::new();
                let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);
                let session = Arc::new(MockSession::new());
                manager.set_session(session);
                let actions = Arc::new(MockHostActions::new());
                manager.set_host_actions(actions);
                let _outcome = dispatch_hostcall_events(
                    "prop-call", &manager, &tools, &op, payload,
                ).await;
            });
        }
    }

    /// Deeply nested JSON payloads (up to 50 levels) must not panic.
    #[test]
    fn dispatch_deeply_nested_payload_never_panics() {
        asupersync::test_utils::run_test(|| async move {
            let manager = ExtensionManager::new();
            let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);
            let session = Arc::new(MockSession::new());
            manager.set_session(session);

            for depth in [10, 25, 50] {
                let payload = deeply_nested_json(depth);
                let _outcome = dispatch_hostcall_events(
                    "prop-call",
                    &manager,
                    &tools,
                    "sendMessage",
                    payload.clone(),
                )
                .await;
                let _outcome =
                    dispatch_hostcall_session("prop-call", &manager, "append_entry", payload).await;
            }
        });
    }

    /// `registerProvider` with valid API types should succeed.
    #[test]
    fn register_provider_valid_api_types_succeed() {
        asupersync::test_utils::run_test(|| async move {
            let manager = ExtensionManager::new();
            let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);
            for api in [
                "anthropic-messages",
                "openai-completions",
                "openai-responses",
                "google-generative-ai",
            ] {
                let outcome = dispatch_hostcall_events(
                    "prop-call",
                    &manager,
                    &tools,
                    "registerProvider",
                    json!({"id": format!("provider-{api}"), "api": api}),
                )
                .await;
                assert!(
                    matches!(outcome, HostcallOutcome::Success(_)),
                    "registerProvider with api={api} should succeed, got: {outcome:?}"
                );
            }
        });
    }

    /// `registerProvider` with invalid API types should error.
    #[test]
    fn register_provider_invalid_api_types_error() {
        asupersync::test_utils::run_test(|| async move {
            let manager = ExtensionManager::new();
            let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);
            for api in ["invalid-api", "openai", "anthropic", ""] {
                let outcome = dispatch_hostcall_events(
                    "prop-call",
                    &manager,
                    &tools,
                    "registerProvider",
                    json!({"id": "test-provider", "api": api}),
                )
                .await;
                assert!(
                    matches!(outcome, HostcallOutcome::Error { .. }),
                    "registerProvider with api={api:?} should error, got: {outcome:?}"
                );
            }
        });
    }

    // ------------------------------------------------------------------
    // FUZZ-P1.9 Phase 2: Capability bypass + concurrent dispatch tests
    // ------------------------------------------------------------------

    /// Strategy for methods that require specific capabilities.
    fn capability_method_strategy() -> impl Strategy<Value = (String, String)> {
        prop_oneof![
            Just(("session".to_string(), "session".to_string())),
            Just(("events".to_string(), "events".to_string())),
            Just(("ui".to_string(), "ui".to_string())),
            Just(("tool".to_string(), "tool".to_string())),
            Just(("exec".to_string(), "exec".to_string())),
            Just(("http".to_string(), "http".to_string())),
            Just(("log".to_string(), "log".to_string())),
            Just(("fs".to_string(), "read".to_string())),
        ]
    }

    /// Strategy for capability mismatch bypass attempts.
    fn capability_mismatch_case_strategy() -> impl Strategy<Value = (String, String, Value)> {
        prop_oneof![
            // tool(read) requires read, but declared exec
            Just((
                "tool".to_string(),
                "exec".to_string(),
                json!({ "name": "read", "input": { "path": "README.md" } }),
            )),
            // fs(read) requires read, but declared write
            Just((
                "fs".to_string(),
                "write".to_string(),
                json!({ "op": "read", "path": "README.md" }),
            )),
            // exec requires exec, but declared read
            Just((
                "exec".to_string(),
                "read".to_string(),
                json!({ "cmd": "echo", "args": ["hello"] }),
            )),
            // session requires session, but declared ui
            Just((
                "session".to_string(),
                "ui".to_string(),
                json!({ "op": "get_state" }),
            )),
            // ui requires ui, but declared events
            Just((
                "ui".to_string(),
                "events".to_string(),
                json!({ "op": "notify", "title": "hi" }),
            )),
            // events requires events, but declared session
            Just((
                "events".to_string(),
                "session".to_string(),
                json!({ "op": "sendMessage", "message": { "customType": "x", "content": "y" } }),
            )),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 256,
            max_shrink_iters: 0,
            .. ProptestConfig::default()
        })]

        /// Capability bypass: deny-all policy must reject all capability-gated calls.
        #[test]
        fn capability_deny_all_rejects_gated_calls(
            (method, _expected_cap) in capability_method_strategy(),
            payload in json_value(),
        ) {
            asupersync::test_utils::run_test(|| async move {
                let dir = tempfile::tempdir().expect("tempdir");
                let tools = crate::tools::ToolRegistry::new(&[], dir.path(), None);
                let http = crate::connectors::http::HttpConnector::with_defaults();
                let policy = ExtensionPolicy {
                    mode: ExtensionPolicyMode::Strict,
                    max_memory_mb: 256,
                    default_caps: Vec::new(),
                    deny_caps: vec![
                        "read".to_string(),
                        "write".to_string(),
                        "exec".to_string(),
                        "http".to_string(),
                        "tool".to_string(),
                        "session".to_string(),
                        "ui".to_string(),
                        "events".to_string(),
                        "log".to_string(),
                        "env".to_string(),
                    ],
                    ..Default::default()
                };
                let ctx = HostCallContext {
                    runtime_name: "prop-test",
                    extension_id: Some("ext.prop"),
                    tools: &tools,
                    http: &http,
                    manager: None,
                    policy: &policy,
                    js_runtime: None,
                    interceptor: None,
                };
                let call = HostCallPayload {
                    call_id: "cap-bypass-test".to_string(),
                    capability: String::new(),
                    method,
                    params: payload,
                    timeout_ms: Some(5000),
                    cancel_token: None,
                    context: None,
                };
                let result = dispatch_host_call_shared(&ctx, call).await;
                assert!(
                    result.is_error,
                    "deny-all policy should reject call, got is_error=false"
                );
            });
        }

        /// Capability bypass: per-extension deny overrides global allow.
        #[test]
        fn capability_per_extension_deny_overrides_allow(
            (method, _expected_cap) in capability_method_strategy(),
            payload in json_value(),
        ) {
            asupersync::test_utils::run_test(|| async move {
                let dir = tempfile::tempdir().expect("tempdir");
                let tools = crate::tools::ToolRegistry::new(&[], dir.path(), None);
                let http = crate::connectors::http::HttpConnector::with_defaults();
                let mut per_ext = std::collections::HashMap::new();
                per_ext.insert(
                    "ext.untrusted".to_string(),
                    ExtensionOverride {
                        mode: None,
                        allow: Vec::new(),
                        deny: vec![
                            "session".to_string(),
                            "events".to_string(),
                            "ui".to_string(),
                            "tool".to_string(),
                            "exec".to_string(),
                            "http".to_string(),
                            "log".to_string(),
                            "read".to_string(),
                            "write".to_string(),
                            "env".to_string(),
                        ],
                        quota: None,
                    },
                );
                let policy = ExtensionPolicy {
                    mode: ExtensionPolicyMode::Permissive,
                    max_memory_mb: 256,
                    default_caps: Vec::new(),
                    deny_caps: Vec::new(),
                    per_extension: per_ext,
                    ..Default::default()
                };
                let ctx = HostCallContext {
                    runtime_name: "prop-test",
                    extension_id: Some("ext.untrusted"),
                    tools: &tools,
                    http: &http,
                    manager: None,
                    policy: &policy,
                    js_runtime: None,
                    interceptor: None,
                };
                let call = HostCallPayload {
                    call_id: "per-ext-deny".to_string(),
                    capability: String::new(),
                    method,
                    params: payload,
                    timeout_ms: Some(5000),
                    cancel_token: None,
                    context: None,
                };
                let result = dispatch_host_call_shared(&ctx, call).await;
                assert!(
                    result.is_error,
                    "per-extension deny should reject, got is_error=false"
                );
            });
        }

        /// Capability bypass: declared capability mismatch must be rejected as invalid_request.
        #[test]
        fn capability_mismatch_rejected_as_invalid_request(
            (method, declared_capability, params) in capability_mismatch_case_strategy(),
            call_suffix in "[a-z0-9]{1,12}",
        ) {
            asupersync::test_utils::run_test(|| async move {
                let dir = tempfile::tempdir().expect("tempdir");
                let tools = crate::tools::ToolRegistry::new(&[], dir.path(), None);
                let http = crate::connectors::http::HttpConnector::with_defaults();
                let policy = ExtensionPolicy {
                    mode: ExtensionPolicyMode::Permissive,
                    max_memory_mb: 256,
                    default_caps: Vec::new(),
                    deny_caps: Vec::new(),
                    ..Default::default()
                };
                let ctx = HostCallContext {
                    runtime_name: "prop-test",
                    extension_id: Some("ext.prop"),
                    tools: &tools,
                    http: &http,
                    manager: None,
                    policy: &policy,
                    js_runtime: None,
                    interceptor: None,
                };
                let call = HostCallPayload {
                    call_id: format!("cap-mismatch-{call_suffix}"),
                    capability: declared_capability,
                    method,
                    params,
                    timeout_ms: Some(5_000),
                    cancel_token: None,
                    context: None,
                };
                let result = dispatch_host_call_shared(&ctx, call).await;
                assert!(result.is_error, "capability mismatch must be rejected");
                let err = result.error.expect("error payload should exist");
                assert_eq!(
                    err.code,
                    HostCallErrorCode::InvalidRequest,
                    "capability mismatch must map to invalid_request"
                );
                assert!(
                    err.message.contains("mismatch"),
                    "error message should describe mismatch, got: {}",
                    err.message
                );
            });
        }

        /// Concurrent dispatch: multiple rapid dispatches to same manager must not panic.
        #[test]
        fn concurrent_dispatch_same_manager_safe(
            ops in prop::collection::vec(op_strategy(), 3..8),
            payloads in prop::collection::vec(json_value(), 3..8),
        ) {
            asupersync::test_utils::run_test(|| async move {
                let manager = ExtensionManager::new();
                let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);
                let session = Arc::new(MockSession::new());
                manager.set_session(session);
                let actions = Arc::new(MockHostActions::new());
                manager.set_host_actions(actions);
                let count = ops.len().min(payloads.len());
                for i in 0..count {
                    let _outcome = dispatch_hostcall_events(
                        &format!("concurrent-{i}"),
                        &manager,
                        &tools,
                        &ops[i],
                        payloads[i].clone(),
                    )
                    .await;
                }
                // Also dispatch session ops in the same run.
                for i in 0..count {
                    let _outcome = dispatch_hostcall_session(
                        &format!("concurrent-session-{i}"),
                        &manager,
                        &ops[i],
                        payloads[i].clone(),
                    )
                    .await;
                }
            });
        }

        /// Tool registration: malformed extension tool definitions must not panic.
        #[test]
        fn extension_tool_def_malformed_never_panics(
            name in "\\PC{0,50}",
            description in "\\PC{0,200}",
            schema in json_value(),
        ) {
            asupersync::test_utils::run_test(|| async move {
                let manager = ExtensionManager::new();
                let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);
                // Register an extension with a malformed tool definition.
                let tool_def = json!({
                    "name": name,
                    "description": description,
                    "inputSchema": schema,
                });
                let payload = RegisterPayload {
                    name: "prop-test-ext".to_string(),
                    version: "0.1.0".to_string(),
                    api_version: "1".to_string(),
                    capabilities: Vec::new(),
                    capability_manifest: None,
                    tools: vec![tool_def],
                    slash_commands: Vec::new(),
                    shortcuts: Vec::new(),
                    flags: Vec::new(),
                    event_hooks: Vec::new(),
                };
                manager.register(payload);
                // Verify tool defs are retrievable and getAllTools doesn't panic.
                let _defs = manager.extension_tool_defs();
                let _outcome = dispatch_hostcall_events(
                    "tool-def-test",
                    &manager,
                    &tools,
                    "getAllTools",
                    json!({}),
                )
                .await;
            });
        }
    }

    /// Capability bypass: verify that all known capability methods
    /// are properly denied under deny-all policy (non-proptest variant
    /// for deterministic coverage).
    #[test]
    fn capability_deny_all_covers_all_methods() {
        asupersync::test_utils::run_test(|| async move {
            let dir = tempfile::tempdir().expect("tempdir");
            let tools = crate::tools::ToolRegistry::new(&[], dir.path(), None);
            let http = crate::connectors::http::HttpConnector::with_defaults();
            let policy = ExtensionPolicy {
                mode: ExtensionPolicyMode::Strict,
                max_memory_mb: 256,
                default_caps: Vec::new(),
                deny_caps: vec![
                    "read".to_string(),
                    "write".to_string(),
                    "exec".to_string(),
                    "http".to_string(),
                    "tool".to_string(),
                    "session".to_string(),
                    "ui".to_string(),
                    "events".to_string(),
                    "log".to_string(),
                    "env".to_string(),
                ],
                ..Default::default()
            };
            for method in [
                "session", "events", "ui", "tool", "exec", "http", "log", "env",
            ] {
                let ctx = HostCallContext {
                    runtime_name: "prop-test",
                    extension_id: Some("ext.test"),
                    tools: &tools,
                    http: &http,
                    manager: None,
                    policy: &policy,
                    js_runtime: None,
                    interceptor: None,
                };
                let call = HostCallPayload {
                    call_id: format!("deny-{method}"),
                    capability: String::new(),
                    method: method.to_string(),
                    params: json!({"op": "test"}),
                    timeout_ms: Some(5000),
                    cancel_token: None,
                    context: None,
                };
                let result = dispatch_host_call_shared(&ctx, call).await;
                assert!(
                    result.is_error,
                    "deny-all policy should reject method={method}, got is_error=false"
                );
            }
        });
    }

    /// Concurrent session state: set/get model and name interleaved
    /// across multiple dispatches preserves consistency.
    #[test]
    fn concurrent_session_state_interleaved() {
        asupersync::test_utils::run_test(|| async move {
            let manager = ExtensionManager::new();
            let session = Arc::new(MockSession::new());
            manager.set_session(session.clone());

            // Interleave model and name updates.
            for i in 0..20 {
                let _out = dispatch_hostcall_session(
                    "interleave",
                    &manager,
                    "set_name",
                    json!({"name": format!("session-{i}")}),
                )
                .await;
                let _out = dispatch_hostcall_session(
                    "interleave",
                    &manager,
                    "set_model",
                    json!({"provider": format!("prov-{i}"), "modelId": format!("model-{i}")}),
                )
                .await;
            }

            // Final state should reflect last update.
            let name_out =
                dispatch_hostcall_session("interleave", &manager, "get_name", json!({})).await;
            if let HostcallOutcome::Success(value) = name_out {
                assert_eq!(
                    value.as_str(),
                    Some("session-19"),
                    "final session name should be session-19"
                );
            }

            let (got_prov, got_model) = session.get_model().await;
            assert_eq!(got_prov.as_deref(), Some("prov-19"));
            assert_eq!(got_model.as_deref(), Some("model-19"));
        });
    }

    /// True parallel dispatch: spawn multiple hostcalls onto one runtime and one manager.
    #[test]
    fn concurrent_dispatch_parallel_tasks_same_manager_safe() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let handle = runtime.handle();

        runtime.block_on(async move {
            let manager = ExtensionManager::new();
            let session = Arc::new(MockSession::new());
            manager.set_session(session);
            let actions = Arc::new(MockHostActions::new());
            manager.set_host_actions(actions);
            let tools = Arc::new(crate::tools::ToolRegistry::new(&[], Path::new("."), None));

            let mut joins = Vec::new();
            for idx in 0..12 {
                let manager_cloned = manager.clone();
                let tools_cloned = Arc::clone(&tools);
                joins.push(handle.spawn(async move {
                    let event_payload = if idx % 2 == 0 {
                        json!({
                            "provider": format!("provider-{idx}"),
                            "modelId": format!("model-{idx}")
                        })
                    } else {
                        json!({
                            "thinkingLevel": if idx % 3 == 0 { "high" } else { "medium" }
                        })
                    };
                    let event_op = if idx % 2 == 0 {
                        "setModel"
                    } else {
                        "setThinkingLevel"
                    };
                    let _event = dispatch_hostcall_events(
                        &format!("parallel-event-{idx}"),
                        &manager_cloned,
                        &tools_cloned,
                        event_op,
                        event_payload,
                    )
                    .await;
                    let _session = dispatch_hostcall_session(
                        &format!("parallel-session-{idx}"),
                        &manager_cloned,
                        "set_name",
                        json!({ "name": format!("parallel-{idx}") }),
                    )
                    .await;
                }));
            }

            for join in joins {
                join.await;
            }

            let final_name =
                dispatch_hostcall_session("parallel-final", &manager, "get_name", json!({})).await;
            assert!(
                matches!(final_name, HostcallOutcome::Success(_)),
                "final get_name should succeed after concurrent writes, got: {final_name:?}"
            );
        });
    }

    /// Session hostcall operations exercise a real SessionHandle, not MockSession.
    #[test]
    fn session_ops_with_real_session_handle_roundtrip() {
        asupersync::test_utils::run_test(|| async move {
            let manager = ExtensionManager::new();
            let session = Arc::new(crate::session::SessionHandle(Arc::new(
                asupersync::sync::Mutex::new(crate::session::Session::create()),
            )));
            manager.set_session(session.clone());

            let set_name = dispatch_hostcall_session(
                "real-session",
                &manager,
                "set_name",
                json!({ "name": "real-session-name" }),
            )
            .await;
            assert!(
                matches!(set_name, HostcallOutcome::Success(_)),
                "set_name should succeed, got: {set_name:?}"
            );

            let get_name =
                dispatch_hostcall_session("real-session", &manager, "get_name", json!({})).await;
            if let HostcallOutcome::Success(value) = get_name {
                assert_eq!(value.as_str(), Some("real-session-name"));
            } else {
                panic!();
            }

            let set_model = dispatch_hostcall_session(
                "real-session",
                &manager,
                "set_model",
                json!({ "provider": "prov-real", "modelId": "model-real" }),
            )
            .await;
            assert!(
                matches!(set_model, HostcallOutcome::Success(_)),
                "set_model should succeed, got: {set_model:?}"
            );

            let get_model =
                dispatch_hostcall_session("real-session", &manager, "get_model", json!({})).await;
            if let HostcallOutcome::Success(value) = get_model {
                assert_eq!(
                    value.get("provider").and_then(Value::as_str),
                    Some("prov-real")
                );
                assert_eq!(
                    value.get("modelId").and_then(Value::as_str),
                    Some("model-real")
                );
            } else {
                panic!();
            }

            let append_entry = dispatch_hostcall_session(
                "real-session",
                &manager,
                "append_entry",
                json!({
                    "customType": "marker",
                    "data": { "kind": "real-session-check", "value": 42 }
                }),
            )
            .await;
            assert!(
                matches!(append_entry, HostcallOutcome::Success(_)),
                "append_entry should succeed, got: {append_entry:?}"
            );

            let state =
                dispatch_hostcall_session("real-session", &manager, "get_state", json!({})).await;
            if let HostcallOutcome::Success(value) = state {
                assert_eq!(
                    value.get("sessionName").and_then(Value::as_str),
                    Some("real-session-name")
                );
                assert_eq!(
                    value.get("thinkingLevel").and_then(Value::as_str),
                    Some("off")
                );
            } else {
                panic!();
            }
        });
    }
}
