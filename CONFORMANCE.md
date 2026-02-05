# Conformance Testing Strategy

> **Purpose:** Document how pi_agent_rust validates behavioral compatibility with Pi Agent (TypeScript).

## Overview

pi_agent_rust must behave identically to the TypeScript reference implementation for all observable behaviors. This document describes the conformance testing approach used to verify this compatibility.

## Testing Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                        Test Layers                                   │
├─────────────────────────────────────────────────────────────────────┤
│                                                                     │
│   ┌─────────────────┐   ┌─────────────────┐   ┌─────────────────┐  │
│   │   Unit Tests    │   │  Conformance    │   │  Integration    │  │
│   │   (src/*.rs)    │   │  Tests          │   │  Tests          │  │
│   │                 │   │  (fixtures)     │   │  (E2E)          │  │
│   └────────┬────────┘   └────────┬────────┘   └────────┬────────┘  │
│            │                     │                     │            │
│   Tests internal        Tests observable       Tests full          │
│   logic in isolation    behavior vs fixtures   agent workflow      │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
```

## Test Categories

### 1. Unit Tests (`cargo test --lib`)

Location: `src/*.rs` inline `#[cfg(test)]` modules

**Coverage:**
- Message type serialization/deserialization
- SSE parser edge cases
- Truncation algorithms
- Path resolution
- Provider message conversion
- Package manager source parsing/identity + settings updates
- Skills loader + prompt template expansion

**Count:** 35+ tests

### 2. Conformance Tests (Fixture-Based)

Location: `tests/conformance/`

**Purpose:** Verify tool behavior matches TypeScript reference.

**Fixture Format:**
```json
{
  "version": "1.0",
  "tool": "read",
  "description": "Conformance tests for the read tool",
  "cases": [
    {
      "name": "read_simple_file",
      "setup": [
        {"type": "create_file", "path": "test.txt", "content": "hello"}
      ],
      "input": {"path": "test.txt"},
      "expected": {
        "content_exact": "hello",
        "details_none": true
      }
    }
  ]
}
```

**Fixture Files:**
| Tool | File | Cases |
|------|------|-------|
| read | `read_tool.json` | 5+ |
| bash | `bash_tool.json` | 10+ |
| edit | `edit_tool.json` | 8+ |
| write | `write_tool.json` | 6+ |
| grep | `grep_tool.json` | 8+ |
| find | `find_tool.json` | 5+ |
| ls | `ls_tool.json` | 5+ |
| truncation | `truncation.json` | 10+ |

### 3. Integration Tests

Location: `tests/*.rs`

**Purpose:** End-to-end testing of agent workflows.

**Coverage (current):**
- `tests/rpc_mode.rs`: RPC protocol sanity (get_state, prompt streaming events, get_session_stats)
- `tests/e2e_cli.rs`: headless CLI smoke (print mode, selection paths)
- `tests/provider_streaming.rs`: VCR-backed provider streaming playback (Anthropic/OpenAI/Gemini/Azure)
- `tests/compaction.rs`: compaction engine behavior with scripted provider

**Planned:**
- Fixture-based RPC conformance harness comparing Rust RPC responses/events against the TypeScript reference (`legacy_pi_mono_code/pi-mono/packages/coding-agent/docs/rpc.md`).

### 4. Extension Conformance (Differential Oracle)

Location: `tests/ext_conformance_diff.rs` + `tests/ext_conformance/`

**Purpose:** Validate extension runtime behavior (registration/events/hostcalls) against the TypeScript reference by running the SAME extension in BOTH the TS oracle (Bun + jiti) and Rust QuickJS runtime, then comparing registration snapshots.

**Results (2026-02-05):**

| Corpus | Passed | Total | Rate | Notes |
|--------|--------|-------|------|-------|
| Official | 60 | 60 | 100% | All pass, test runs in CI |
| Community | 53 | 58 | 91.4% | 53/53 testable pass; 5 TS oracle env failures |

**Community TS oracle failures (environment issues, not Rust bugs):**
- `nicobailon-interactive-shell`: requires native `pty.node` module
- `nicobailon-interview-tool`: missing `form/index.html` file
- `qualisero-background-notify`: missing `../../shared` module
- `qualisero-pi-agent-scip`: missing `./dist/extension.js`
- `qualisero-safe-git`: missing `../../shared` module

**Key runtime features enabling conformance:**
- In-memory virtual filesystem (`__pi_vfs`) for `node:fs`
- CJS-to-ESM transformation shim for CommonJS extensions
- `createRequire` resolves actual builtin modules
- Virtual module stubs: `shell-quote`, `vscode-languageserver-protocol`, `@modelcontextprotocol/sdk`
- Comprehensive node polyfills: `fs`, `path`, `os`, `crypto`, `url`, `process`, `buffer`, `child_process`

Current building blocks:
- Differential test runner (`tests/ext_conformance_diff.rs`)
- TS oracle harness (`tests/ext_conformance/ts_harness/run_extension.ts`)
- Vendored artifacts (`tests/ext_conformance/artifacts/*`)
- Deterministic PiJS scheduler conformance (`tests/event_loop_conformance.rs`)

### 4A. Extension Conformance Matrix + Test Plan (bd-2kyq)

This section turns the **extension taxonomy** (see `EXTENSIONS.md` §1B) into a
concrete conformance matrix and a test plan. The goal is to ensure **every
extension shape** has **explicit, testable pass/fail criteria** and **fixture
coverage**.

#### Conformance Matrix (shape × capability × expected behaviors)

| Extension Shape | Entrypoint / Config | Required Capabilities / I/O | Expected Behaviors (Pass/Fail) | Coverage (Current / Planned) |
|---|---|---|---|---|
| **PiJS (JS/TS)** | `extension.json` (`pi.ext.manifest.v1`) or package manifest; entry `.ts/.js` | `tool` (→ `read/write/exec`), `http`, `session`, `ui`, `log` | **PASS** if: registrations match (tools/commands/flags/shortcuts/providers); derived capability matches hostcall method (see `EXTENSIONS.md` §3.2A); deterministic event ordering per scheduler contract; mock outputs deterministic under fixed spec; errors map to taxonomy (`timeout/denied/io/invalid_request/internal`). | **Current:** `tests/e2e_extension_registration.rs`, `tests/extensions_registration.rs`, `tests/ext_conformance.rs`, `tests/event_loop_conformance.rs`, `tests/ext_conformance/event_payloads/event_payloads.json`, `tests/ext_conformance/mock_specs/*`, `tests/ext_conformance_fixture_schema.rs`. **Planned:** differential TS↔Rust runner (`bd-21dv`). |
| **WASM Component** | `extension.json` with `runtime="wasm"`; entry `.wasm` component | WIT hostcalls → same capability set as PiJS | **PASS** if: registration + hostcall behavior matches PiJS contract; capability derivation identical to JS; deterministic logs; error taxonomy identical. | **Planned:** WASM host conformance + parity suite (`bd-nom`, `bd-320`). |
| **MCP Server** | MCP config or CLI args (stdio/http/sse) | MCP protocol (tools list + tool call/response); policy-gated connectors | **PASS** if: tool schemas discoverable; tool calls execute with deterministic mocks; policy denials surfaced as MCP errors; timeouts handled. | **Planned:** MCP conformance harness + fixtures (TBD). |
| **Skill Pack** | `SKILL.md` + assets | File load only (no hostcalls) | **PASS** if: frontmatter valid; name/description parsed; injected into system prompt; skill resolution precedence correct. | **Current:** `tests/resource_loader.rs`, `tests/e2e_cli.rs` (skill discovery paths). |
| **Prompt Template** | `.md` prompt file (optional frontmatter) | File load only | **PASS** if: template parse succeeds; parameters substitute deterministically; `/template` invocation expands correctly. | **Current:** `tests/resource_loader.rs`, `tests/e2e_cli.rs` (template paths). |
| **Theme** | `.json` theme file | File load only | **PASS** if: JSON schema valid; theme resolves/loads; TUI applies without panics. | **Current:** `tests/tui_snapshot.rs` + theme loader coverage. |
| **Package Source** | Package manifest listing resources | Depends on contained resources | **PASS** if: resource discovery resolves correctly; collisions resolved deterministically; package precedence honored. | **Current:** `tests/package_manager.rs`, `tests/resource_loader.rs`, `tests/e2e_cli.rs` (package flows). |

#### Test Plan (fixtures → harness → assertions)

1. **Fixture schemas**  
   - Validate event payload fixtures: `tests/ext_conformance/event_payloads/event_payloads.json`  
   - Validate mock specs: `tests/mock_spec_schema.rs` + `tests/mock_spec_validation.rs`

2. **Registration parity**  
   - Rust runtime: `tests/extensions_registration.rs` + `tests/e2e_extension_registration.rs`  
   - Output: tools/commands/flags/shortcuts/providers must match expected snapshots

3. **Event conformance**  
   - Use `tests/ext_conformance/event_payloads/event_payloads.json` to drive event hooks  
   - Validate scheduling/determinism: `tests/event_loop_conformance.rs`

4. **Hostcall + capability mapping**  
   - Exercise `tool_call` / `tool_result` / `pi.http` / `pi.exec` with mock specs  
   - Assert derived capabilities match taxonomy (see `EXTENSIONS.md` §3.2A)

5. **Differential TS ↔ Rust (oracle mode)**  
   - TS harness: `tests/ext_conformance/ts_harness/run_extension.ts`  
   - Rust harness: `tests/ext_conformance.rs` + conformance comparators  
   - Planned runner: `bd-21dv` (per-extension comparisons + report)

6. **Resource packs**  
   - Skills/prompts/themes/packages: `tests/resource_loader.rs` + `tests/e2e_cli.rs`

7. **Pass/Fail Criteria Summary**  
   - **PASS** = registration parity + deterministic outputs + error taxonomy compliance  
   - **FAIL** = any mismatch in registration, capability derivation, or normalized output diff  
   - **SKIP** = unsupported capability/shape (must include rationale + tracking bead)

### Extension Logs (JSONL)

All extension-related logs must conform to the **ext.log.v1** schema
(see `EXTENSIONS.md`). The conformance harness records JSONL logs per scenario:

- **Harness output:** `target/ext_conformance/logs/<scenario_id>.jsonl`
- **Capture output:** `tests/ext_conformance/capture/<ext>/<scenario>/extension.log.jsonl`

**Normalization for deterministic diffs:**
- Replace `ts`, `pid`, `host`, `run_id`, `session_id`, `artifact_id`,
  `trace_id`, `span_id` with placeholders.
- Normalize absolute paths to `<cwd>/...`.

**Deterministic runtime controls (TS oracle + Rust PiJS):**
- Patched globals: `Date`/`Date.now`, `Math.random`, `process.cwd`, `process.env.HOME`, `pi.time.nowMs`.
- Env vars: `PI_DETERMINISTIC_TIME_MS`, `PI_DETERMINISTIC_TIME_STEP_MS`, `PI_DETERMINISTIC_RANDOM`,
  `PI_DETERMINISTIC_RANDOM_SEED`, `PI_DETERMINISTIC_CWD`, `PI_DETERMINISTIC_HOME`.

**CI consumption:**
- Archive `target/ext_conformance/logs/**` as CI artifacts.
- Diffs should be grouped by `event` and `correlation` IDs to speed triage.

### NPM Registry Conformance (bd-3dd7)

Most npm extensions are **tier 3+** and therefore `#[ignore]` by default. To attempt all npm-registry
extensions, include ignored tests:

```bash
CARGO_TARGET_DIR=/tmp/pi_target cargo test --test ext_conformance_generated ext_npm_ -- --include-ignored
```

**Snapshot (2026-02-05):**
- npm extensions attempted: 63
- passed: 28
- failed: 35
- self-contained subset (`conformance_tier <= 2` and `has_npm_deps = false`): 14/17 passed (82.4%)

**Failure summary (one row per failing extension):**

| Extension | Category | Detail |
|---|---|---|
| `npm/aliou-pi-guardrails` | `missing_npm_dependency` | @aliou/pi-utils-settings |
| `npm/aliou-pi-linkup` | `missing_global_console` | console is not defined |
| `npm/aliou-pi-processes` | `relative_import_resolution` | ../components/processes-component |
| `npm/aliou-pi-synthetic` | `manifest_mismatch` | expected command 'synthetic:quotas' not found in actual commands: [] |
| `npm/aliou-pi-toolchain` | `missing_npm_dependency` | @aliou/sh |
| `npm/benvargas-pi-ancestor-discovery` | `missing_node_shim_export` | Could not find export 'isAbsolute' in module 'node:path' |
| `npm/imsus-pi-extension-minimax-coding-plan-mcp` | `missing_node_shim_export` | Could not find export 'readFile' in module 'node:fs' |
| `npm/juanibiapina-pi-files` | `missing_npm_dependency` | @juanibiapina/pi-extension-settings |
| `npm/lsp-pi` | `missing_npm_dependency` | vscode-languageserver-protocol/node.js |
| `npm/marckrenn-pi-sub-bar` | `missing_npm_dependency` | @marckrenn/pi-sub-shared |
| `npm/marckrenn-pi-sub-core` | `missing_npm_dependency` | @marckrenn/pi-sub-shared |
| `npm/permission-pi` | `missing_npm_dependency` | shell-quote |
| `npm/pi-agentic-compaction` | `missing_npm_dependency` | just-bash |
| `npm/pi-amplike` | `manifest_mismatch` | manifest says it registers tools, but no tool defs were captured |
| `npm/pi-bash-confirm` | `manifest_mismatch` | expected command 'demo-bash-confirm' not found in actual commands: ["bash-confirm"] |
| `npm/pi-brave-search` | `missing_npm_dependency` | @mozilla/readability |
| `npm/pi-ghostty-theme-sync` | `missing_node_shim_export` | Could not find export 'createHash' in module 'node:crypto' |
| `npm/pi-mermaid` | `missing_npm_dependency` | beautiful-mermaid |
| `npm/pi-messenger` | `missing_node_shim_export` | Could not find export 'isAbsolute' in module 'node:path' |
| `npm/pi-multicodex` | `missing_virtual_module_export` | Could not find export 'getApiProvider' in module '@mariozechner/pi-ai' |
| `npm/pi-repoprompt-mcp` | `missing_npm_dependency` | @modelcontextprotocol/sdk |
| `npm/pi-screenshots-picker` | `missing_npm_dependency` | glob |
| `npm/pi-search-agent` | `missing_npm_dependency` | dotenv |
| `npm/pi-session-ask` | `runtime_error` | not a function |
| `npm/pi-shadow-git` | `missing_node_shim_export` | Could not find export 'isAbsolute' in module 'node:path' |
| `npm/pi-super-curl` | `missing_npm_dependency` | uuid |
| `npm/pi-telemetry-otel` | `missing_npm_dependency` | @opentelemetry/api |
| `npm/pi-wakatime` | `missing_node_builtin` | node:stream |
| `npm/pi-watch` | `missing_npm_dependency` | chokidar |
| `npm/pi-web-access` | `missing_npm_dependency` | @mozilla/readability |
| `npm/ralph-loop-pi` | `missing_virtual_module_export` | Could not find export 'AssistantMessageComponent' in module '@mariozechner/pi-coding-agent' |
| `npm/vaayne-agent-kit` | `missing_npm_dependency` | @modelcontextprotocol/sdk/client/index.js |
| `npm/vaayne-pi-mcp` | `missing_npm_dependency` | @modelcontextprotocol/sdk/client/index.js |
| `npm/vaayne-pi-web-tools` | `missing_npm_dependency` | jsdom |
| `npm/zenobius-pi-dcp` | `missing_npm_dependency` | bunfig |

---

## Test Logging (JSONL + Artifact Index)

To make E2E and integration tests auditable and diffable, tests emit **structured JSONL logs**
and a **JSONL artifact index**. These are intended for CI artifact capture and deterministic
diffing alongside normalized fixtures.

### Log Schema: `pi.test.log.v1`

Each log entry is one JSON object per line:

```json
{
  "schema": "pi.test.log.v1",
  "type": "log",
  "test": "e2e_cli_help_flag",
  "seq": 1,
  "ts": "2026-02-03T03:01:02.123Z",
  "t_ms": 123,
  "level": "info",
  "category": "setup",
  "message": "Created test directory",
  "context": {
    "path": "/tmp/pi-test-123/workspace",
    "size": "42 bytes"
  }
}
```

**Field notes:**
- `ts` is ISO-8601 UTC; `t_ms` is relative to harness start.
- `test` is optional; when present it is a single string.
- `context` is a flat string map (redacted for sensitive keys).

### Artifact Index Schema: `pi.test.artifact.v1`

Each artifact entry is one JSON object per line:

```json
{
  "schema": "pi.test.artifact.v1",
  "type": "artifact",
  "test": "e2e_cli_help_flag",
  "seq": 1,
  "ts": "2026-02-03T03:01:05.000Z",
  "t_ms": 3000,
  "name": "stdout.txt",
  "path": "/tmp/pi-test-123/stdout.txt",
  "size_bytes": 2048,
  "sha256": "sha256:deadbeef..."
}
```

### Normalization (Deterministic Diffs)

Normalized JSONL replaces non-deterministic values so diffs are stable:
- `ts` → `<TIMESTAMP>`
- `t_ms` → `0`
- absolute project paths → `<PROJECT_ROOT>/...`
- temp/test paths → `<TEST_ROOT>/...`
- UUIDs/run IDs → `<UUID>` / `<RUN_ID>` when present in strings
- local ports in URLs → `<PORT>`

Normalized outputs are written alongside raw logs with a `.normalized.jsonl` suffix.


## Fixture Schema

### Test Case Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `name` | string | yes | Unique test identifier |
| `description` | string | no | Human-readable description |
| `setup` | array | no | Steps to initialize test environment |
| `input` | object | yes | Tool input parameters |
| `expected` | object | yes | Expected results |
| `expect_error` | bool | no | Whether test should fail |
| `error_contains` | string | no | Expected error substring |
| `tags` | array | no | Categories for filtering |

### Setup Steps

| Type | Fields | Description |
|------|--------|-------------|
| `create_file` | `path`, `content` | Create file with content |
| `create_dir` | `path` | Create directory |
| `run_command` | `command` | Execute shell command |

### Expected Results

| Field | Type | Description |
|-------|------|-------------|
| `content_exact` | string | Content must match exactly |
| `content_contains` | array | Content must include all substrings |
| `content_not_contains` | array | Content must NOT include any substring |
| `content_regex` | string | Content must match regex |
| `details` | object | Details must contain keys (value check optional) |
| `details_exact` | object | Details must match exactly |
| `details_none` | bool | Details must be None |

---

## Reference Capture Process

### Phase 1: Manual Fixture Creation (Current)

Fixtures were created by:
1. Running TypeScript tools with specific inputs
2. Capturing outputs and metadata
3. Encoding expected behavior in JSON

### Phase 2: Automated Capture (Planned)

Future automation with TypeScript capture harness:

```bash
# Run TypeScript reference and capture output
cd pi-mono
node capture-fixtures.js --tool read --output fixtures/read_tool.json

# Run Rust implementation against same fixtures
cd ../pi_agent_rust
cargo test --test conformance_fixtures
```

---

## Running Conformance Tests

### All Tests
```bash
cargo test
```

### Library Tests Only
```bash
cargo test --lib
```

### Conformance Tests Only
```bash
cargo test --test tools_conformance
cargo test --test conformance_fixtures
```

### With Output
```bash
cargo test -- --nocapture
```

### Specific Tool
```bash
cargo test read_tool
cargo test bash_tool
```

---

## Adding New Conformance Tests

### 1. For Existing Tools

Add cases to the appropriate `tests/conformance/fixtures/<tool>_tool.json`:

```json
{
  "name": "new_test_case",
  "description": "Test some edge case",
  "setup": [...],
  "input": {...},
  "expected": {...}
}
```

### 2. For New Tools

1. Create fixture file: `tests/conformance/fixtures/<tool>_tool.json`
2. Add test module to `tests/tools_conformance.rs`
3. Implement fixture runner for the tool

### 3. Verify Against TypeScript

Before adding a fixture, verify the expected behavior:

```bash
# In pi-mono
echo '{"path": "test.txt"}' | node -e "
  const tool = require('./tools/read');
  process.stdin.on('data', async (d) => {
    const result = await tool.execute(JSON.parse(d));
    console.log(JSON.stringify(result, null, 2));
  });
"
```

---

## Behavioral Contract

### Tool Output Structure

All tools return:
```rust
struct ToolResult {
    content: Vec<ContentBlock>,  // Primary output
    details: Option<Value>,      // Metadata (truncation info, etc.)
    is_error: bool,              // Error flag
    error_type: Option<String>,  // Error classification
}
```

### Truncation Behavior

| Constant | Value | Used By |
|----------|-------|---------|
| `DEFAULT_MAX_LINES` | 2000 | read, bash, grep |
| `DEFAULT_MAX_BYTES` | 50KB | read, bash, grep, find, ls |
| `GREP_MAX_LINE_LENGTH` | 500 | grep |

Truncation message format:
```
[N more lines in file. Use offset=M to continue.]
```

### Path Resolution

1. Absolute paths used as-is
2. `~` expanded to home directory
3. Relative paths resolved from working directory
4. Symlinks followed for reads, not for writes

### Error Handling

Tools should return errors (not panic) for:
- File not found
- Permission denied
- Invalid path
- Timeout exceeded
- Invalid input parameters

---

## Test Failure Triage

### Common Causes

| Symptom | Likely Cause | Fix |
|---------|--------------|-----|
| Content mismatch | Different newline handling | Check `\n` vs `\r\n` |
| Details mismatch | Extra/missing metadata | Update fixture or code |
| Timeout | Async handling difference | Check spawn/wait logic |
| Order mismatch | Non-deterministic output | Sort before compare |

### Debugging

```bash
# Run specific test with debug output
RUST_LOG=debug cargo test test_name -- --nocapture

# Compare outputs manually
cargo run -- -p 'read test.txt' > rust_output.txt
node pi-mono/cli.js -p 'read test.txt' > ts_output.txt
diff rust_output.txt ts_output.txt
```

---

## Coverage Goals

| Category | Target | Current |
|----------|--------|---------|
| Core types | 100% | ~95% |
| Tools | 100% | ~80% |
| Providers | Streaming paths | ~70% |
| Session | JSONL format | ~60% |
| CLI | Argument parsing | ~40% |

---

## Future Work

1. **TypeScript Reference Harness**: Automated fixture generation from pi-mono
2. **Session Format Tests**: JSONL compatibility verification
3. **CLI Argument Tests**: Flag parsing conformance
4. **Streaming Tests**: SSE event sequence validation
5. **Performance Benchmarks**: Latency and throughput comparison

---

## Related Documentation

- [FEATURE_PARITY.md](FEATURE_PARITY.md): Implementation status tracker
- [README.md](README.md): Project overview
- [AGENTS.md](AGENTS.md): AI agent instructions
