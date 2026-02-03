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
- `tests/rpc_mode.rs`: RPC protocol sanity (get_state, prompt streaming events, get_session_stats counting tool calls/results)

**Planned:**
- Fixture-based RPC conformance harness comparing Rust RPC responses/events against the TypeScript reference (`legacy_pi_mono_code/pi-mono/packages/coding-agent/docs/rpc.md`).

### 4. Extension Conformance (Planned)

Location (planned): `tests/ext_conformance/`

**Purpose:** Validate extension runtime behavior (registration/events/hostcalls) against the TypeScript reference.

Initial fixture sources:
- `legacy_pi_mono_code/pi-mono/.pi/extensions/*` (built-in reference extensions)
- Minimal synthetic fixtures for connector calls (`exec`, `tool`, `http`, `session`)

### Extension Logs (JSONL)

All extension-related logs must conform to the **ext.log.v1** schema
(see `EXTENSIONS.md`). The conformance harness records JSONL logs per scenario:

- **Harness output:** `target/ext_conformance/logs/<scenario_id>.jsonl`
- **Capture output:** `tests/ext_conformance/capture/<ext>/<scenario>/extension.log.jsonl`

**Normalization for deterministic diffs:**
- Replace `ts`, `pid`, `host`, `run_id`, `session_id`, `artifact_id`,
  `trace_id`, `span_id` with placeholders.
- Normalize absolute paths to `<cwd>/...`.

**CI consumption:**
- Archive `target/ext_conformance/logs/**` as CI artifacts.
- Diffs should be grouped by `event` and `correlation` IDs to speed triage.

---

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
