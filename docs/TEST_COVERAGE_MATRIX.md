## Test Coverage Matrix (Current Source Inventory)

> Last regenerated: 2026-08-03
> Owner bead: `bd-8t27h.1`

This document is the current source-file coverage inventory for `src/**/*.rs`. It is not a drop-in certification artifact and does not override `docs/evidence/dropin-certification-verdict.json`.

### Regeneration Evidence

- `rg --files src -g '*.rs' | sort` -> 121 current source files.
- `rg --files tests -g '*.rs' | wc -l` -> 327 Rust test files under `tests/`.
- `rg -n '#\\[cfg\\(test\\)|mod tests' src -g '*.rs'` -> in-source unit-test inventory used for the `Unit` status below.
- `python3 scripts/check_traceability_matrix.py` passes with 302/302 classified tests traced (100.00%) and 39/39 classified E2E suites covered (100.00%).
- `docs/coverage-baseline-map.json` is historical coverage evidence from 2026-02-14 and covers 107 source files; this markdown inventory reflects the 121-file current tree.
- Drift guard: `cargo test --test traceability_staleness source_coverage_matrix_matches_current_src_inventory`.

### Current Drift Check

- Current `src/` inventory: 121 files.
- Source-file rows below: 121.
- Source files omitted from this document: 0.
- Split modules, provider expansion modules, hostcall scheduling/queue modules, PiWasm, session v2/SQLite, resources, resource governor, and scheduler/admission surfaces are represented explicitly and linked through the `resource_scheduler_admission` artifact-inventory lane.
- Machine-readable traceability remains governed by `docs/traceability_matrix.json`, `tests/suite_classification.toml`, `docs/e2e_scenario_matrix.json`, and `scripts/check_traceability_matrix.py`.

### Legend

- **Unit**: in-source `#[cfg(test)]` or `mod tests` exists in the source file.
- **Integration/E2E/Conformance**: representative test files or governed artifacts that exercise the surface.
- **Waived glue**: re-export or test-support module; represented here so it cannot disappear from the matrix silently.
- **Gap owner**: active bead that owns a known weakness in the row.

---

## 1) Source-File Coverage Matrix

| Source file | Area | Coverage evidence / status |
|---|---|---|
| `src/acp.rs` | ACP protocol | Unit; `tests/sdk_api.rs`, `tests/sdk_integration.rs`, `tests/sdk_unit.rs`. |
| `src/agent.rs` | Agent loop | Unit; `tests/agent_loop_vcr.rs`, `tests/agent_loop_reliability.rs`, `tests/e2e_agent_loop.rs`, `tests/rpc_mode.rs`. |
| `src/agent_cx.rs` | Agent context | Unit; covered through agent/RPC suites. |
| `src/app.rs` | App orchestration | Unit; `tests/e2e_cli.rs`, `tests/e2e_rpc.rs`, `tests/main_cli_selection.rs`. |
| `src/auth.rs` | Auth and OAuth | Unit; `tests/auth_oauth_refresh_vcr.rs`, `tests/extensions_provider_oauth.rs`. |
| `src/autocomplete.rs` | Prompt autocomplete | Unit; interactive coverage via `tests/tui_state.rs`. |
| `src/bin/pi_legacy_capture.rs` | Legacy capture utility | Unit; opt-in capture utility, not a default user path. |
| `src/buffer_shim.rs` | Node buffer shim | `tests/node_buffer_shim.rs`; branch export baseline marks this as branch-SIGSEGV fallback. |
| `src/cli.rs` | CLI parsing | Unit; `tests/main_cli_selection.rs`, `tests/cli_edge_cases.rs`, `tests/e2e_cli.rs`. |
| `src/compaction.rs` | Session compaction | Unit; `tests/compaction.rs`, `tests/compaction_bug.rs`. |
| `src/compaction_worker.rs` | Compaction worker | Unit; exercised by compaction suites. |
| `src/config.rs` | Config loading | Unit; `tests/config_precedence.rs`, `tests/config_edge_cases.rs`. |
| `src/conformance.rs` | Conformance runner | Unit; `tests/conformance_*.rs`, `tests/tools_conformance.rs`. |
| `src/conformance_shapes.rs` | Conformance schemas | Unit; `tests/ext_conformance_shapes.rs`. |
| `src/connectors/http.rs` | HTTP connector | `tests/pi_connector_shims.rs`; connector coverage still needs machine-readable trace expansion under `bd-8t27h.3`. |
| `src/connectors/mod.rs` | Connector registry | Unit; `tests/rpc_session_connector.rs`, `tests/pi_connector_shims.rs`. |
| `src/crypto_shim.rs` | Node crypto shim | Unit; `tests/node_crypto_shim.rs`. |
| `src/doctor.rs` | Doctor and diagnostics | Unit; `tests/doctor_swarm_temp_dir_json.rs`, `tests/franken_node_compatibility_doctor_contract.rs`. |
| `src/error.rs` | Error types | Unit; `tests/error_types.rs`, `tests/error_handling.rs`. |
| `src/error_hints.rs` | Error remediation hints | Unit; `tests/error_handling.rs`. |
| `src/extension_conformance_matrix.rs` | Extension matrix | Unit; `tests/ext_conformance_matrix.rs`. |
| `src/extension_dispatcher.rs` | Extension dispatcher | Unit; `tests/event_dispatch_latency.rs`, `tests/extensions_event_wiring.rs`, `tests/extensions_event_cancellation.rs`; timing ignored test owner `bd-8t27h.11`. |
| `src/extension_events.rs` | Extension events | Unit; `tests/extensions_event_wiring.rs`, `tests/extensions_event_cancellation.rs`, `tests/extensions_repair_events.rs`. |
| `src/extension_inclusion.rs` | Extension inclusion list | Unit; `tests/ext_inclusion_list.rs`. |
| `src/extension_index.rs` | Extension index/search | Unit; `tests/ext_entry_scan.rs`, `tests/extension_code_search.rs`. |
| `src/extension_license.rs` | Extension license audit | Unit; `tests/extension_license.rs`; report evidence in `docs/extension-license-report.json`. |
| `src/extension_popularity.rs` | Extension popularity scoring | Unit; coverage is mostly artifact/report oriented and should be traced in `bd-8t27h.3`. |
| `src/extension_preflight.rs` | Extension preflight | Unit; `tests/ext_preflight_analyzer.rs`, `tests/e2e_workflow_preflight.rs`. |
| `src/extension_replay.rs` | Extension replay | Unit; `tests/e2e_replay_bundle_validation.rs`, `tests/e2e_replay_bundles.rs`. |
| `src/extension_scoring.rs` | Extension scoring | Unit; `tests/extension_scoring.rs`, `tests/extension_scoring_ope.rs`, `tests/extension_scoring_voi_meanfield.rs`. |
| `src/extension_tools.rs` | Extension tools | Unit; `tests/e2e_extension_registration.rs`; branch export baseline marks this as branch-SIGSEGV fallback. |
| `src/extension_validation.rs` | Extension validation | Unit; `tests/extension_validation.rs`, `tests/extension_lockfile_provenance.rs`, `tests/ext_provenance_verification.rs`. |
| `src/extensions.rs` | Extension protocol/runtime | Unit; `tests/extensions_*.rs`, `tests/ext_conformance*.rs`, `tests/e2e_extension_registration.rs`. |
| `src/extensions/compatibility.rs` | Extension compatibility contracts and scanner | Unit; compatibility scanner tests in this module plus `tests/ext_entry_scan.rs` and extension conformance suites. |
| `src/extensions_js.rs` | QuickJS bridge | Unit; `tests/event_loop_conformance.rs`, `tests/js_runtime_ordering.rs`, `tests/node_*_shim.rs`, `tests/e2e_ts_extension_loading.rs`. |
| `src/file_lock.rs` | Cross-process directory locking | Unit; session-index lock integration coverage in `tests/session_index_tests.rs` and RPC concurrency coverage in `tests/e2e_rpc.rs`. |
| `src/flake_classifier.rs` | Flake classifier | Unit; patterns are mirrored by `scripts/ci_conformance_retry.sh`. |
| `src/hostcall_amac.rs` | Hostcall AMAC | Unit; `tests/streaming_hostcall.rs`. |
| `src/hostcall_io_uring_lane.rs` | Hostcall io_uring lane | Unit; `tests/streaming_hostcall.rs`. |
| `src/hostcall_queue.rs` | Hostcall queue | Unit; `tests/hostcall_queue_ebr.rs`, `tests/hostcall_queue_loom.rs`; loom opt-in owner `bd-8t27h.6`. |
| `src/hostcall_rewrite.rs` | Hostcall rewrite | Unit; `tests/streaming_hostcall.rs`. |
| `src/hostcall_s3_fifo.rs` | Hostcall S3 FIFO | Unit; `tests/hostcall_s3_fifo_policy.rs`. |
| `src/hostcall_superinstructions.rs` | Hostcall superinstructions | Unit; `tests/streaming_hostcall.rs`. |
| `src/hostcall_trace_jit.rs` | Hostcall trace JIT | Unit; `tests/streaming_hostcall.rs`. |
| `src/http/client.rs` | HTTP client | Unit; `tests/http_client.rs`; branch export baseline marks `src/http/*.rs` as branch-SIGSEGV fallback. |
| `src/http/mod.rs` | HTTP module glue | Waived glue: re-export/test-module wiring. |
| `src/http/sse.rs` | HTTP SSE | Unit; `tests/repro_sse_flush.rs`. |
| `src/http/test_api.rs` | HTTP test support | Waived test-only support module; compiled only for tests. |
| `src/http/test_asupersync.rs` | HTTP test support | Waived test-only support module; compiled only for tests. |
| `src/http_shim.rs` | Node HTTP shim | `tests/node_http_shim.rs`; branch export baseline marks this as branch-SIGSEGV fallback. |
| `src/interactive.rs` | TUI root | Unit test module wiring; `tests/tui_snapshot.rs`, `tests/tui_state.rs`, `tests/e2e_tui.rs`. |
| `src/interactive/agent.rs` | TUI agent lane | Unit; `tests/e2e_tui.rs`, `tests/tui_state.rs`. |
| `src/interactive/commands.rs` | Interactive commands | Unit; `tests/interactive_commands_unit.rs`, `tests/interactive_extension_ui.rs`. |
| `src/interactive/conversation.rs` | Conversation model | Unit; `tests/tui_state.rs`. |
| `src/interactive/ext_session.rs` | Extension session UI | Unit; `tests/interactive_extension_ui.rs`; branch export baseline marks this as branch-SIGSEGV fallback. |
| `src/interactive/file_refs.rs` | File references | Unit; `tests/tui_state.rs`. |
| `src/interactive/keybindings.rs` | Interactive keybindings | Unit; `tests/tui_state.rs`. |
| `src/interactive/model_selector_ui.rs` | Model selector UI | Unit; `tests/model_selector_cycling.rs`, `tests/tui_state.rs`. |
| `src/interactive/perf.rs` | TUI performance telemetry | Unit; `tests/e2e_tui_perf.rs`, `tests/perf_regression.rs`. |
| `src/interactive/share.rs` | Share/export UI | Unit; exercised through interactive state and command tests. |
| `src/interactive/state.rs` | Interactive state | Unit; `tests/tui_state.rs`. |
| `src/interactive/tests.rs` | Interactive test module | Waived test-only module included by `src/interactive.rs`. |
| `src/interactive/text_utils.rs` | Text utilities | Covered through interactive state/view tests; direct unit row should be added if this grows. |
| `src/interactive/tool_render.rs` | Tool rendering | Unit; `tests/tui_snapshot.rs`, `tests/tui_state.rs`. |
| `src/interactive/tree.rs` | Conversation tree | Covered through `tests/tui_state.rs` and session/navigation tests; direct trace should be expanded in `bd-8t27h.3`. |
| `src/interactive/tree_ui.rs` | Tree UI | Covered through `tests/tui_snapshot.rs` and `tests/tui_state.rs`. |
| `src/interactive/view.rs` | View rendering | Unit; `tests/tui_snapshot.rs`, `tests/e2e_tui.rs`. |
| `src/keybindings.rs` | Keybinding config | Unit; interactive/TUI tests. |
| `src/lib.rs` | Crate exports | Waived glue: exported module surface is compiled by all targets; no behavior-only row. |
| `src/main.rs` | CLI entry | Unit; `tests/e2e_cli.rs`, `tests/e2e_rpc.rs`, `tests/main_cli_selection.rs`; branch export baseline marks this as branch-SIGSEGV fallback. |
| `src/migrations.rs` | Migrations | Unit; SQLite/session migration coverage through `tests/session_sqlite.rs`. |
| `src/model.rs` | Message/content model | Unit; `tests/model_serialization.rs`. |
| `src/model_routing.rs` | Model-routing policy and evidence | Unit; model-routing tests in this module plus `tests/model_selector_cycling.rs`. |
| `src/model_selector.rs` | Model selector | Unit; `tests/model_selector_cycling.rs`. |
| `src/models.rs` | Model registry | Unit; `tests/model_registry.rs`. |
| `src/package_manager.rs` | Package manager | Unit; `tests/package_manager.rs`, `tests/e2e_cli.rs`. |
| `src/perf_build.rs` | Perf build metadata | Unit; `tests/perf_bench_harness.rs`, `tests/perf_budgets.rs`, `tests/perf_regression.rs`. |
| `src/permissions.rs` | Capability permissions | Unit; `tests/capability_policy_model.rs`, `tests/capability_policy_scoped.rs`, `tests/capability_denial_matrix.rs`. |
| `src/pi_wasm.rs` | PiWasm runtime | Unit; `tests/lab_runtime_extensions.rs`; unsupported imports fail closed, with bounded Emscripten compatibility stubs covered by source tests. |
| `src/platform.rs` | Platform helpers | Unit. |
| `src/provider.rs` | Provider trait/schema | Unit; `tests/provider_factory.rs`, `tests/provider_contract.rs`, `tests/provider_native_contract.rs`. |
| `src/provider_metadata.rs` | Provider metadata | Unit; `tests/provider_metadata_comprehensive.rs`, `tests/provider_registry_guardrails.rs`. |
| `src/providers/anthropic.rs` | Anthropic provider | Unit; `tests/provider_streaming/anthropic.rs`, `tests/e2e_provider_streaming.rs`. |
| `src/providers/azure.rs` | Azure provider | Unit; `tests/provider_streaming/azure.rs`, provider error/path suites. |
| `src/providers/bedrock.rs` | Bedrock provider | Unit; provider native/contract suites. |
| `src/providers/cohere.rs` | Cohere provider | Unit; `tests/provider_streaming/cohere.rs`, provider error/path suites. |
| `src/providers/copilot.rs` | Copilot provider | Unit; provider native/contract suites. |
| `src/providers/cursor.rs` | Cursor Connect provider | Unit; `tests/provider_smoke_matrix.rs`, `tests/provider_native_contract.rs`, and provider factory suites. |
| `src/providers/gemini.rs` | Gemini provider | Unit; `tests/provider_streaming/gemini.rs`, provider error/path suites. |
| `src/providers/gitlab.rs` | GitLab Duo provider | Unit; provider native/contract suites. |
| `src/providers/model_fetch.rs` | Live provider-model discovery and cache | Unit tests in this module; static-registry integration through `tests/model_registry.rs`. |
| `src/providers/mod.rs` | Provider factory | Unit; `tests/provider_factory.rs`, `tests/provider_native_verify.rs`; branch export baseline marks this family partly branch-SIGSEGV fallback. |
| `src/providers/openai.rs` | OpenAI chat provider | Unit; `tests/provider_streaming/openai.rs`, provider error/path suites. |
| `src/providers/openai_responses.rs` | OpenAI Responses provider | Unit; `tests/provider_streaming/openai_responses.rs`, provider error/path suites. |
| `src/providers/vertex.rs` | Vertex provider | Unit; provider native/contract suites. |
| `src/resource_governor.rs` | Resource governor | Unit; `tests/cargo_headroom_admission.rs`, `tests/resource_edge_cases.rs`; traceability lane `resource_scheduler_admission`. |
| `src/resources.rs` | Resource loading | Unit; `tests/resource_loader.rs`, `tests/resource_edge_cases.rs`; traceability lane `resource_scheduler_admission`. |
| `src/rpc.rs` | RPC/stdin mode | Unit; `tests/rpc_mode.rs`, `tests/rpc_protocol.rs`, `tests/rpc_edge_cases.rs`, `tests/e2e_rpc.rs`. |
| `src/scheduler.rs` | Scheduler/admission | Unit; `tests/scheduler_repro.rs`, `tests/cargo_headroom_admission.rs`; traceability lane `resource_scheduler_admission`. |
| `src/sdk.rs` | SDK API | Unit; `tests/sdk_api.rs`, `tests/sdk_integration.rs`, `tests/sdk_unit.rs`. |
| `src/semantic_workspace_graph.rs` | Semantic workspace graph and context bundles | Unit; `tests/semantic_workspace_graph_contract.rs`, `tests/semantic_workspace_graph_builder.rs`, and agent integration tests. |
| `src/session.rs` | Session JSONL/tree | Unit; `tests/session_conformance.rs`, `tests/e2e_session_persistence.rs`; branch export baseline marks this as branch-SIGSEGV fallback. |
| `src/session_index.rs` | Session index | Unit; `tests/session_index_tests.rs`, `tests/reproduce_index_gap.rs`. |
| `src/session_metrics.rs` | Session metrics | Unit; `tests/provider_session_coverage.rs` and session evidence suites. |
| `src/session_picker.rs` | Session picker UI | Unit; `tests/session_picker.rs`. |
| `src/session_sqlite.rs` | SQLite session backend | Unit; `tests/session_sqlite.rs`, `tests/fault_injection_persistence.rs`; branch export baseline marks this as branch-SIGSEGV fallback. |
| `src/session_store_v2.rs` | Session store v2 | Unit; `tests/session_store_v2.rs`, `tests/session_store_v2_contract.rs`. |
| `src/session_test.rs` | Session test helpers | Waived test-support module; compiled by session tests. |
| `src/sse.rs` | SSE parser | Unit; `tests/sse_strict_compliance.rs`, `tests/repro_sse_flush.rs`, `tests/repro_sse_newline.rs`. |
| `src/subagents.rs` | Native isolated child-agent tool | Unit tests in this module; opt-in registration coverage through built-in tool tests. |
| `src/swarm_activity_ledger.rs` | Swarm activity ledger | Unit; evidence docs in `docs/swarm-activity-ledger.md`, CI evidence bundle tests. |
| `src/swarm_flight_recorder.rs` | Swarm flight recorder | Unit and E2E; `tests/e2e_swarm_flight_recorder.rs` covers deterministic multi-agent replay artifacts. |
| `src/swarm_progress_slo.rs` | Swarm progress SLO evaluation | Unit; `tests/swarm_progress_slo_contract.rs`, `tests/swarm_progress_cli.rs`, and `tests/swarm_progress_slo_e2e.rs`. |
| `src/swarm_replay.rs` | Swarm trace replay and policy comparison | Unit; `tests/swarm_replay_trace_contract.rs`, `tests/swarm_replay_ingestor.rs`, and `tests/swarm_replay_preview_cli.rs`. |
| `src/terminal_images.rs` | Terminal images | Unit; interactive/TUI rendering tests. |
| `src/theme.rs` | Theme loading | Unit; `tests/tui_snapshot.rs`, interactive UI tests. |
| `src/tools.rs` | Built-in tools | Unit; `tests/tools_conformance.rs`, `tests/e2e_tools.rs`, `tests/tools_hardened.rs`; branch export baseline marks this as branch-SIGSEGV fallback. |
| `src/tui.rs` | Terminal renderer | Unit; `tests/tui_snapshot.rs`, `tests/tui_state.rs`, `tests/e2e_tui.rs`. |
| `src/validation_broker.rs` | Validation admission and slot broker | Unit; `tests/validation_broker_contract.rs`, `tests/validation_broker_store.rs`, `tests/validation_broker_cli.rs`, and `tests/validation_broker_e2e.rs`. |
| `src/vcr.rs` | VCR playback/record | Unit; `tests/vcr_parity_validation.rs`, `tests/vcr_redaction_scan.rs`, provider/RPC VCR suites. |
| `src/version_check.rs` | Version checks | Unit; cross-platform and release-readiness tests exercise the surrounding behavior. |

---

## 2) Test Suite Inventory Pointer

The full Rust test inventory is too large for this markdown table to remain the source of truth. Current counts on 2026-08-03:

| Inventory | Count | Source of truth |
|---|---:|---|
| Source files | 121 | `rg --files src -g '*.rs' | sort` |
| Rust test files | 327 | `rg --files tests -g '*.rs' | sort` |
| Classified top-level test files | 302 | `tests/suite_classification.toml` |
| Traceability-matrix referenced classified tests | 302 | `docs/traceability_matrix.json` via `scripts/check_traceability_matrix.py` |
| Classified-but-untraced tests | 0 | `scripts/check_traceability_matrix.py` |

Representative high-signal suites:

| Suite family | Representative files | Main surfaces |
|---|---|---|
| Agent/RPC/CLI | `tests/agent_loop_vcr.rs`, `tests/e2e_agent_loop.rs`, `tests/rpc_mode.rs`, `tests/e2e_cli.rs`, `tests/e2e_rpc.rs` | `agent`, `app`, `main`, `rpc`, CLI selection |
| Providers | `tests/provider_streaming/*.rs`, `tests/provider_factory.rs`, `tests/provider_native_contract.rs`, `tests/provider_metadata_comprehensive.rs` | Native provider modules, metadata, factory routing |
| Extensions | `tests/ext_conformance*.rs`, `tests/extensions_*.rs`, `tests/e2e_extension_registration.rs` | Extension protocol, QuickJS bridge, policy, conformance |
| TUI | `tests/tui_snapshot.rs`, `tests/tui_state.rs`, `tests/e2e_tui.rs`, `tests/e2e_tui_features.rs`, `tests/e2e_tui_perf.rs` | Interactive state, view, rendering, keybindings |
| Sessions | `tests/session_conformance.rs`, `tests/session_index_tests.rs`, `tests/session_sqlite.rs`, `tests/session_store_v2.rs`, `tests/e2e_session_persistence.rs` | JSONL/tree/index/sqlite/store v2 persistence |
| Tools | `tests/tools_conformance.rs`, `tests/e2e_tools.rs`, `tests/tools_hardened.rs` | Built-in tools and tool I/O contracts |
| Resources/scheduler | `tests/resource_loader.rs`, `tests/resource_edge_cases.rs`, `tests/scheduler_repro.rs`, `tests/cargo_headroom_admission.rs` | Resources, resource governor, scheduler/admission |
| Shims | `tests/node_buffer_shim.rs`, `tests/node_crypto_shim.rs`, `tests/node_http_shim.rs`, `tests/node_fs_shim.rs`, `tests/node_child_process_shim.rs` | Node compatibility shims |

---

## 3) Mock / Fake / Stub Audit

This matrix does not certify no-mock compliance. Use the dedicated guards and docs:

- `tests/non_mock_compliance_gate.rs`
- `tests/non_mock_rubric_gate.rs`
- `.github/workflows/ci.yml` no-mock guard steps
- `docs/non-mock-rubric.json`

Known allowlisted test doubles remain local to test harnesses and are not release-path substitutes: `tests/common/harness.rs` local TCP `MockHttpServer`, package command stubs in CLI E2E tests, and recording host/session harnesses for extension message-session tests.

---

## 4) JSONL / Artifact Coverage

Structured evidence locations are governed outside this markdown file:

- `docs/traceability_matrix.json`
- `docs/e2e_scenario_matrix.json`
- `tests/e2e_results/*`
- `tests/ext_conformance/reports/*`
- `tests/perf/reports/*`

JSONL artifact-inventory expansion was completed under `bd-8t27h.9`; the current high-value inventory is enforced by `tests/traceability_staleness.rs` and `scripts/check_traceability_matrix.py`.

---

## 5) Completed Follow-up Work From the 2026-05 Refresh

| Bead | Scope |
|---|---|
| `bd-8t27h.2` | Added the deterministic source coverage drift guard that detected this refresh. |
| `bd-8t27h.3` | Repair traceability matrix, suite classification, evidence logs, and E2E scenario coverage. |
| `bd-8t27h.5` | Replace macOS ignored extension OAuth MockHttpServer tests. |
| `bd-8t27h.6` | Make hostcall queue loom tests runnable behind explicit opt-in cfg/profile. |
| `bd-8t27h.9` | Expand JSONL artifact inventory for high-value suites. |
| `bd-8t27h.10` | Determinize E2E golden corpus dynamic cassette path. |
| `bd-8t27h.11` | Move extension dispatcher timing regression away from wall-clock flake. |
| `bd-8t27h.12` | Normalize manual perf/report generators to tmpdir-aware smoke tests. |
| `bd-8t27h.13` | Document and test PiWasm unsupported import fail-closed policy. |
| `bd-8t27h.16` | Bound extension random trials into deterministic smoke lane. |
| `bd-8t27h.17` | Onboard unvendored extension conformance corpus. |
| `bd-8t27h.18` | Normalize npm registry conformance diff ignore. |

---

## 6) Running Extension Conformance Tests

```bash
# Generated per-extension registration tests, tiers 1-2 by default.
cargo test --test ext_conformance_generated --features ext-conformance

# Owner-tracked opt-in lane for generated tier 3-5 extension tests.
cargo test --test ext_conformance_generated --features ext-conformance -- --include-ignored

# Differential TypeScript/Rust oracle.
cargo test --test ext_conformance_diff --features ext-conformance

# Official extensions only, bounded.
PI_OFFICIAL_MAX=5 cargo test --test ext_conformance_diff --features ext-conformance

# Scenario execution tests.
cargo test --test ext_conformance_scenarios --features ext-conformance

# Default conformance-related tests.
cargo test conformance
cargo test extensions_policy_negative
```

---

## 7) Coverage Tooling

Coverage reports are generated with `cargo-llvm-cov` as described in `README.md`.

Current historical baseline evidence lives in `docs/coverage-baseline-map.json`:

| Metric | Value |
|---|---:|
| Generated at | 2026-02-14T14:00:00Z |
| Source files in baseline | 107 |
| Source files at baseline comparison refresh | 110 |
| Line coverage | 79.08% |
| Branch coverage | 51.95% lower bound |
| Function coverage | 78.01% |
| Branch-measurable files | 63 |
| Branch export SIGSEGV fallback files | 44 |

That baseline is useful historical evidence, but it is not a current-head full source inventory. This document supplies the current 121-file inventory, enforced by the drift guard completed under `bd-8t27h.2`.
