## Test Coverage Matrix (Current Source Inventory)

> Last regenerated: 2026-10-24
> Owner bead: `bd-8t27h.1`

This document is the current source-file coverage inventory for `src/**/*.rs`. It is not a drop-in certification artifact and does not override `docs/evidence/dropin-certification-verdict.json`.

### Regeneration Evidence

- `rg --files src -g '*.rs' | sort` -> 676 current source files.
- `rg --files tests -g '*.rs' | wc -l` -> 457 Rust test files under `tests/`.
- `rg -n '#\[cfg\(test\)|mod tests' src -g '*.rs'` -> in-source unit-test inventory used for the `Unit` status below.
- `python3 scripts/check_traceability_matrix.py` passes with 431/431 classified tests traced (100.00%) and 56/56 classified E2E suites covered (100.00%).
- `docs/coverage-baseline-map.json` is historical coverage evidence from 2026-02-14 and covers 107 source files; this markdown inventory reflects the 676-file current tree.
- Drift guard: `cargo test --test traceability_staleness source_coverage_matrix_matches_current_src_inventory`.

### Current Drift Check

- Latest recorded full `src/` inventory: 676 files (2026-10-24), with no missing or stale rows.
- Source-file rows below: 676, one per file in the current `src/` inventory.
- The 224 rows added on 2026-10-24 state only what each file shows: its own `//!` summary, its `#[test]` count, and the test modules it declares. Their integration-suite links were not audited, and a listed test count is not a passing result.
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
| `src/acp/content.rs` | ACP content conversion at the editor/agent boundary: bounded prompt blocks, embedded resources kept as labeled reference content | Unit (18 in-module tests). |
| `src/acp/history.rs` | ACP session recovery and transcript replay from the configured session directory, without running an agent turn | Unit (18 in-module tests). |
| `src/acp/mcp.rs` | Session-owned MCP tools for ACP: inert setup, with trust admission and connection inside the reserved prompt task | Unit (9 in-module tests); `src/acp/mcp/integration_tests.rs`. |
| `src/acp/mcp/integration_tests.rs` | ACP MCP integration test suite | Test module; loopback MCP transports and a local scripted provider through the production permission callback (4 tests). |
| `src/acp/recovery_tests.rs` | ACP turn recovery test suite | Test module; real configured session, loopback provider HTTP/SSE transport, durable recovery driver and editor notifications (18 tests). |
| `src/acp/steering.rs` | Turn-owned ACP input | Unit (4 in-module tests). |
| `src/advisor.rs` | Advisor turns | `tests/e2e_advisor.rs` (concern injection, failure isolation). |
| `src/agent.rs` | Agent loop | Unit; `tests/agent_loop_vcr.rs`, `tests/agent_loop_reliability.rs`, `tests/e2e_agent_loop.rs`, `tests/rpc_mode.rs`. |
| `src/agent_cx.rs` | Agent context | Unit; covered through agent/RPC suites. |
| `src/agent_cx/http.rs` | Agent context HTTP | Unit; covered through agent/RPC suites. |
| `src/agent_cx/process.rs` | Agent context process execution | Unit; covered through agent/RPC suites. |
| `src/agent_cx/process/capture.rs` | Agent context process output capture | Unit; in-module tests. |
| `src/agent_hub.rs` | Agent hub registry (bd-cv653.5.3): session-scoped roster of spawned child agents | Unit (7 tests); `tests/agent_hub.rs`. |
| `src/agent_hub/ipc.rs` | Bounded, authenticated local channel from one native child agent to its parent hub: roster, own inbox, parent and peer messages | Unit (8 in-module tests). |
| `src/agent_hub/ipc/results.rs` | Read-only result access through a native child's existing owned channel | `src/agent_hub/ipc/results/tests.rs` (10 tests). |
| `src/agent_hub/ipc/results/stream.rs` | Connection-owned result snapshots and bounded wire framing | `src/agent_hub/ipc/results/stream/tests.rs` (9 tests). |
| `src/agent_hub/ipc/results/stream/tests.rs` | Test suite for `src/agent_hub/ipc/results/stream.rs` | Test module (9 tests). |
| `src/agent_hub/ipc/results/tests.rs` | Test suite for `src/agent_hub/ipc/results.rs` | Test module (10 tests). |
| `src/agent_hub/wait.rs` | Bounded reply waits over the same inbox and authenticated child channel as ordinary hub messages | `src/agent_hub/wait/tests.rs` (24 tests). |
| `src/agent_hub/wait/tests.rs` | Test suite for `src/agent_hub/wait.rs` | Test module (24 tests). |
| `src/agent_tests.rs` | Test suite for `src/agent.rs` | Test module (163 tests). |
| `src/app.rs` | App orchestration | Unit; `tests/e2e_cli.rs`, `tests/e2e_rpc.rs`, `tests/main_cli_selection.rs`. |
| `src/approval.rs` | Tool approval flow | `tests/e2e_plan_mode.rs`, `tests/e2e_rpc.rs` approval paths. |
| `src/approval/scope.rs` | Approval plan file scoping | Unit; in-module unit tests; `tests/e2e_plan_mode.rs`. |
| `src/archive_read.rs` | Bounded, read-only navigation of ZIP archives and ZIP-based packages | Unit (10 in-module tests). |
| `src/artifact_output.rs` | Artifact output formatting | Unit; covered through agent/tools suites. |
| `src/ask.rs` | Ask prompts | Interactive coverage via `tests/tui_state.rs` and RPC suites. |
| `src/ast_tools.rs` | AST tools | `tests/ast_tools.rs`, `tests/e2e_ast_tools.rs`. |
| `src/auth.rs` | Auth and OAuth | Unit; `tests/auth_oauth_refresh_vcr.rs`, `tests/extensions_provider_oauth.rs`. |
| `src/autocomplete.rs` | Prompt autocomplete | Unit; interactive coverage via `tests/tui_state.rs`. |
| `src/bash_mediation.rs` | Bash mediation | `tests/bash_mediation.rs`. |
| `src/bash_mediation/dcg.rs` | Bash deterministic command generator grammar and constraints | Unit; `tests/bash_mediation.rs`. |
| `src/bin/pi_legacy_capture.rs` | Legacy capture utility | Unit; opt-in capture utility, not a default user path. |
| `src/bin/pi_mcp_fixture.rs` | MCP test fixture binary | Waived glue; test-support binary driven by `tests/mcp.rs`. |
| `src/bpe.rs` | Vendored tiktoken BPE core (bd-w8q6u): rank tables loaded from gzip assets for token counting | Unit (2 tests); exercised through `src/token_count.rs` and its tests. |
| `src/browser.rs` | Opt-in headless Chromium automation tool via CDP attach (bd-cv653.2.4) | `tests/browser.rs`, `tests/cross_surface_parity.rs`. |
| `src/browser/cdp.rs` | Browser CDP protocol transport | Unit; `tests/e2e_browser.rs`, `tests/browser_cdp.rs`. |
| `src/browser/cdp/contexts.rs` | Browser-context ownership for native download policy and recovery | Unit (5 in-module tests). |
| `src/browser/cdp/diagnostics.rs` | Request-owned browser diagnostics, collected by the ordinary CDP receiver | Unit (8 in-module tests). |
| `src/browser/cdp/diagnostics/network.rs` | Bounded request-hop summaries | Unit (8 in-module tests). |
| `src/browser/dialog.rs` | Browser JavaScript dialog handling and response dispatch | Unit; `src/browser/dialog/tests.rs`, `tests/browser.rs`. |
| `src/browser/dialog/tests.rs` | Browser JavaScript dialog handling test suite | Test module; dialog handling tests. |
| `src/browser/download.rs` | Browser download lifecycle and management | Unit; `tests/e2e_browser.rs`. |
| `src/browser/exports.rs` | Browser module exports and public surface | Waived glue; re-exports for browser submodules. |
| `src/browser/frames.rs` | Explicit, per-operation frame selection for the native browser backend | Unit (8 in-module tests); `tests/browser_frames.rs` (preflight; live Chromium lane is opt-in). |
| `src/browser/frames/pointer.rs` | Projection of renderer-local pointer candidates through the selected iframe chain, with parent hit-testing before native input | Unit (6 in-module tests); `tests/browser_remote_pointer.rs` (live Chromium lane, opt-in). |
| `src/browser/frames/targets.rs` | Per-operation ownership of flattened CDP iframe sessions, admitted only by attach events from the selected page | Unit (4 in-module tests). |
| `src/browser/frames/tests_transport.rs` | Browser frame transport test suite | Test module; public browser tool over a loopback HTTP/WebSocket CDP peer (8 tests; validates CDP routing, not Chromium rendering or native input). |
| `src/browser/history.rs` | Native main-document history traversal with destination policy checks | Unit (4 in-module tests). |
| `src/browser/interaction.rs` | Browser interaction and navigation | Unit; `tests/e2e_browser.rs`. |
| `src/browser/interaction/ax_state.rs` | Bounded control-state projection from Chromium's accessibility response | Unit (12 in-module tests). |
| `src/browser/interaction/forms.rs` | Structured, verified form operations (option selection, checked state) on one pinned DOM node | Unit (9 in-module tests); `tests/browser_forms.rs` (preflight and loopback CDP wire cases; live Chromium lane is opt-in). |
| `src/browser/interaction/frame_input.rs` | Frame-local DOM lookup and verified page-viewport input coordinates | Unit (5 in-module tests); `tests/browser_frames.rs` live lane. |
| `src/browser/interaction/pointer.rs` | Native pointer sequences on one resolved node, never JavaScript clicks | Unit (12 in-module tests). |
| `src/browser/interaction/selectors.rs` | Semantic locators resolve to the same document-owned backend IDs as CSS | `src/browser/interaction/selectors/tests.rs` (10 tests). |
| `src/browser/interaction/selectors/tests.rs` | Test suite for `src/browser/interaction/selectors.rs` | Test module (10 tests). |
| `src/browser/interaction/text_input.rs` | One document/focus contract for main-page and frame-local text input | Unit (4 in-module tests). |
| `src/browser/interaction/upload.rs` | Browser file upload interaction | Unit; `tests/e2e_browser.rs`. |
| `src/browser/interaction/wait.rs` | Readiness observations on one pinned document, under the native call budget | `src/browser/interaction/wait/tests.rs` (9 tests). |
| `src/browser/interaction/wait/navigation.rs` | Explicit same-origin destination waits following one native input | `src/browser/interaction/wait/navigation/tests.rs` (7 tests). |
| `src/browser/interaction/wait/navigation/tests.rs` | Test suite for `src/browser/interaction/wait/navigation.rs` | Test module (7 tests). |
| `src/browser/interaction/wait/tests.rs` | Test suite for `src/browser/interaction/wait.rs` | Test module (9 tests). |
| `src/browser/launch.rs` | Browser process launch and attach | Unit; `tests/e2e_browser.rs`. |
| `src/browser/mock.rs` | Browser mock backend and fixtures | Unit; `tests/browser_mock.rs`. |
| `src/browser/policy.rs` | Browser security and access policy | Unit; `tests/browser_policy.rs`. |
| `src/browser/session.rs` | Conversation-owned browser namespaces over the registry's existing live scope | `src/browser/session/tests.rs` (11 tests). |
| `src/browser/session/batch.rs` | Explicit, fail-fast browser workflows under one conversation lease and budget | `src/browser/session/batch/tests.rs` (15 tests). |
| `src/browser/session/batch/tests.rs` | Test suite for `src/browser/session/batch.rs` | Test module (15 tests). |
| `src/browser/session/tests.rs` | Test suite for `src/browser/session.rs` | Test module (11 tests). |
| `src/browser/session/tests/native.rs` | Public BrowserTool workflows over real HTTP/WebSocket transport | Test module (6 tests). |
| `src/browser/session/tests/native/batch.rs` | Batch workflows reuse the existing bounded native HTTP/WebSocket peer | Test module (11 tests). |
| `src/browser/storage.rs` | Explicit, origin-scoped persistence for an owned browser's authentication state | Unit (16 in-module tests). |
| `src/browser/storage/restore.rs` | Restore explicit private state through the same page-bound native transport | Unit (8 in-module tests). |
| `src/btw.rs` | `/btw` ephemeral side questions (bd-cv653.3.16) | Unit (6 tests); `tests/btw_tan.rs`. |
| `src/buffer_shim.rs` | Node buffer shim | `tests/node_buffer_shim.rs`; branch export baseline marks this as branch-SIGSEGV fallback. |
| `src/checkpoint.rs` | Session checkpoints | `tests/checkpoint.rs` (checkpoint/rewind/fresh/retry). |
| `src/cli.rs` | CLI parsing | Unit; `tests/main_cli_selection.rs`, `tests/cli_edge_cases.rs`, `tests/e2e_cli.rs`. |
| `src/commit_split.rs` | Commit splitting | `tests/commit_split.rs`. |
| `src/compaction.rs` | Session compaction | Unit; `tests/compaction.rs`, `tests/compaction_bug.rs`. |
| `src/compaction_snap.rs` | Snapcompact compaction mode (bd-cv653.7.6) | Unit (6 tests); `tests/snapcompact.rs`, `tests/snapcompact_provider.rs`. |
| `src/compaction_snap/archive.rs` | Bounded, streaming construction of the optional visual history archive | Unit (9 in-module tests). |
| `src/compaction_snap/replay.rs` | Admit a complete persisted archive before cloning or attaching any frame | Unit (17 in-module tests). |
| `src/compaction_worker.rs` | Compaction worker | Unit; exercised by compaction suites. |
| `src/completions.rs` | Shell completions | CLI coverage via `tests/main_cli_selection.rs`, `tests/cli_edge_cases.rs`. |
| `src/computer.rs` | Opt-in desktop computer automation tool (bd-cv653.2.5) | `tests/computer.rs`, `tests/cross_surface_parity.rs`. |
| `src/computer/mock.rs` | Computer tool mock backend | Unit; `tests/computer_mock.rs`. |
| `src/computer/native.rs` | Computer tool native platform backend | Unit; `tests/computer_tool.rs`. |
| `src/computer/native/accessibility.rs` | Computer tool native accessibility tree | Unit; `tests/computer_tool.rs`. |
| `src/computer/native/input.rs` | Computer tool native input simulation | Unit; `tests/computer_tool.rs`. |
| `src/computer/native/macos.rs` | macOS desktop operations through a bundled literal Swift helper, with host-owned approval, capability, timeout and publication | Unit (18 in-module tests, one macOS-only). |
| `src/computer/process.rs` | Computer tool process supervision | Unit; `tests/computer_tool.rs`. |
| `src/computer/process/input.rs` | Owned desktop input transport: a single writer holds stdin as a lease and closes it before process teardown | Unit (4 in-module tests). |
| `src/config.rs` | Config loading | Unit; `tests/config_precedence.rs`, `tests/config_edge_cases.rs`. |
| `src/conformance.rs` | Conformance runner | Unit; `tests/conformance_*.rs`, `tests/tools_conformance.rs`. |
| `src/conformance_shapes.rs` | Conformance schemas | Unit; `tests/ext_conformance_shapes.rs`. |
| `src/connectors/http.rs` | HTTP connector | `tests/pi_connector_shims.rs`; connector coverage still needs machine-readable trace expansion under `bd-8t27h.3`. |
| `src/connectors/mod.rs` | Connector registry | Unit; `tests/rpc_session_connector.rs`, `tests/pi_connector_shims.rs`. |
| `src/context_files.rs` | Context file loading | Covered through agent/session suites and `tests/config_precedence.rs`. |
| `src/crash.rs` | Crash capture: redacted crash bundles from panics and fatal signals | Unit (6 tests); `tests/fault_injection_persistence.rs`, `tests/e2e_golden_path.rs`, `tests/adversarial_extensions.rs`, `tests/e2e_ftui.rs`. |
| `src/crypto_shim.rs` | Node crypto shim | Unit; `tests/node_crypto_shim.rs`. |
| `src/current_time.rs` | `current_time` tool, the shipped implementation since 82fd0468 (2026-09-02) routed the registry to this module; `src/tools.rs` still carries an older in-file `CurrentTimeTool` that nothing constructs (its own unit tests keep it compiling) pending the maintainer's decision | Unit tests in the module (offset rendering, snapshot fields); registry membership in `src/tools.rs` tests; `src/xdev.rs` one-liner drift test; CLI default-list goldens; `tests/readme_tool_inventory.rs`. |
| `src/debug.rs` | Debug (DAP) facade | `tests/debug.rs`. |
| `src/debug/adapter_configuration_tests.rs` | End-to-end native DebugTool calls against the deterministic DAP wire peer | Test module (8 tests). |
| `src/debug/adapters.rs` | DAP adapters | `tests/debug.rs`. |
| `src/debug/adapters/dotnet.rs` | Bounded recognition of managed PE images for the native .NET debugger | `src/debug/adapters/dotnet/tests.rs` (11 tests). |
| `src/debug/adapters/dotnet/tests.rs` | Structural image tests and native-tool protocol workflows | Test module (11 tests). |
| `src/debug/breakpoints.rs` | DAP breakpoint management | Unit; `tests/debug_tool.rs`. |
| `src/debug/codelldb_tests.rs` | CodeLLDB profile and public DebugTool regressions | Test module (7 tests). |
| `src/debug/configuration.rs` | Explicit, bounded workspace DAP configuration | Unit (8 in-module tests). |
| `src/debug/dap.rs` | DAP protocol | `tests/debug.rs`. |
| `src/debug/dap/connect.rs` | Explicit connections to pre-existing loopback DAP servers | Unit (5 in-module tests). |
| `src/debug/dap/delve.rs` | Delve DAP adapter transport | Unit; `tests/debug_delve.rs`. |
| `src/debug/dap/reliability.rs` | DAP connection reliability and timeout handling | Unit; `tests/debug_tool.rs`. |
| `src/debug/dap/reverse.rs` | Request-scoped reverse DAP service | `src/debug/dap/reverse/tests.rs` (5 tests). |
| `src/debug/dap/reverse/tests.rs` | Test suite for `src/debug/dap/reverse.rs` | Test module (5 tests). |
| `src/debug/dap/tests.rs` | DAP test harness and fixtures | Test support module; exercises DAP adapters. |
| `src/debug/delve_tests.rs` | Delve DAP integration test cases | Test support module; exercises Go debugging workflows. |
| `src/debug/launch_environment.rs` | Per-launch debuggee environment | Unit (7 in-module tests); `src/debug/launch_environment_native_tests.rs` (4 tests). |
| `src/debug/launch_environment_native_tests.rs` | Public DebugTool requests over the real Rust DAP transport to the shared deterministic protocol peer | Test module (4 tests). |
| `src/debug/ownership.rs` | Per-conversation ownership of retained native debugger sessions | `src/debug/ownership/tests.rs` (5 tests). |
| `src/debug/ownership/identity.rs` | Cancellation-aware identity waits | Unit (5 in-module tests). |
| `src/debug/ownership/tests.rs` | Test suite for `src/debug/ownership.rs` | Test module (5 tests). |
| `src/debug/remote.rs` | Attach to an already-running debugpy DAP endpoint | Unit (4 in-module tests). |
| `src/debug/session.rs` | DAP sessions | `tests/debug.rs`. |
| `src/debug/session/execution.rs` | Debug session execution loop and lifecycle | Unit; `tests/debug_tool.rs`. |
| `src/debug/session/inspection.rs` | Debug session state and stack inspection | Unit; `tests/debug_tool.rs`. |
| `src/debug/source_root_tests.rs` | Native tool calls and the existing DAP protocol peer | Test module (8 tests). |
| `src/debug/terminal.rs` | Explicit local-launch terminal host | `src/debug/terminal/tests.rs` (6 tests). |
| `src/debug/terminal/tests.rs` | Test suite for `src/debug/terminal.rs` | Test module (6 tests). |
| `src/debug/terminal/worker.rs` | Dedicated PTY lanes | No in-module tests; reached through `src/debug/terminal.rs`. |
| `src/debug/tests.rs` | Debug tool test suite and mock adapters | Test support module; exercises debug actions. |
| `src/delight.rs` | Premium delight layer (OMP-ADOPT / bd-cv653.9.9) | Unit (6 tests); `tests/delight.rs`, `tests/e2e_tui.rs`, `tests/chrome_tui_integration.rs`. |
| `src/dialects.rs` | Provider dialects | Provider conformance suites; `tests/json_mode_parity.rs`. |
| `src/doctor.rs` | Doctor and diagnostics | Unit; `tests/doctor_swarm_temp_dir_json.rs`, `tests/franken_node_compatibility_doctor_contract.rs`. |
| `src/document_read.rs` | Native, bounded text extraction from DOCX, PPTX, XLSX, and EPUB packages | Unit (12 in-module tests). |
| `src/document_read/epub.rs` | EPUB package metadata and spine-ordered XHTML text extraction | Unit (10 in-module tests). |
| `src/document_read/xlsx.rs` | Excel workbook text extraction using the shared bounded OPC/XML reader | Unit (10 in-module tests). |
| `src/embedded_assets.rs` | Embedded assets | Waived glue; exercised implicitly by resource loading suites. |
| `src/enforcement.rs` | Extension policy enforcement state machine | Unit (37 tests); `tests/ci_strict_gates_validation.rs`, `tests/extensions_auth_error_coverage.rs`, `tests/graduated_rollout_integration_sec72.rs`, `tests/security_http_policy.rs`. |
| `src/error.rs` | Error types | Unit; `tests/error_types.rs`, `tests/error_handling.rs`. |
| `src/error_hints.rs` | Error remediation hints | Unit; `tests/error_handling.rs`. |
| `src/eval.rs` | Eval harness | `tests/eval.rs`. |
| `src/eval/bridge_io.rs` | Service Python's owned output while one native tool future is pending | `src/eval/bridge_io/tests.rs` (9 tests). |
| `src/eval/bridge_io/delivery.rs` | Cancellation-aware Python input delivery using the existing blocking-I/O pool | `src/eval/bridge_io/delivery/tests.rs` (12 tests). |
| `src/eval/bridge_io/delivery/tests.rs` | Test suite for `src/eval/bridge_io/delivery.rs` | Test module (12 tests). |
| `src/eval/bridge_io/tests.rs` | Test suite for `src/eval/bridge_io.rs` | Test module (9 tests). |
| `src/eval/bridge_result.rs` | Explicit, bounded native result projection over the ordinary eval bridge | Unit (8 in-module tests). |
| `src/eval/display.rs` | Model-visible rich output and structured SDK results from eval cells | Unit (11 in-module tests). |
| `src/eval/display/images.rs` | Bounded image envelope inspection | Unit (5 in-module tests). |
| `src/eval/isolation.rs` | Fresh per-call kernels over the ordinary native eval lifecycle | `src/eval/isolation/tests.rs` (7 tests). |
| `src/eval/isolation/tests.rs` | Test suite for `src/eval/isolation.rs` | Test module (7 tests). |
| `src/eval/js_kernel.rs` | Eval JS kernel | Unit; `tests/eval.rs`; in-source kernel tests. |
| `src/eval/js_kernel/async_host.rs` | Invocation-owned timers and nonblocking tool replies for the sync JS realm | `src/eval/js_kernel/async_host/tests.rs` (21 tests). |
| `src/eval/js_kernel/async_host/tests.rs` | Test suite for `src/eval/js_kernel/async_host.rs` | Test module (21 tests). |
| `src/eval/js_kernel/console.rs` | Invocation-owned native JavaScript console output | Unit (6 in-module tests). |
| `src/eval/js_kernel/display.rs` | A cell-owned rich-output sink | Unit (12 in-module tests). |
| `src/eval/js_kernel/evaluate.rs` | Choose script versus top-level-await grammar without executing a probe | No in-module tests; reached through `src/eval/js_kernel.rs`. |
| `src/eval/js_kernel/evaluate/imports.rs` | Static-import convenience syntax for retained scripts, not a second linker | `src/eval/js_kernel/evaluate/imports/tests.rs` (12 tests). |
| `src/eval/js_kernel/evaluate/imports/tests.rs` | Test suite for `src/eval/js_kernel/evaluate/imports.rs` | Test module (12 tests). |
| `src/eval/js_kernel/evaluation_tests.rs` | Test suite for `src/eval/js_kernel.rs` | Test module (8 tests). |
| `src/eval/js_kernel/modules.rs` | Local ES modules for the retained eval realm, not the extension loader | Unit (2 in-module tests); `src/eval/js_kernel/modules/data_tests.rs` (14 tests); `src/eval/js_kernel/modules/tests.rs` (11 tests). |
| `src/eval/js_kernel/modules/data.rs` | Data-only default exports over the same admitted file and realm lifetime | Unit (1 in-module test). |
| `src/eval/js_kernel/modules/data_tests.rs` | Test suite for `src/eval/js_kernel/modules.rs` | Test module (14 tests). |
| `src/eval/js_kernel/modules/packages.rs` | Workspace-installed ESM packages over the existing file/linker boundary | `src/eval/js_kernel/modules/packages/tests.rs` (12 tests). |
| `src/eval/js_kernel/modules/packages/maps.rs` | Package map selection is independent of filesystem existence and loading | Unit (20 in-module tests). |
| `src/eval/js_kernel/modules/packages/tests.rs` | Test suite for `src/eval/js_kernel/modules/packages.rs` | Test module (12 tests). |
| `src/eval/js_kernel/modules/reload.rs` | Explicit graph generations, without discarding the retained eval realm | Unit (6 in-module tests). |
| `src/eval/js_kernel/modules/tests.rs` | Test suite for `src/eval/js_kernel/modules.rs` | Test module (11 tests). |
| `src/eval/js_kernel/modules/typescript.rs` | Module admission for eval's shared TypeScript compiler | Unit (4 in-module tests). |
| `src/eval/judgment.rs` | Invocation-owned host calls and typed judgments over the eval bridge | `src/eval/judgment/tests.rs` (12 tests). |
| `src/eval/judgment/background.rs` | Retained, tool-free auxiliary model jobs admitted by the current eval cell | `src/eval/judgment/background/tests.rs` (31 tests). |
| `src/eval/judgment/background/tests.rs` | Native retained-job fixtures with controlled provider streams and real jobs | Test module (31 tests). |
| `src/eval/judgment/batch.rs` | Cell-owned batches of ordinary typed judgments, not detached model jobs | `src/eval/judgment/batch/tests.rs` (14 tests). |
| `src/eval/judgment/batch/tests.rs` | Native contracts and controlled-provider workflows; no live model requests | Test module (14 tests). |
| `src/eval/judgment/completion.rs` | One-shot completions share the cell's binding, receipts and attempt ceiling | `src/eval/judgment/completion/image_tests.rs` (8 tests); `src/eval/judgment/completion/tests.rs` (17 tests). |
| `src/eval/judgment/completion/image_tests.rs` | Multimodal completion tests through native admission and both eval kernels | Test module (8 tests). |
| `src/eval/judgment/completion/tests.rs` | Controlled-provider execution tests, not live-model or compilation evidence | Test module (17 tests). |
| `src/eval/judgment/drain.rs` | Read newly settled batch judgments without consuming the native job | `src/eval/judgment/drain/tests.rs` (13 tests). |
| `src/eval/judgment/drain/tests.rs` | Native jobs and policy-controlled host seams; no live model acceptance | Test module (13 tests). |
| `src/eval/judgment/images.rs` | Invocation-owned image evidence for judgments and one-shot completions | `src/eval/judgment/images/tests.rs` (12 tests). |
| `src/eval/judgment/images/tests.rs` | Native image bridge fixtures; no external files or live model requests | Test module (12 tests). |
| `src/eval/judgment/tests.rs` | Native eval/kernel integrations with a controlled provider, not live-model acceptance | Test module (12 tests). |
| `src/eval/lifecycle.rs` | Exclusive cell admission and explicit, language-scoped kernel reset | Unit (24 in-module tests). |
| `src/eval/lifecycle/source.rs` | Shared bounded file admission for scripts, ES modules and text assets | Unit (6 in-module tests). |
| `src/eval/lifecycle/source/typescript.rs` | TypeScript preparation using the project's existing SWC stack | Unit (10 in-module tests). |
| `src/eval/output.rs` | Per-invocation console capture and native ToolUpdate snapshots for both kernels | Unit (20 in-module tests). |
| `src/eval/output/archive.rs` | Opt-in, disk-backed console and complete native cell-result archives | Unit (10 in-module tests); `src/eval/output/archive/result_tests.rs` (8 tests). |
| `src/eval/output/archive/result_tests.rs` | Full cell artifacts exercise the same capture/finalization as the tool | Test module (8 tests). |
| `src/eval/output/live.rs` | Bounded provisional rich results, shared by Python and QuickJS | `src/eval/output/live/tests.rs` (6 tests). |
| `src/eval/output/live/tests.rs` | Test suite for `src/eval/output/live.rs` | Test module (6 tests). |
| `src/eval/python_runtime.rs` | Discover Python before sending any cell, without changing retained kernels | `src/eval/python_runtime/tests.rs` (12 tests). |
| `src/eval/python_runtime/tests.rs` | Test suite for `src/eval/python_runtime.rs` | Test module (12 tests). |
| `src/eval/session.rs` | Session-scoped retained eval namespaces over the registry's live identity | `src/eval/session/tests.rs` (16 tests). |
| `src/eval/session/tests.rs` | Test suite for `src/eval/session.rs` | Test module (16 tests). |
| `src/eval/transport.rs` | Bounded Python protocol transport | Unit (17 in-module tests). |
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
| `src/extensions/event_coalescer_impl.rs` | Extension event coalescer | Extension event suites (`tests/extensions_event_wiring.rs`). |
| `src/extensions/exec_mediation.rs` | Extension exec mediation | `tests/bash_mediation.rs`; in-source exec-security tests. |
| `src/extensions/extension_manager_impl.rs` | Extension manager | Extension lifecycle suites (`tests/extensions_runtime_matrix.rs`). |
| `src/extensions/fs_connector.rs` | Extension fs connector | `tests/extensions_fs_shim.rs`. |
| `src/extensions/fs_connector/atomic_write.rs` | Staged extension filesystem writes with a single atomic publication point | Unit; in-module tests; `tests/security_fs_escape.rs`. |
| `src/extensions/fs_connector/atomic_write/metadata.rs` | Linux descriptor-scoped metadata (ACL/xattr) preservation for atomic replacement | Unit; in-module tests. |
| `src/extensions/native_runtime.rs` | Active native descriptor runtime | Active native descriptor runtime interpreter. |
| `src/extensions/native_runtime/shutdown_tests.rs` | Native runtime shutdown admission test suite | Test module; real native runtime entry points and shared shutdown admission. |
| `src/extensions/native_runtime/streams.rs` | Native provider stream identities and bounded, explicit terminal receipts | Unit (12 in-module tests). |
| `src/extensions/native_runtime_experimental.rs` | Native runtime (experimental) | In-source runtime-parity tests. |
| `src/extensions/permission_drift.rs` | Permission drift detection | In-source tests; policy suites. |
| `src/extensions/policy_snapshot_tests.rs` | Policy snapshot tests | In-source test module (waived glue). |
| `src/extensions/protocol.rs` | Extension protocol | Extension conformance suites. |
| `src/extensions/tests.rs` | Extension test root | In-source test module (waived glue). |
| `src/extensions/tests/baseline.rs` | Extension tests: baseline | In-source test module (waived glue). |
| `src/extensions/tests/concurrency.rs` | Extension tests: concurrency | In-source test module (waived glue). |
| `src/extensions/tests/core.rs` | Extension tests: core | In-source test module (waived glue). |
| `src/extensions/tests/enforcement.rs` | Extension tests: enforcement | In-source test module (waived glue). |
| `src/extensions/tests/event_timeouts.rs` | Extension tests: event timeouts | In-source test module (waived glue). |
| `src/extensions/tests/exec_security.rs` | Extension tests: exec security | In-source test module (waived glue). |
| `src/extensions/tests/policy_transition.rs` | Extension tests: policy transition | In-source test module (waived glue). |
| `src/extensions/tests/reactor.rs` | Extension tests: reactor | In-source test module (waived glue). |
| `src/extensions/tests/registration.rs` | Extension tests: registration | In-source test module (waived glue). |
| `src/extensions/tests/risk_math.rs` | Extension tests: risk math | In-source test module (waived glue). |
| `src/extensions/tests/runtime_parity.rs` | Extension tests: runtime parity | In-source test module (waived glue). |
| `src/extensions/tests/security_alerts.rs` | Extension tests: security alerts | In-source test module (waived glue). |
| `src/extensions/tests/shared_dispatch.rs` | Extension tests: shared dispatch | In-source test module (waived glue). |
| `src/extensions/tests/ui_protocol.rs` | Extension tests: UI protocol | In-source test module (waived glue). |
| `src/extensions/wasm_host.rs` | Wasm extension host | Extension runtime suites; in-source tests. |
| `src/extensions_js.rs` | QuickJS bridge | Unit; `tests/event_loop_conformance.rs`, `tests/js_runtime_ordering.rs`, `tests/node_*_shim.rs`, `tests/e2e_ts_extension_loading.rs`. |
| `src/failover.rs` | Provider failover | `tests/e2e_failover.rs`. |
| `src/failover/provider_code.rs` | Preserve Connect error categories across the agent's text error boundary | Unit (10 in-module tests). |
| `src/file_identity.rs` | Platform file identity for TOCTOU checks (`(dev, ino)` on Unix, volume serial + file index via `winapi-util` on Windows) | Unit (5 tests); exercised through the identity guards in `src/jobs.rs` and `src/mcp/trust.rs`. |
| `src/file_lock.rs` | Cross-process directory locking | Unit; session-index lock integration coverage in `tests/session_index_tests.rs` and RPC concurrency coverage in `tests/e2e_rpc.rs`. |
| `src/flake_classifier.rs` | Flake classifier | Unit; patterns are mirrored by `scripts/ci_conformance_retry.sh`. |
| `src/gallery.rs` | Visual component gallery harness (OMP-ADOPT / bd-cv653.9.10) | Unit (1 test); `tests/gallery.rs`, `tests/chrome_tui_integration.rs`. |
| `src/gc.rs` | Session GC | `tests/gc.rs`. |
| `src/github.rs` | GitHub integration | `tests/hub.rs`; covered through hub/review suites. |
| `src/goal.rs` | Persistent, branch-scoped objectives for SDK hosts | `src/goal/budget_tests.rs` (19 tests); `src/goal/tests.rs` (14 tests). |
| `src/goal/budget_tests.rs` | Native journal and SDK coverage for explicit total-budget revisions | Test module (19 tests). |
| `src/goal/continuation.rs` | Opt-in goal continuation that stops when tool work stops changing | `src/goal/continuation/tests.rs` (17 tests). |
| `src/goal/continuation/tests.rs` | Test suite for `src/goal/continuation.rs` | Test module (17 tests). |
| `src/goal/driver.rs` | Bounded goal execution through the existing controlled SDK prompt path | `src/goal/driver/tests.rs` (15 tests). |
| `src/goal/driver/budget.rs` | Stop a goal inside its native prompt when completed assistant usage reaches the remaining allowance | Unit (8 in-module tests). |
| `src/goal/driver/input.rs` | Live goal input uses the current native turn's bounded mailbox | Unit (9 in-module tests). |
| `src/goal/driver/tests.rs` | Native goal runner probes: real session, agent, OpenAI wire adapter and bounded loopback HTTP peer | Test module (15 tests). |
| `src/goal/driver/verification.rs` | Await host evidence under the goal run's original clock and cancellation | Unit (4 in-module tests). |
| `src/goal/reconciliation.rs` | Resolve an interrupted native intent without replaying work or resetting its budget | Unit (9 in-module tests). |
| `src/goal/rpc.rs` | RPC projection and lifecycle adapter for persistent goals | Unit (3 in-module tests); `src/goal/rpc/resolution_tests.rs` (11 tests). |
| `src/goal/rpc/resolution.rs` | Explicit host decisions on the RPC loop's existing guarded stop route | `src/goal/rpc/resolution/reconciliation_tests.rs` (8 tests). |
| `src/goal/rpc/resolution/reconciliation_tests.rs` | Native lifecycle adapters, not replacement journals or provider simulations | Test module (8 tests). |
| `src/goal/rpc/resolution_tests.rs` | Native journal/persistence tests for the RPC adapter's reviewed stop route | Test module (11 tests). |
| `src/goal/terminal.rs` | Terminal controls over the native branch-scoped goal journal | Unit (1 in-module test); `src/goal/terminal/control_tests.rs` (7 tests); `src/goal/terminal/tests.rs` (11 tests). |
| `src/goal/terminal/control_tests.rs` | Native goal/session tests with a controlled Provider boundary | Test module (7 tests). |
| `src/goal/terminal/tests.rs` | Real session/SDK journal tests; no provider or persistence replacement | Test module (11 tests). |
| `src/goal/tests.rs` | Test suite for `src/goal.rs` | Test module (14 tests). |
| `src/handoff.rs` | Handoff generation | `tests/handoff_generator.rs`. |
| `src/hostcall_amac.rs` | Hostcall AMAC | Unit; `tests/streaming_hostcall.rs`. |
| `src/hostcall_egraph.rs` | Equality-saturation rewrite search over hot hostcall execution plans | Unit (43 tests); `tests/hostcall_egraph.rs`. |
| `src/hostcall_io_uring_lane.rs` | Hostcall io_uring lane | Unit; `tests/streaming_hostcall.rs`. |
| `src/hostcall_queue.rs` | Hostcall queue | Unit; `tests/hostcall_queue_ebr.rs`, `tests/hostcall_queue_loom.rs`; loom opt-in owner `bd-8t27h.6`. |
| `src/hostcall_rewrite.rs` | Hostcall rewrite | Unit; `tests/streaming_hostcall.rs`. |
| `src/hostcall_s3_fifo.rs` | Hostcall S3 FIFO | Unit; `tests/hostcall_s3_fifo_policy.rs`. |
| `src/hostcall_superinstructions.rs` | Hostcall superinstructions | Unit; `tests/streaming_hostcall.rs`. |
| `src/hostcall_trace_jit.rs` | Hostcall trace JIT | Unit; `tests/streaming_hostcall.rs`. |
| `src/http/client.rs` | HTTP client | Unit; `tests/http_client.rs`; branch export baseline marks `src/http/*.rs` as branch-SIGSEGV fallback. |
| `src/http/mod.rs` | HTTP module glue | Waived glue: re-export/test-module wiring. |
| `src/http/proxy.rs` | Outbound HTTP/HTTPS proxy resolution (gh #210) | Unit (21 in-module tests covering precedence, `no_proxy` bypass matching, and credential handling); `tests/http_proxy.rs`. |
| `src/http/proxy/socks5.rs` | SOCKS5 CONNECT negotiation (RFC 1928) and username/password auth (RFC 1929) | Unit (12 in-module tests covering negotiation, authentication and bounds). |
| `src/http/sse.rs` | HTTP SSE | Unit; `tests/repro_sse_flush.rs`. |
| `src/http/test_api.rs` | HTTP test support | Waived test-only support module; compiled only for tests. |
| `src/http/test_asupersync.rs` | HTTP test support | Waived test-only support module; compiled only for tests. |
| `src/http_shim.rs` | Node HTTP shim | `tests/node_http_shim.rs`; branch export baseline marks this as branch-SIGSEGV fallback. |
| `src/hub.rs` | Hub | `tests/hub.rs`. |
| `src/interactive.rs` | TUI root | Unit test module wiring; `tests/tui_snapshot.rs`, `tests/tui_state.rs`, `tests/e2e_tui.rs`. |
| `src/interactive/agent.rs` | TUI agent lane | Unit; `tests/e2e_tui.rs`, `tests/tui_state.rs`. |
| `src/interactive/commands.rs` | Interactive commands | Unit; `tests/interactive_commands_unit.rs`, `tests/interactive_extension_ui.rs`. |
| `src/interactive/conversation.rs` | Conversation model | Unit; `tests/tui_state.rs`. |
| `src/interactive/ext_session.rs` | Extension session UI | Unit; `tests/interactive_extension_ui.rs`; branch export baseline marks this as branch-SIGSEGV fallback. |
| `src/interactive/file_refs.rs` | File references | `tests/tui_state.rs`. |
| `src/interactive/keybindings.rs` | Interactive keybindings | Unit; `tests/tui_state.rs`. |
| `src/interactive/login_flow.rs` | UI-independent `/login` flow shared by the classic and FTUI stacks | Unit (3 in-module tests). |
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
| `src/interactive/workspace_reports.rs` | Workspace slash commands shared by the interactive stacks | Unit (9 in-module tests). |
| `src/interactive_ftui.rs` | FTUI interactive surface | `tests/e2e_ftui.rs`. |
| `src/interactive_ftui/console_input.rs` | Native Windows console-input ownership for the FTUI lifecycle | Unit (12 in-module tests); `tests/ftui_conhost_console.rs` (real Windows console, opt-in) and `examples/ftui_conhost_probe.rs`. |
| `src/interactive_ftui/info_commands.rs` | Read-only OMP info commands on the default stack | Unit (7 in-module tests). |
| `src/interactive_ftui/plan_commands.rs` | Default-FTUI access to the SDK's session-bound plan lifecycle | Unit; `src/interactive_ftui/plan_commands/tests.rs`. |
| `src/interactive_ftui/plan_commands/tests.rs` | FTUI plan command test suite | Test module; plan command tests. |
| `src/interactive_ftui/session_pins.rs` | Sessions pinned to the top of the `/resume` list (OMP `/pin`) | Unit (1 in-module test). |
| `src/interactive_ftui/settings.rs` | Settings on the default interactive stack: one validated choice is saved before the live session is updated | Unit (5 in-module tests). |
| `src/interactive_ftui/workspace_commands.rs` | OMP workspace commands on the default stack | Unit (1 in-module test). |
| `src/interactive_ftui/workspace_commands/goal_progress.rs` | Display and invocation-owned cancellation for terminal goal commands | Unit (8 in-module tests). |
| `src/interactive_ftui/workspace_commands/model_handoff.rs` | Explicit model-assisted continuation briefs on the default terminal | `src/interactive_ftui/workspace_commands/model_handoff/tests.rs` (16 tests). |
| `src/interactive_ftui/workspace_commands/model_handoff/bundle.rs` | Portable handoff bundles: explicit local publication, never automatic delivery | No in-module tests; reached through `src/interactive_ftui/workspace_commands/model_handoff.rs`. |
| `src/interactive_ftui/workspace_commands/model_handoff/import.rs` | Provider-free receipt of a portable handoff | `src/interactive_ftui/workspace_commands/model_handoff/import/tests.rs` (10 tests). |
| `src/interactive_ftui/workspace_commands/model_handoff/import/tests.rs` | Native dispatcher, filesystem publication and session-reader regressions | Test module (10 tests). |
| `src/interactive_ftui/workspace_commands/model_handoff/tests.rs` | Native dispatcher/session and controlled-provider tests, not live-model proof | Test module (16 tests). |
| `src/jobs.rs` | Background jobs | `tests/jobs.rs`. |
| `src/jobs/host_task.rs` | Owned native work in the ordinary jobs registry | Unit (12 in-module tests); `src/jobs/host_task/progress_tests.rs` (7 tests). |
| `src/jobs/host_task/progress_tests.rs` | Native progress, interruption, privacy and retention regressions | Test module (7 tests). |
| `src/judgment.rs` | Tool-free judgments and completions through one explicit provider binding | `src/judgment/tests.rs` (24 tests). |
| `src/judgment/completion.rs` | Stateless text completion through the existing bound auxiliary model | Unit (3 in-module tests). |
| `src/judgment/completion/schema.rs` | Host-validated completion data, not provider-specific structured-output mode | Unit (8 in-module tests). |
| `src/judgment/contract.rs` | Validation of caller rubrics and terminal provider answers | No in-module tests; reached through `src/judgment.rs`. |
| `src/judgment/images.rs` | Image evidence stays outside text serialization and privacy projection | No in-module tests; reached through `src/judgment.rs`. |
| `src/judgment/tests.rs` | Unit-level provider-stream fixtures, not live model or DSR acceptance | Test module (24 tests). |
| `src/keybindings.rs` | Keybinding config | Unit; interactive/TUI tests. |
| `src/lib.rs` | Crate exports | Waived glue: exported module surface is compiled by all targets; no behavior-only row. |
| `src/lsp.rs` | LSP facade | `tests/lsp.rs`, `tests/e2e_lsp.rs`. |
| `src/lsp/actions.rs` | LSP code action and refactoring execution | Unit; `tests/lsp.rs`. |
| `src/lsp/actions/command_edits.rs` | LSP execute command workspace edit collector | `tests/lsp.rs`, `src/lsp/actions/command_tests.rs`. |
| `src/lsp/actions/command_tests.rs` | LSP execute command test suite | Test module; execute command tests. |
| `src/lsp/actions/preview_tests.rs` | LSP code action preview and approval test suite | Test module; code action preview tests. |
| `src/lsp/actions/protocol_tests.rs` | LSP code action protocol test cases | Test support module; exercises code actions. |
| `src/lsp/actions/refactor.rs` | LSP symbol rename and refactor operations | Unit; `src/lsp/actions/refactor/tests.rs`, `tests/lsp.rs`. |
| `src/lsp/actions/refactor/directory.rs` | Coordinated rename_file through the existing LSP/refactor transaction | `src/lsp/actions/refactor/directory/tests.rs` (15 tests). |
| `src/lsp/actions/refactor/directory/merge.rs` | Reconcile independent server edit sets against one original workspace | Unit (7 in-module tests). |
| `src/lsp/actions/refactor/directory/tests.rs` | Exercise real tool dispatch, child framing, refactor caching and filesystem mutation | Test module (15 tests). |
| `src/lsp/actions/refactor/formatting.rs` | LSP document and range formatting | Unit; `src/lsp/actions/refactor/formatting/tests.rs`. |
| `src/lsp/actions/refactor/formatting/tests.rs` | LSP document formatting test suite | Test module; document formatting tests. |
| `src/lsp/actions/refactor/formatting/review_tests.rs` | LSP document formatting review test suite | Test module; document formatting review tests. |
| `src/lsp/actions/refactor/prepare.rs` | LSP rename preparation and target confirmation | Unit; `src/lsp/actions/refactor/prepare/tests.rs`, `tests/lsp.rs`. |
| `src/lsp/actions/refactor/prepare/tests.rs` | LSP rename preparation test suite | Test module; prepare rename tests. |
| `src/lsp/actions/refactor/prepare/tests/protocol.rs` | LSP rename preparation protocol test cases | Test support module; exercises prepare rename protocol. |
| `src/lsp/actions/refactor/tests.rs` | LSP symbol refactor test suite | Test module; refactor tests. |
| `src/lsp/actions/refactor/versions.rs` | LSP request-time document version tracking across resource operations | Unit; `src/lsp/actions/refactor/versions/tests.rs`. |
| `src/lsp/actions/refactor/versions/tests.rs` | LSP document version tracking test suite | Test module; version tracking tests. |
| `src/lsp/actions/refactor/versions/tests/protocol.rs` | LSP document version tracking protocol test cases | Test support module; exercises version tracking. |
| `src/lsp/actions/review_tests.rs` | LSP code action review and approval test suite | Test module; code action review tests. |
| `src/lsp/actions/selection_tests.rs` | LSP code action selection test suite | Test module; action selection tests. |
| `src/lsp/client.rs` | LSP client | `tests/lsp.rs`. |
| `src/lsp/client/document_sync.rs` | LSP client live document synchronization | Unit; `src/lsp/client/document_sync/tests.rs`. |
| `src/lsp/client/document_sync/tests.rs` | LSP document synchronization tests | Test module; document sync tests. |
| `src/lsp/client/file_uri.rs` | LSP file URI parser and path resolution | Unit; `src/lsp/client/file_uri/tests.rs`. |
| `src/lsp/client/file_uri/tests.rs` | LSP file URI test suite | Test module; file URI tests. |
| `src/lsp/client/pull_diagnostics.rs` | LSP client pull diagnostics | Unit; `src/lsp/client/pull_diagnostics/tests.rs`. |
| `src/lsp/client/pull_diagnostics/tests.rs` | LSP pull diagnostics test suite | Test module; pull diagnostics tests. |
| `src/lsp/client/request.rs` | LSP client request dispatch and deadline management | Unit; `src/lsp/client/request/tests.rs`. |
| `src/lsp/client/request/array_results.rs` | Bounded streamed array results for read-only symbol and navigation requests | Unit (5 in-module tests); `src/lsp/semantic/streamed_reads.rs`. |
| `src/lsp/client/request/partial_results.rs` | Request-owned LSP partial-result collection, delivered only after an observed successful response | Unit (3 in-module tests); `src/lsp/client/request/partial_results/wire_tests.rs`. |
| `src/lsp/client/request/partial_results/wire_tests.rs` | LSP partial-result wire test suite | Test module; held `workspace/diagnostic` requests over the real framed-stdio transport (6 tests). |
| `src/lsp/client/request/tests.rs` | LSP request dispatch test suite | Test module; request tests. |
| `src/lsp/client/request/workspace_diagnostics.rs` | Whole-workspace pull diagnostics with optional request-owned partial results and no reuse of previous result IDs | Unit (9 in-module tests); `src/lsp/client/request/partial_results/wire_tests.rs`. |
| `src/lsp/client/test_server.rs` | LSP test server fixture | Test support module; exercises client tests. |
| `src/lsp/client/workspace.rs` | Connection-local workspace settings and server-to-client workspace requests | `src/lsp/client/workspace/tests.rs` (19 tests). |
| `src/lsp/client/workspace/tests.rs` | Test suite for `src/lsp/client/workspace.rs` | Test module (19 tests). |
| `src/lsp/client/workspace/tests/protocol.rs` | Exercise workspace service replies and configuration effects through the actual client, registry, tool workflow and framed child stdio | Test module (14 tests). |
| `src/lsp/completion.rs` | LSP native completion engine and handle manager | Unit; `src/lsp/completion/tests.rs`, `src/lsp/completion/tests/protocol.rs`. |
| `src/lsp/completion/item.rs` | LSP completion item normalization, resolution, and text edits | Unit; `src/lsp/completion/item/tests.rs`. |
| `src/lsp/completion/item/indentation.rs` | Apply the negotiated completion indentation policy to the primary edit only | Unit (11 in-module tests). |
| `src/lsp/completion/item/tests.rs` | LSP completion item test suite | Test module; completion item tests. |
| `src/lsp/completion/snippet.rs` | LSP completion snippet placeholder parsing and expansion | Unit; `src/lsp/completion/snippet/tests.rs`. |
| `src/lsp/completion/snippet/tests.rs` | LSP snippet test suite | Test module; snippet tests. |
| `src/lsp/completion/snippet/transform.rs` | Bounded placeholder transforms for headless semantic completions | Unit; `src/lsp/completion/snippet/transform/tests.rs`. |
| `src/lsp/completion/snippet/transform/tests.rs` | LSP snippet transform test suite | Test module; snippet transform tests. |
| `src/lsp/completion/tests.rs` | LSP completion handle and integration test suite | Test module; completion tests. |
| `src/lsp/completion/tests/protocol.rs` | LSP completion protocol scenario tests | Test module; protocol scenarios. |
| `src/lsp/diagnostics_tests.rs` | LSP model-facing diagnostics test suite | Test module; diagnostics tool tests. |
| `src/lsp/edits.rs` | LSP edits | `tests/lsp.rs`. |
| `src/lsp/edits/annotations.rs` | LSP change annotations retained with a workspace edit, and the reviewed-confirmation requirement | Unit (`annotations/tests.rs`). |
| `src/lsp/edits/annotations/tests.rs` | LSP change annotation test suite | Test module; annotation retention and confirmation tests. |
| `src/lsp/edits/annotations/tests/protocol.rs` | LSP change annotation protocol tests against a scripted language-server fixture | Test module; negotiation and reviewed-approval scenarios (`server.py` fixture). |
| `src/lsp/edits/sequence.rs` | LSP workspace edit sequence parser and validator | Unit; in-module tests. |
| `src/lsp/edits/sequence/tests.rs` | LSP workspace edit sequence test suite | Test module; sequence tests. |
| `src/lsp/edits/transaction.rs` | LSP atomic workspace edit transaction engine | Unit; in-module tests. |
| `src/lsp/edits/transaction/directory.rs` | A bounded, final directory move in an otherwise regular-file transaction | `src/lsp/edits/transaction/directory/tests.rs` (10 tests). |
| `src/lsp/edits/transaction/directory/tests.rs` | Test suite for `src/lsp/edits/transaction/directory.rs` | Test module (10 tests). |
| `src/lsp/edits/transaction/evidence.rs` | LSP edit transaction evidence tracking and verification | Unit; `src/lsp/edits/transaction/evidence/tests.rs`. |
| `src/lsp/edits/transaction/evidence/tests.rs` | LSP edit transaction evidence test suite | Test module; transaction evidence tests. |
| `src/lsp/edits/transaction/tests.rs` | LSP workspace edit transaction test suite | Test module; transaction tests. |
| `src/lsp/hierarchy.rs` | LSP call hierarchy navigation | Unit; `src/lsp/hierarchy/tests.rs`, `tests/lsp.rs`. |
| `src/lsp/hierarchy/tests.rs` | LSP call hierarchy test suite | Test module; hierarchy tests. |
| `src/lsp/jsonrpc.rs` | LSP JSON-RPC | `tests/lsp.rs`. |
| `src/lsp/jsonrpc/completion.rs` | LSP request waits that retain their owner and retire abandoned requests on drop | Unit (6 in-module tests). |
| `src/lsp/jsonrpc/outbound.rs` | Bounded, non-blocking admission to the single child-stdin pump | Unit (6 in-module tests). |
| `src/lsp/project_diagnostics.rs` | Agent-visible native project diagnostics for one anchored server and workspace, distinct from a bounded file scan | Unit (7 in-module tests). |
| `src/lsp/refactor_preview.rs` | LSP refactor preview and approval staging | Unit; `src/lsp/refactor_preview/tests.rs`, `src/lsp/refactor_preview/tests/protocol.rs`. |
| `src/lsp/refactor_preview/group.rs` | Connection and document evidence for one coordinated refactor | No in-module tests; reached through `src/lsp/refactor_preview.rs`. |
| `src/lsp/refactor_preview/tests.rs` | LSP refactor preview test suite | Test module; refactor preview tests. |
| `src/lsp/refactor_preview/tests/protocol.rs` | LSP refactor preview protocol test cases | Test support module; refactor protocol scenarios. |
| `src/lsp/registry.rs` | LSP registry | `tests/lsp.rs`. |
| `src/lsp/semantic.rs` | LSP semantic inspection, signature help, and inlay hints | Unit; `src/lsp/semantic/tests.rs`, `src/lsp/semantic/tests/hints.rs`. |
| `src/lsp/semantic/hints.rs` | LSP inlay hints resolution and parameter/type hints | Unit; `src/lsp/semantic/hints/tests.rs`. |
| `src/lsp/semantic/hints/tests.rs` | LSP inlay hints parser and contract test suite | Test module; inlay hint tests. |
| `src/lsp/semantic/navigation.rs` | LSP exact source-bound definition, reference, and hover navigation | Unit; `src/lsp/semantic/navigation/tests.rs`, `tests/lsp.rs`. |
| `src/lsp/semantic/navigation/protocol.rs` | LSP source-bound navigation protocol test cases | Test support module; exercises navigation protocol. |
| `src/lsp/semantic/navigation/selection.rs` | Server-suggested enclosing selections for the existing refactor workflow | Unit (11 in-module tests). |
| `src/lsp/semantic/navigation/tests.rs` | LSP source-bound navigation test suite | Test module; navigation tests. |
| `src/lsp/semantic/outline.rs` | Complete, source-checked document outlines, including streamed hierarchies | `src/lsp/semantic/outline/tests.rs` (18 tests). |
| `src/lsp/semantic/outline/source_text.rs` | Exact code excerpts from a validated, immutable document-symbol source | Unit (10 in-module tests). |
| `src/lsp/semantic/outline/tests.rs` | Test suite for `src/lsp/semantic/outline.rs` | Test module (18 tests). |
| `src/lsp/semantic/signatures.rs` | LSP signature help, parameter doc extraction, and overload resolution | Unit; `src/lsp/semantic/signatures/tests.rs`. |
| `src/lsp/semantic/signatures/tests.rs` | LSP signature help parsing and formatting test suite | Test module; signature help tests. |
| `src/lsp/semantic/streamed_reads.rs` | LSP streamed symbol and source-navigation read test suite | Test module; tool-to-wire coverage of streamed results, retries, timeouts and the source-drift fence (9 tests). |
| `src/lsp/semantic/symbols.rs` | LSP workspace symbol search and lazy location resolution | Unit; `src/lsp/semantic/symbols/tests.rs`, `tests/lsp.rs`. |
| `src/lsp/semantic/symbols/tests.rs` | LSP workspace symbol test suite | Test module; workspace symbol tests. |
| `src/lsp/semantic/symbols/tests/protocol.rs` | LSP workspace symbol protocol test cases | Test support module; exercises symbol protocol. |
| `src/lsp/semantic/tests.rs` | LSP semantic inspection and signature help integration test suite | Test module; semantic inspection tests. |
| `src/lsp/semantic/tests/hints.rs` | LSP inlay hint integration and resolve scenario tests | Test module; inlay hint integration tests. |
| `src/lsp/text.rs` | LSP text mapping | `tests/lsp.rs`. |
| `src/lsp/workspace_diagnostics.rs` | LSP workspace diagnostics tool | Unit; `src/lsp/workspace_diagnostics/tests.rs`. |
| `src/lsp/workspace_diagnostics/tests.rs` | LSP workspace diagnostics test suite | Test module; workspace diagnostics tests. |
| `src/magic_keywords.rs` | Magic keywords | `tests/magic_keywords.rs`. |
| `src/main.rs` | CLI entry | Unit; `tests/e2e_cli.rs`, `tests/e2e_rpc.rs`, `tests/main_cli_selection.rs`; branch export baseline marks this as branch-SIGSEGV fallback. |
| `src/markdown_rich.rs` | Markdown, math, mermaid, and visual rendering (OMP-ADOPT / bd-cv653.9.7) | Unit (9 tests); `tests/markdown_rich.rs`, `tests/chrome_tui_integration.rs`. |
| `src/mcp.rs` | MCP facade | `tests/mcp.rs`. |
| `src/mcp/auth.rs` | Native MCP OAuth | Unit (16 in-module tests). |
| `src/mcp/config.rs` | MCP config | `tests/mcp.rs`. |
| `src/mcp/config/acp.rs` | Decoding of ACP session-scoped MCP server lists with a distinct trust provenance and no execution grant | Unit (6 in-module tests). |
| `src/mcp/content.rs` | MCP content block shaper and serialization | Unit; `tests/mcp_conformance.rs`. |
| `src/mcp/manager.rs` | MCP manager | `tests/mcp.rs`. |
| `src/mcp/manager/auth_tests.rs` | Wire-level regressions for native MCP authentication admission and failures | Test module (9 tests). |
| `src/mcp/manager/calls.rs` | MCP tool call execution and cancellation | Unit; `tests/mcp.rs`. |
| `src/mcp/manager/calls/progress_tests.rs` | Real mounted-tool -> manager -> native transport workflows against an independently implemented, bounded protocol peer | Test module (5 tests). |
| `src/mcp/manager/catalog.rs` | MCP tool catalog discovery and pagination | Unit; `tests/mcp_conformance.rs`. |
| `src/mcp/manager/catalog/context.rs` | MCP resource catalog and context discovery | Unit; in-module tests. |
| `src/mcp/manager/config_tests.rs` | Discovery-to-execution regressions for imported server settings | Test module (5 tests). |
| `src/mcp/manager/connection.rs` | MCP server connection setup, lifecycle, and owner tracking | Unit; `tests/mcp.rs`. |
| `src/mcp/manager/live_catalog_tests.rs` | MCP live catalog refresh test suite | Test module; live-agent catalog refresh, withdrawal, abort and owner reconciliation (5 tests). |
| `src/mcp/manager/output_schema.rs` | MCP tool call structured-output contract validation | Unit; `src/mcp/manager/calls.rs`, `tests/mcp.rs`. |
| `src/mcp/progress.rs` | Request-owned MCP progress | `src/mcp/progress/tests.rs` (12 tests). |
| `src/mcp/progress/tests.rs` | Test suite for `src/mcp/progress.rs` | Test module (12 tests). |
| `src/mcp/sse_transport.rs` | Legacy MCP HTTP+SSE transport (protocol revision 2024-11-05) | Unit (12 in-module tests). |
| `src/mcp/transport.rs` | MCP transport | `tests/mcp.rs`. |
| `src/mcp/transport/cancellable_stdio.rs` | Cancellation-aware Unix MCP pipes | Unit (12 in-module tests). |
| `src/mcp/transport/progress_tests.rs` | Test suite for `src/mcp/transport.rs` | Test module (2 tests). |
| `src/mcp/trust.rs` | MCP trust | `tests/mcp.rs`. |
| `src/mcp/uri_template.rs` | RFC 6570 URI template expansion and variable validation | Unit; `src/mcp.rs`, `tests/mcp.rs`. |
| `src/media_tools.rs` | Opt-in media trio tools `inspect_image` / `generate_image` / `tts` (bd-cv653.2.7) | `tests/media_tools.rs`, `tests/conformance_fixtures.rs`. |
| `src/media_tools/artifact.rs` | Media tools artifact publishing and staging | Unit; `tests/media_tools.rs`. |
| `src/media_tools/generation.rs` | Image generation tool backend | Unit; `tests/media_tools.rs`, `tests/conformance_fixtures.rs`. |
| `src/media_tools/generation/inputs.rs` | Image generation input resolution and validation | Unit; `tests/media_tools.rs`. |
| `src/media_tools/speech.rs` | Text-to-speech audio synthesis tool | Unit; `tests/media_tools.rs`, `tests/conformance_fixtures.rs`. |
| `src/media_tools/transport.rs` | Media tools shared HTTP transport and auth | Unit; `tests/media_tools.rs`. |
| `src/media_tools/video.rs` | Bounded local-video previews for the read tool | Unit (12 in-module tests). |
| `src/media_tools/vision.rs` | Vision image analysis tool | Unit; `tests/media_tools.rs`, `tests/conformance_fixtures.rs`. |
| `src/memory.rs` | Project memory bank and atomic supersession | Unit; `tests/memory.rs` exercises the public tools, registry gate and startup injection. Mutation tests are also in `src/memory/transactions.rs`. New coverage added, not executed on the editing host. |
| `src/memory/cass.rs` | Optional read-only project-history recall from the owner's installed CASS archive, screened with the memory privacy rules | Unit (12 in-module tests); `tests/memory.rs` (CASS backend registry cases). |
| `src/memory/reflection.rs` | Authenticated bounded memory synthesis and citation identity checks | Unit (10 tests); `tests/memory.rs` uses the public tool and Gemini over loopback HTTP for successful synthesis, terminal failures, unknown citations and credential redaction. Added, not executed: DSR unavailable. |
| `src/memory/shared.rs` | Shared cross-session memory bank | Unit; `tests/memory.rs`. |
| `src/memory/shared/delegation.rs` | Shared memory agent delegation and filtering | Unit; `tests/memory.rs`. |
| `src/memory/shared/delegation_tests.rs` | Shared memory delegation test cases | Test support module; exercises memory delegation. |
| `src/memory/shared_tests.rs` | Shared memory test suite | Test support module; exercises shared memory backend. |
| `src/memory/transactions.rs` | Atomic primary/FTS/audit writes and supersession | Unit (11 real-SQLite tests), including late audit failure, rollback after reopening, competing writers, stale supersession, index repair and the retain tool. Added, not executed: DSR unavailable. |
| `src/migrations.rs` | Migrations | Unit; SQLite/session migration coverage through `tests/session_sqlite.rs`. |
| `src/model.rs` | Message/content model | Unit; `tests/model_serialization.rs`. |
| `src/model/media_duration.rs` | Bounded container-header inspection that supplies media duration hints for context accounting | Unit (12 in-module tests). |
| `src/model_routing.rs` | Model-routing policy and evidence | Unit; model-routing tests in this module plus `tests/model_selector_cycling.rs`. |
| `src/model_selector.rs` | Model selector | Unit; `tests/model_selector_cycling.rs`. |
| `src/models.rs` | Model registry | Unit; `tests/model_registry.rs`. |
| `src/overlay_system.rs` | Unified overlay and set-piece surfaces (OMP-ADOPT / bd-cv653.9.8) | Unit (3 tests); `tests/overlay_system.rs`, `tests/chrome_tui_integration.rs`. |
| `src/package_manager.rs` | Package manager | Unit; `tests/package_manager.rs`, `tests/e2e_cli.rs`. |
| `src/perf_build.rs` | Perf build metadata | Unit; `tests/perf_bench_harness.rs`, `tests/perf_budgets.rs`, `tests/perf_regression.rs`. |
| `src/permissions.rs` | Capability permissions | Unit; `tests/capability_policy_model.rs`, `tests/capability_policy_scoped.rs`, `tests/capability_denial_matrix.rs`. |
| `src/pi_wasm.rs` | PiWasm runtime | Unit; `tests/lab_runtime_extensions.rs`; unsupported imports fail closed, with bounded Emscripten compatibility stubs covered by source tests. |
| `src/plan.rs` | Plan mode | `tests/e2e_plan_mode.rs`. |
| `src/plan/session.rs` | SDK plan review, prompt ownership and journaled lifecycle | Unit; `src/plan/session/tests.rs` exercises real SDK handles and provider/tool loops. Added, not executed: DSR unavailable. |
| `src/plan/session/tests.rs` | SDK plan lifecycle contract and wire tests | Test module; 26 tests for submission identity, policy separation, prompt cleanup, persistence and actual tool dispatch. Added, not executed: DSR unavailable. |
| `src/platform.rs` | Platform helpers | Unit. |
| `src/pmu_telemetry.rs` | PMU-guided stall-cycle elimination and microarchitectural regression budgets | `tests/pmu_telemetry.rs`. |
| `src/prewalk.rs` | One-shot model handoff after planning and the first successful file edit | Unit (3 in-module tests). |
| `src/profiler.rs` | Sampling profiler front-end (`--profile` / `PI_PROFILE=1`, bd-cv653.7.12.1) | Unit (3 tests); no dedicated integration test (manual `pi --profile`). |
| `src/provider.rs` | Provider trait/schema | Unit; `tests/provider_factory.rs`, `tests/provider_contract.rs`, `tests/provider_native_contract.rs`. |
| `src/provider_metadata.rs` | Provider metadata | Unit; `tests/provider_metadata_comprehensive.rs`, `tests/provider_registry_guardrails.rs`. |
| `src/providers/anthropic.rs` | Anthropic provider | Unit; `tests/provider_streaming/anthropic.rs`, `tests/e2e_provider_streaming.rs`. |
| `src/providers/anthropic/transport.rs` | Anthropic Messages transport shared with Vertex (SSE lifecycle validation) | Unit (in-module, 15 tests). |
| `src/providers/azure.rs` | Azure provider | Unit; `tests/provider_streaming/azure.rs`, provider error/path suites. |
| `src/providers/azure_terminal_safety_tests.rs` | Azure streamed tool-call finalization and terminal error preservation | Unit (in-module, terminal safety tests). |
| `src/providers/bedrock.rs` | Bedrock provider | Unit; provider native/contract suites; public-provider HTTP tests cover native thinking/cache controls, hook fallback, exact rewritten-body SigV4 signing, and JSON cache accounting (added, not executed on the editing host). |
| `src/providers/bedrock/request_options.rs` | Model-aware Bedrock thinking budgets/adaptive effort and native prompt-cache checkpoints | Unit (12 in-module request-shape tests); public-provider HTTP coverage in `src/providers/bedrock.rs`. These tests were added but not executed on the editing host because DSR is unavailable. |
| `src/providers/bedrock/streaming.rs` | Incremental AWS binary event-stream validation and ConverseStream message lifecycle | Unit; `tests/provider_bedrock_streaming.rs` exercises public-provider incremental delivery, signed/redacted reasoning replay, malformed frames, and terminal handling. No fresh DSR execution is claimed by this inventory update. |
| `src/providers/cohere.rs` | Cohere provider | Unit; `tests/provider_streaming/cohere.rs`, provider error/path suites; public-provider multimodal/tool HTTP tests in `src/providers/cohere/request_options.rs`. |
| `src/providers/cohere/request_options.rs` | Native Cohere image payloads and thinking budgets | Unit (10 request/validation tests plus 9 public-provider HTTP tests), including first-delta delivery before completion, parallel tool/image replay, request hooks and credential redaction. Added, not executed: DSR unavailable. |
| `src/providers/cohere/streaming.rs` | Bounded Cohere v2 indexed content/tool lifecycle and terminal stream handling | Unit (16 tests), including interleaved calls, malformed arguments, sparse indices, byte/block limits, native completion and one-error-then-EOF behavior. Added, not executed: DSR unavailable. |
| `src/providers/copilot.rs` | Copilot provider | Unit; provider native/contract suites. |
| `src/providers/cursor.rs` | Cursor Connect provider | Unit; `tests/provider_smoke_matrix.rs`, `tests/provider_native_contract.rs`, and provider factory suites. |
| `src/providers/cursor/connect.rs` | Connect response admission and terminal validation for the Cursor provider | Unit (28 in-module tests). |
| `src/providers/cursor/stream_limits.rs` | Whole-response admission for Cursor streams, not just individual frames | Unit (19 in-module tests). |
| `src/providers/cursor/wire_tests.rs` | Regression coverage for atomic, bounded Cursor protobuf decoding | Test module (16 tests). |
| `src/providers/extension_stream.rs` | Admission and terminal semantics for extension-authored provider streams | Unit; `src/providers/extension_stream/tests.rs`, `tests/extension_stream_terminal.rs`, `tests/extensions_provider_streaming.rs`. |
| `src/providers/extension_stream/blocks.rs` | Per-block admission for structured extension streams | Unit (8 in-module tests). |
| `src/providers/extension_stream/tests.rs` | Extension stream decoder test suite | Test module; extension stream terminal and admission tests. |
| `src/providers/gemini.rs` | Gemini provider | Unit; `tests/provider_streaming/gemini.rs`, provider error/path suites. |
| `src/providers/gemini/files.rs` | Credential-scoped Gemini Files API staging for large inline media | Unit (in-module, loopback HTTP fixtures); two concurrency cases are timing-flaky, see bd-eg6ng. |
| `src/providers/gemini/reasoning.rs` | Gemini reasoning and thinking trace handling | Unit; `tests/provider_streaming/gemini.rs`. |
| `src/providers/gemini/tests/integration_reasoning.rs` | Gemini streaming reasoning integration tests | Test module; streaming reasoning tests. |
| `src/providers/gemini/thinking.rs` | Gemini thinking-budget mapping | Unit (in-module). |
| `src/providers/gemini/wire.rs` | Gemini wire types and serialization | Unit; in-module tests. |
| `src/providers/gitlab.rs` | GitLab Duo provider | Unit; `tests/provider_streaming/gitlab.rs`, provider error/path suites. |
| `src/providers/mod.rs` | Provider factory | Unit; `tests/provider_factory.rs`, `tests/provider_native_verify.rs`; branch export baseline marks this family partly branch-SIGSEGV fallback. |
| `src/providers/model_fetch.rs` | Live provider-model discovery and cache | Unit tests in this module; static-registry integration through `tests/model_registry.rs`. |
| `src/providers/openai.rs` | OpenAI chat provider | Unit; `tests/provider_streaming/openai.rs`, provider error/path suites. |
| `src/providers/openai_deepseek.rs` | Recovery of DeepSeek tool calls emitted in the text channel | No in-module tests; reached through `src/providers/openai.rs`. |
| `src/providers/openai_deepseek_tests.rs` | Terminal and fragmented-stream regressions for DeepSeek text tool recovery | Test module (16 tests). |
| `src/providers/openai_terminal_safety_tests.rs` | OpenAI streamed tool argument validation and stream failure preservation | Unit (in-module, terminal safety tests). |
| `src/providers/openai_responses.rs` | OpenAI Responses provider | Unit; `tests/provider_streaming/openai_responses.rs`, provider error/path suites. |
| `src/providers/vertex.rs` | Vertex provider | Unit; provider native/contract suites. |
| `src/providers/vertex/tests_transport.rs` | Vertex Anthropic transport tests over real loopback HTTP/SSE | Test module; one case is timing-flaky under parallel execution, see bd-eg6ng. |
| `src/resource_governor.rs` | Resource governor | Unit; `tests/cargo_headroom_admission.rs`, `tests/resource_edge_cases.rs`; traceability lane `resource_scheduler_admission`. |
| `src/resources.rs` | Resource loading | Unit; `tests/resource_loader.rs`, `tests/resource_edge_cases.rs`; traceability lane `resource_scheduler_admission`. |
| `src/review.rs` | Review workflow | `tests/review.rs`. |
| `src/review/hosted.rs` | Explicit hosted-review targets and bounded, noninteractive forge reads | Unit (7 in-module tests). |
| `src/rpc.rs` | RPC/stdin mode | Unit; `tests/rpc_mode.rs`, `tests/rpc_protocol.rs`, `tests/rpc_edge_cases.rs`, `tests/e2e_rpc.rs`. |
| `src/rpc/attachments.rs` | Borrowed RPC media projection and bounded attachment retrieval for negotiated references | Unit (9 in-module tests). |
| `src/scheduler.rs` | Scheduler/admission | Unit; `tests/scheduler_repro.rs`, `tests/cargo_headroom_admission.rs`; traceability lane `resource_scheduler_admission`. |
| `src/sdk.rs` | SDK API | Unit; `tests/sdk_api.rs`, `tests/sdk_integration.rs`, `tests/sdk_unit.rs`. |
| `src/sdk/extension_bootstrap.rs` | SDK session extension model resolution and runtime bootstrap | Unit; `tests/sdk_integration.rs`. |
| `src/sdk/extension_bootstrap_tests.rs` | SDK extension bootstrap regression test suite | Test module; extension model resolution tests. |
| `src/sdk/recovery.rs` | Retry and failover recovery for every SDK turn entry point | Unit; `src/sdk/tests/recovery.rs`, `tests/sdk_multimodal.rs`. |
| `src/sdk/recovery/ownership.rs` | Abandoned-turn quarantine: a dropped SDK turn future keeps provider re-entry closed until the session is reconstructed | Unit (9 in-module tests). |
| `src/sdk/tests/recovery.rs` | SDK entry-point recovery regression suite | Test module; retry, failover, abort and persistence-quarantine tests. |
| `src/secrets.rs` | Secret handling | `tests/secrets.rs`. |
| `src/secrets/structured.rs` | Transactional secret screening for structured outbound values | Unit (14 in-module tests); `tests/secrets.rs`. |
| `src/security_scan.rs` | Agent-facing security scanner tool: plan/run/disposition/compare (bd-cv653.2.6) | Unit (6 tests); `tests/security_scan.rs`. |
| `src/security_scan/dependencies.rs` | Cargo dependency security analysis | Unit; `tests/security_scan.rs`. |
| `src/security_scan/dependencies/tests.rs` | Dependency security scanning tests | Test support module; exercises dependency scanner. |
| `src/security_scan/dependencies/python.rs` | Offline `uv.lock` and `poetry.lock` inventory for `dependency_plan` | Unit (`python/tests.rs`). |
| `src/security_scan/dependencies/python/tests.rs` | Python lockfile inventory tests | Test support module; parser schemas, normalization, variants, privacy, limits, discovery, confinement. |
| `src/security_scan/dependency_review.rs` | Offline comparison and review of native dependency-audit artifacts | Unit; `src/security_scan/dependency_review/tests.rs`, `src/security_scan/dependency_review/tests/wire.rs`. |
| `src/security_scan/dependency_review/disposition.rs` | Explicit, version-bound dependency triage written as a new SARIF artifact | Unit; `src/security_scan/dependency_review/disposition/tests.rs`. |
| `src/security_scan/dependency_review/disposition/tests.rs` | Dependency disposition test suite | Test module; disposition tests (8 tests). |
| `src/security_scan/dependency_review/snapshot.rs` | Validation of native dependency-audit SARIF before it is used as a baseline | No in-file tests; exercised by `src/security_scan/dependency_review/tests.rs` and `src/security_scan/dependency_review/disposition/tests.rs`. |
| `src/security_scan/dependency_review/tests.rs` | Dependency review test suite and audit fixture builder | Test module; comparison, tampering and confined-reader tests (9 tests). |
| `src/security_scan/dependency_review/tests/wire.rs` | Dependency review wire test cases | Test module; real audit exporter against a loopback OSV peer through the public comparison operation (2 tests, Unix only). |
| `src/security_scan/osv.rs` | OSV vulnerability database client | Unit; `tests/security_scan.rs`. |
| `src/security_scan/osv/artifact.rs` | OSV vulnerability report artifact serialization | Unit; `tests/security_scan.rs`. |
| `src/security_scan/osv/tests.rs` | OSV client test cases | Test support module; exercises OSV integration. |
| `src/security_scan/source.rs` | Source code vulnerability analysis | Unit; `tests/security_scan.rs`. |
| `src/security_scan/source/operation.rs` | Revocable ownership of synchronous source work dispatched to a blocking worker | Unit (9 in-module tests). |
| `src/security_scan/source/storage.rs` | Confined persistence for source reports and review decisions | `src/security_scan/source/storage/tests.rs` (10 tests). |
| `src/security_scan/source/storage/tests.rs` | Test suite for `src/security_scan/source/storage.rs` | Test module (10 tests). |
| `src/self_update.rs` | Self-update | `tests/self_update.rs`. |
| `src/semantic_workspace_graph.rs` | Semantic workspace graph and context bundles | Unit; `tests/semantic_workspace_graph_contract.rs`, `tests/semantic_workspace_graph_builder.rs`, and agent integration tests. |
| `src/session.rs` | Session JSONL/tree | Unit; `tests/session_conformance.rs`, `tests/e2e_session_persistence.rs`; branch export baseline marks this as branch-SIGSEGV fallback. |
| `src/session/attachments.rs` | Immutable image and media attachment blobs for JSONL sessions and their V2 index | Unit (25 in-module tests). |
| `src/session/fork_tests.rs` | Session fork workspace-provenance test suite and shared fixtures for the SDK, RPC and classic/FTUI fork entrypoints | Test module; fork tests (4 tests). |
| `src/session_control.rs` | Session control, live steering, follow-up, and input retraction | Unit; `src/session_control/agent_tests.rs`. |
| `src/session_control/agent_tests.rs` | Session control agent integration test suite | Test module; session control agent tests. |
| `src/session_control/agent_tests/deadline_recovery.rs` | Session control turn deadline recovery test cases | Test support module; exercises turn timeout recovery. |
| `src/session_control/attachments.rs` | Session control media attachment admission and authorship tracking | Unit; in-module tests. |
| `src/session_control/deadline.rs` | Session control turn deadline management and sleep futures | Unit; `src/session_control/deadline/tests.rs`, `src/session_control/agent_tests.rs`. |
| `src/session_control/deadline/tests.rs` | Turn deadline unit test cases | Test module; exercises turn timeouts. |
| `src/session_control/execution.rs` | Owned live-turn execution: keeps turns on their initiating owner and drains cooperative aborts | Unit; `src/session_control/execution/tests.rs`, `src/session_control/execution/agent_tests.rs`. |
| `src/session_control/execution/agent_tests.rs` | Owned live-turn execution tests against real agent turns and wire streams | Test module; exercises initiator cancellation and native drain. |
| `src/session_control/execution/native_input_tests.rs` | Native-input entrypoints through the real SDK, Agent, OpenAI adapter and HTTP/SSE parser | Test module (4 tests). |
| `src/session_control/execution/tests.rs` | Owned live-turn execution unit test cases | Test module; exercises owner migration, pre-poll cancellation and abort drain. |
| `src/session_control/recovery.rs` | Session control turn recovery and input reconstruction | Unit; `src/session_control/agent_tests.rs`. |
| `src/session_import.rs` | Session import | `tests/session_import.rs`. |
| `src/session_import/conversion.rs` | Session import format conversion and mapping | Unit; `tests/session_import.rs`. |
| `src/session_import/history.rs` | Select one foreign conversation before converting or reconciling tools | `src/session_import/history/tests.rs` (19 tests). |
| `src/session_import/history/tests.rs` | Test suite for `src/session_import/history.rs` | Test module (19 tests). |
| `src/session_import/transcript.rs` | Session import transcript parser | Unit; `tests/session_import.rs`. |
| `src/session_index.rs` | Session index | Unit; `tests/session_index_tests.rs`, `tests/reproduce_index_gap.rs`. |
| `src/session_metrics.rs` | Session metrics | Unit; `tests/provider_session_coverage.rs` and session evidence suites. |
| `src/session_picker.rs` | Session picker UI | Unit; `tests/session_picker.rs`. |
| `src/session_picker/browse.rs` | Session picker browse state and navigation | Unit; `tests/session_picker.rs`. |
| `src/session_picker/browse_controls_tests.rs` | Session picker keyboard navigation and filter controls tests | Test module; in-module unit tests. |
| `src/session_recovery.rs` | Explicit, read-only text recovery from an authoritative JSONL transcript | `src/session_recovery/tests.rs` (15 tests). |
| `src/session_recovery/export.rs` | Bounded inspection exports, deliberately separate from session persistence | Unit (8 in-module tests). |
| `src/session_recovery/export/session_copy.rs` | Explicit, create-only text recovery forks | `src/session_recovery/export/session_copy/tests.rs` (9 tests). |
| `src/session_recovery/export/session_copy/tests.rs` | Test suite for `src/session_recovery/export/session_copy.rs` | Test module (9 tests). |
| `src/session_recovery/strict_json.rs` | Bounded JSON parsing that refuses duplicate keys before Value loses them | No in-module tests; reached through `src/session_recovery.rs`. |
| `src/session_recovery/tests.rs` | Test suite for `src/session_recovery.rs` | Test module (15 tests). |
| `src/session_sqlite.rs` | SQLite session backend | Unit; `tests/session_sqlite.rs`, `tests/fault_injection_persistence.rs`; branch export baseline marks this as branch-SIGSEGV fallback. |
| `src/session_sqlite/attachments.rs` | SQLite session attachment and media blob storage | Unit; `tests/session_sqlite.rs`. |
| `src/session_sqlite/entry_io.rs` | SQLite session entry serialization, paging, and batch insertion | Unit; `tests/session_sqlite.rs`. |
| `src/session_store_v2.rs` | Session store v2 | Unit; `tests/session_store_v2.rs`, `tests/session_store_v2_contract.rs`. |
| `src/session_test.rs` | Session test helpers | Waived test-support module; compiled by session tests. |
| `src/session_title.rs` | Best-effort session names from a bounded, private auxiliary completion | Unit; `src/session_title/tests.rs`. |
| `src/session_title/tests.rs` | Session title test suite | Test module; scripted provider replies with production privacy, admission and durable session writes (17 tests). |
| `src/session_workdir.rs` | Explicit, provenance-preserving session workdir recovery and attachment | Unit (11 in-module tests); `tests/session_workdir_recovery.rs`, `tests/session_index_tests.rs`. |
| `src/session_workdir/persisted.rs` | Saved-session workdir discovery, including lazily loaded V2 stores | Unit (8 in-module tests); `tests/session_workdir_recovery.rs`, `tests/provider_bedrock_streaming.rs`. |
| `src/skills_managed.rs` | Managed skills | `tests/skills_managed.rs`. |
| `src/sse.rs` | SSE parser | Unit; `tests/sse_strict_compliance.rs`, `tests/repro_sse_flush.rs`, `tests/repro_sse_newline.rs`. |
| `src/stats.rs` | Local usage statistics over session files (`pi stats`, bd-cv653.7.7) | Unit (8 tests); no dedicated integration test. |
| `src/status_line.rs` | Powerline status line, footer, and sticky HUDs (OMP-ADOPT / bd-cv653.9.4) | Unit (7 tests); `tests/status_line.rs`, `tests/chrome_tui_integration.rs`. |
| `src/stream_rules.rs` | Stream rules | `tests/stream_rules.rs`. |
| `src/subagents.rs` | Native isolated child-agent tool | Unit tests in this module; opt-in registration coverage through built-in tool tests. |
| `src/subagents/background.rs` | Explicit background delegation over the ordinary session-owned jobs runtime | Unit (7 in-module tests). |
| `src/subagents/bundled.rs` | Built-in delegation profiles, using the ordinary native child runner | Unit (8 in-module tests). |
| `src/subagents/deadline.rs` | Child subagent deadline and timeout tracking | Unit; `tests/subagent.rs`. |
| `src/subagents/deadline_tests.rs` | Subagent deadline test cases | Test support module; exercises deadline enforcement. |
| `src/subagents/definition.rs` | Typed, bounded agent Markdown definitions | `src/subagents/definition/tests.rs` (18 tests). |
| `src/subagents/definition/tests.rs` | Test suite for `src/subagents/definition.rs` | Test module (18 tests). |
| `src/subagents/execution.rs` | Subagent process execution and lifecycle | Unit; `tests/subagent.rs`. |
| `src/subagents/execution/ownership.rs` | Subagent execution ownership and registration checkpointing | Unit; `tests/subagent.rs`. |
| `src/subagents/execution/pipes.rs` | Subagent standard I/O pipe framing and drain loops | Unit; `tests/subagent.rs`. |
| `src/subagents/execution/worker.rs` | One real RPC process per retained worker | Unit (7 in-module tests). |
| `src/subagents/execution_tests.rs` | Subagent execution tests | Test support module; exercises subagent runner. |
| `src/subagents/protocol.rs` | Subagent inter-process protocol messages | Unit; `tests/subagent.rs`. |
| `src/subagents/seance.rs` | Read-only consultation of one saved session through the native child runner | Unit (12 in-module tests). |
| `src/subagents/worker.rs` | Session-owned, reusable RPC children | Unit (5 in-module tests). |
| `src/subagents/worker/output_contract.rs` | Strict output contracts for retained RPC conversations | Unit (5 in-module tests); `src/subagents/worker/output_contract/native_tests.rs` (12 tests). |
| `src/subagents/worker/output_contract/native_tests.rs` | Actual SubagentTool admission, RPC children, retained results and privacy | Test module (12 tests). |
| `src/subagents/worker/pool.rs` | A session-owned pool is one native job, including its idle RPC children, admission queue and ordered result manifest | Unit (14 in-module tests). |
| `src/subagents/workflow.rs` | Dependency-ordered native delegation, not a second child runtime | `src/subagents/workflow/background_tests.rs` (8 tests); `src/subagents/workflow/tests.rs` (25 tests). |
| `src/subagents/workflow/background_tests.rs` | Exercise actual background admission, jobs and the native DAG runner | Test module (8 tests). |
| `src/subagents/workflow/decision.rs` | Typed branch selection over settled native results | `src/subagents/workflow/decision/tests.rs` (19 tests). |
| `src/subagents/workflow/decision/tests.rs` | Test suite for `src/subagents/workflow/decision.rs` | Test module (19 tests). |
| `src/subagents/workflow/fanout.rs` | Bounded data-driven task expansion through the ordinary wave runner | `src/subagents/workflow/fanout/batch_tests.rs` (14 tests); `src/subagents/workflow/fanout/tests.rs` (21 tests). |
| `src/subagents/workflow/fanout/batch_tests.rs` | Explicit batches reuse the native wave runner and its acceptance contract | Test module (14 tests). |
| `src/subagents/workflow/fanout/tests.rs` | Test suite for `src/subagents/workflow/fanout.rs` | Test module (21 tests). |
| `src/subagents/workflow/progress.rs` | Recover accepted wave evidence without inventing a second workflow runtime | `src/subagents/workflow/progress/tests.rs` (8 tests). |
| `src/subagents/workflow/progress/tests.rs` | Actual native DAG/job paths with controlled child executables, not model tests | Test module (8 tests). |
| `src/subagents/workflow/selected_inputs.rs` | Explicit data handoff across mutually exclusive workflow branches | `src/subagents/workflow/selected_inputs/tests.rs` (6 tests). |
| `src/subagents/workflow/selected_inputs/tests.rs` | Test suite for `src/subagents/workflow/selected_inputs.rs` | Test module (6 tests). |
| `src/subagents/workflow/selected_inputs/tests/native.rs` | Test suite for `src/subagents/workflow/selected_inputs/tests.rs` | Test module (4 tests). |
| `src/subagents/workflow/tests.rs` | Test suite for `src/subagents/workflow.rs` | Test module (25 tests). |
| `src/subagents/workspace.rs` | Admit workspace controls before discovery, snapshot I/O or child launch | Unit (11 in-module tests). |
| `src/swarm_activity_ledger.rs` | Swarm activity ledger | Unit; evidence docs in `docs/swarm-activity-ledger.md`, CI evidence bundle tests. |
| `src/swarm_flight_recorder.rs` | Swarm flight recorder | Unit and E2E; `tests/e2e_swarm_flight_recorder.rs` covers deterministic multi-agent replay artifacts. |
| `src/swarm_progress_slo.rs` | Swarm progress SLO evaluation | Unit; `tests/swarm_progress_slo_contract.rs`, `tests/swarm_progress_cli.rs`, and `tests/swarm_progress_slo_e2e.rs`. |
| `src/swarm_replay.rs` | Swarm trace replay and policy comparison | Unit; `tests/swarm_replay_trace_contract.rs`, `tests/swarm_replay_ingestor.rs`, and `tests/swarm_replay_preview_cli.rs`. |
| `src/terminal_images.rs` | Terminal images | Unit; interactive/TUI rendering tests. |
| `src/text_completion.rs` | Bounded, terminal-validated completions for tool-free auxiliary model calls | Unit (23 in-module tests). |
| `src/text_completion/request.rs` | Owner-scoped deadlines for inline auxiliary provider requests | Unit (8 in-module tests). |
| `src/theme.rs` | Theme loading | Unit; `tests/tui_snapshot.rs`, interactive UI tests. |
| `src/todo.rs` | Todo tracking | Covered through TUI/session suites. |
| `src/token_count.rs` | BPE token counting | In-source tests; `tests/compaction.rs` cut-point calibration. |
| `src/tools.rs` | Built-in tools | Unit; `tests/tools_conformance.rs`, `tests/e2e_tools.rs`, `tests/tools_hardened.rs`; branch export baseline marks this as branch-SIGSEGV fallback. |
| `src/tools/bash_session.rs` | Session-owned Bash state without replaying earlier commands | Unit (2 in-module tests). |
| `src/tools/structured_json.rs` | Native jq-compatible Read queries and their resource-bounded worker owner | Unit (19 in-module tests). |
| `src/tools/structured_sqlite.rs` | Read SQLite data from an immutable, workspace-admitted database image | Unit (16 in-module tests). |
| `src/tui.rs` | Terminal renderer | Unit; `tests/tui_snapshot.rs`, `tests/tui_state.rs`, `tests/e2e_tui.rs`. |
| `src/turn_recovery.rs` | Turn recovery | `tests/e2e_interruption_resume_replay.rs`; recovery suites. |
| `src/undo.rs` | Undo | Covered through session/TUI suites. |
| `src/url_read.rs` | URL read tool | `tests/e2e_url_read.rs`. |
| `src/url_read/navigation.rs` | Source-aware document retrieval | `src/url_read/navigation/tests.rs` (18 tests). |
| `src/url_read/navigation/tests.rs` | Test suite for `src/url_read/navigation.rs` | Test module (18 tests). |
| `src/url_read/notebook.rs` | Read saved notebook results without running a kernel, MIME hooks or widgets | `src/url_read/notebook/tests.rs` (10 tests). |
| `src/url_read/notebook/tests.rs` | Test suite for `src/url_read/notebook.rs` | Test module (10 tests). |
| `src/url_router.rs` | URL routing | `tests/url_router.rs`. |
| `src/url_router/agent.rs` | Read-only views of native child results, published by the acceptance lease | Unit (8 in-module tests). |
| `src/url_router/conflicts.rs` | Lossless conflict inspection and optimistic, staged resolution (bd-cv653.6.3) | `src/url_router/conflicts/tests.rs` (7 tests). |
| `src/url_router/conflicts/io.rs` | Bounded, cancellable Git queries and regular-file snapshot admission | No in-module tests; reached through `src/url_router/conflicts.rs`. |
| `src/url_router/conflicts/parser.rs` | Byte spans, not reconstructed lines: only a selected marker block is removed | Unit (5 in-module tests). |
| `src/url_router/conflicts/selection.rs` | Explicit, snapshot-bound choices | `src/url_router/conflicts/selection/tests.rs` (8 tests). |
| `src/url_router/conflicts/selection/tests.rs` | Test suite for `src/url_router/conflicts/selection.rs` | Test module (8 tests). |
| `src/url_router/conflicts/tests.rs` | Test suite for `src/url_router/conflicts.rs` | Test module (7 tests). |
| `src/url_router/documents.rs` | Internal URL scheme router (bd-cv653.6.3) | Unit (26 in-module tests). |
| `src/url_router/skills.rs` | Skill instructions, supporting files, and bounded directory inventories | Unit (9 in-module tests). |
| `src/usage.rs` | Usage accounting | Covered through agent-loop and session suites. |
| `src/validation_broker.rs` | Validation admission and slot broker | Unit; `tests/validation_broker_contract.rs`, `tests/validation_broker_store.rs`, `tests/validation_broker_cli.rs`, and `tests/validation_broker_e2e.rs`. |
| `src/validation_broker/doctor.rs` | Native `pi doctor` reports as validation-admission inputs | Unit (22 in-module tests). |
| `src/vcr.rs` | VCR playback/record | Unit; `tests/vcr_parity_validation.rs`, `tests/vcr_redaction_scan.rs`, provider/RPC VCR suites. |
| `src/version_check.rs` | Version checks | Unit; cross-platform and release-readiness tests exercise the surrounding behavior. |
| `src/web_remote.rs` | Web-remote access: ftui-web WASM browser client over WebSocket frame diffs (OMP-ADOPT / bd-cv653.10.1, .10.2) | Unit (3 tests); `tests/web_remote.rs`, `tests/web_security.rs`. |
| `src/web_search.rs` | Web search tool | `tests/e2e_web_search.rs`, `tests/web_search_rungs.rs`. |
| `src/workspace.rs` | Multi-root workspace state and the unified path-confinement helper (bd-cv653.3.12) | Unit (7 tests); `tests/tools_conformance.rs`, `tests/main_cli_selection.rs`, `tests/branch_edge_failure_coverage.rs`. |
| `src/workspace_trust.rs` | Workspace trust | Covered through config/CLI suites. |
| `src/worktree_iso.rs` | Worktree isolation | `tests/worktree_iso.rs`. |
| `src/worktree_iso/snapshot.rs` | Worktree isolation snapshot and rollback | Unit; `tests/worktree_iso.rs`. |
| `src/worktree_iso/snapshot/sparse.rs` | Preserve index-only tracked files without treating sparse absence as deletion | `src/worktree_iso/snapshot/sparse/tests.rs` (12 tests). |
| `src/worktree_iso/snapshot/sparse/tests.rs` | Test suite for `src/worktree_iso/snapshot/sparse.rs` | Test module (12 tests). |
| `src/worktree_iso/snapshot_tests.rs` | Worktree isolation snapshot test cases | Test support module; exercises snapshotting. |
| `src/xdev.rs` | xdev tool development | `tests/e2e_xdev.rs`. |

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
