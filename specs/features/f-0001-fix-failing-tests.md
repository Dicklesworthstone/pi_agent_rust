# f-0001: Fix Failing Tests

> Epic: [Rust Investment Roadmap](../epics/rust-investment-roadmap.md) — Item 1
> Priority: Immediate | Effort: 1-2 days

---

## Problem

12 of 6,199 tests fail on a clean checkout. These mask real regressions and
erode confidence in the test suite. Most failures appeared after upstream
commits `eaa865c0..69ac3dcd` introduced branch-aware model/thinking state.

## Failing Tests (grouped by root cause)

### Group A: Branch-aware model state (6 tests)

These all relate to the new per-branch model and thinking-level persistence
introduced in `eaa865c0` ("branch-local model and thinking-level state").

```
app::tests::select_model_and_thinking_preserves_restore_warning_when_defaulting_for_setup
app::tests::select_model_and_thinking_preserves_restore_warning_when_using_config_default
app::tests::select_model_and_thinking_restores_model_from_header_when_history_missing
app::tests::select_model_and_thinking_restores_model_from_active_branch_only
interactive::ext_session::tests::branch_without_overrides_does_not_inherit_stale_header_state
session::tests::test_navigation_clears_stale_header_metadata_when_target_branch_has_no_override
```

**Files to investigate:**
- `src/app.rs` — `select_model_and_thinking()` function
- `src/session.rs` — branch navigation and header metadata clearing
- `src/interactive/ext_session.rs` — branch override inheritance

**Likely cause:** The branch-aware state changes altered how model/thinking
fields propagate through branches. Tests expect the old behavior where
session header fields were global; the new code scopes them per-branch.

### Group B: Model registry (2 tests)

```
models::tests::built_in_models_preserve_legacy_model_display_names
session::tests::test_session_handle_preserves_alias_equivalent_model_state
```

**Files to investigate:**
- `src/models.rs:2122` — `openrouter/openrouter/auto` expects "Auto Router", gets "Auto"
- `src/session.rs` — alias equivalence for model state

**Likely cause:** A model display name was changed upstream or a new alias
was added without updating the test expectations.

### Group C: Spill file handling (2 tests)

```
rpc::retry_tests::rpc_spill_file_hard_limit_abandons_partial_spill_file
tools::tests::test_bash_hard_limit_retains_partial_spill_file
```

**Files to investigate:**
- `src/rpc.rs:3553` — assertion `temp_file_path.is_none()` fails
- `src/tools.rs:6906` — assertion `!bash_output.spill_failed` fails

**Likely cause:** Spill file cleanup logic race condition or changed semantics
in the hard-limit path. The spill file is being retained when the test expects
abandonment, or vice versa.

### Group D: Autocomplete (1 test)

```
autocomplete::tests::suggest_slash_alone_returns_all_builtins
```

**File:** `src/autocomplete.rs`

**Likely cause:** A slash command was added or removed without updating the
expected completions list.

### Group E: Extension dispatcher (1 test)

```
extension_dispatcher::tests::dispatcher_tool_find_discovers_files
```

**File:** `src/extension_dispatcher.rs`

**Likely cause:** The `find` tool integration in the dispatcher may depend
on filesystem state or a missing test fixture.

## Implementation Plan

1. Run each group's tests in isolation with `RUST_BACKTRACE=1` to get full
   stack traces
2. For Group A: read the branch-aware state commits (`eaa865c0`, `8be7cacb`)
   and reconcile test expectations with the new behavior
3. For Group B: update display name expectations to match current model registry
4. For Group C: add debug logging to spill file path, check for timing issues
5. For Group D: diff the slash command list against test expectations
6. For Group E: check fixture directory and filesystem assumptions

## Acceptance Criteria

- `cargo test` reports 0 failures
- No tests are `#[ignore]`d to achieve this — fix the actual issues
- Each fix is validated to not break other tests in the same module
