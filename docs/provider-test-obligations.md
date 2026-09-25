# Provider Test Obligations

> Mandatory test categories, coverage floors, and artifact requirements for every provider in pi_agent_rust.

---

## Overview

Every provider in the codebase carries a `ProviderTestObligations` struct (defined in `src/provider_metadata.rs`) that declares which test tiers are required:

```rust
pub struct ProviderTestObligations {
    pub unit: bool,      // Unit-level checks (identity, request mapping, auth, tools)
    pub contract: bool,  // Wire-format contract tests with mock HTTP
    pub conformance: bool, // VCR cassette-based streaming conformance
    pub e2e: bool,       // End-to-end agent loop tests
}
```

The constant `TEST_REQUIRED` sets all four to `true` and is the default for every provider in `PROVIDER_METADATA`. CI enforces these obligations via `tests/provider_unit_checklist.rs`.

---

## Tier 1: Unit Tests (Checklist)

**Enforced by:** `tests/provider_unit_checklist.rs`
**Scope:** Pure-logic validation with no HTTP, no VCR, no async runtime.

### Required Test Classes

Every native provider must pass all six checklist classes. If you add a new native provider and skip any class, the meta-test `checklist_all_native_providers_enumerated` will fail CI.

| # | Class | What It Validates | Macro/Pattern |
|---|-------|-------------------|---------------|
| 1 | **Identity** | `name()`, `api()`, `model_id()` return non-empty, correct values | `checklist_all_native_providers_have_identity` (single test enumerates all providers) |
| 2 | **Request mapping** | `build_request()` produces valid serializable JSON with at least one field | `checklist_request_mapping!` macro per provider |
| 3 | **Auth/header composition** | API key from `StreamOptions.api_key` flows into request (not silently dropped) | `checklist_*_auth_key_flows_through` tests |
| 4 | **URL/endpoint resolution** | `api()` returns the expected API type string for the provider | `checklist_providers_have_default_endpoint` (single test enumerates all) |
| 5 | **Tool-call serialization** | `build_request()` with `ToolDef` entries produces provider-specific tool wire format | `checklist_tool_serialization!` macro per provider |
| 6 | **VCR fixture presence** | At least N VCR cassettes exist matching `verify_{provider}_*.json` | `checklist_vcr_fixture_coverage_floor` |

### VCR Coverage Floor (per provider)

| Provider | Minimum Cassettes | Rationale |
|----------|-------------------|-----------|
| anthropic | 3 | simple_text + tool_call + error_auth |
| openai | 3 | simple_text + tool_call + error_auth |
| gemini | 3 | simple_text + tool_call + error_auth |
| cohere | 3 | simple_text + tool_call + error_auth |
| azure | 1 | at least simple_text |
| bedrock | 1 | at least simple_text |
| vertex | 1 | at least simple_text |
| copilot | 1 | at least simple_text |
| gitlab | 1 | at least simple_text |

### Adding a New Provider to the Checklist

1. Add the provider to the `providers` vec in `checklist_all_native_providers_have_identity`
2. Add a `checklist_request_mapping!` invocation
3. Add a `checklist_tool_serialization!` invocation with the correct JSON path for tool definitions
4. Add an auth flow-through test
5. Add the provider to `known_providers` in `checklist_providers_have_default_endpoint`
6. Add the provider to `required_providers` in `checklist_vcr_fixture_coverage_floor`
7. Update the count in `checklist_all_native_providers_enumerated`

### Provider-Specific Tool JSON Paths

Different providers serialize tool definitions at different JSON paths:

| Provider | Tool JSON Path |
|----------|---------------|
| Anthropic | `tools[]` (with `input_schema`) |
| OpenAI | `tools[]` (with `function.parameters`) |
| Gemini | `tools[0].functionDeclarations[]` |
| Cohere | `tools[]` (with `function`) |
| Bedrock | `toolConfig.tools[]` |

---

## Tier 2: Contract Tests (Mock HTTP)

**Enforced by:** `tests/provider_native_contract.rs`
**Scope:** Full request-response cycle using `MockHttpRequest`/`MockHttpResponse` from the test harness. Tests run async but use canned HTTP responses, not real networks.

### Required Scenarios

Each native provider must have contract tests covering:

| # | Scenario | What It Validates |
|---|----------|-------------------|
| 1 | **Simple text response** | Provider correctly decodes a streaming text response into `TextDelta` + `Done` events |
| 2 | **Tool call response** | Provider decodes a tool-call response into `ToolCallStart`/`ToolCallDelta`/`ToolCallEnd` events |
| 3 | **Auth header construction** | The correct auth header (`Authorization: Bearer`, `X-API-Key`, `api-key`, etc.) is sent |
| 4 | **Request payload shape** | URL path, Content-Type, and body JSON structure match provider spec |

### Test Infrastructure

```rust
// Helper to create a ModelEntry for contract tests
fn make_model_entry(provider: &str, model_id: &str, base_url: &str) -> ModelEntry;

// Helper to create SSE-formatted mock responses
fn text_event_stream_response(body: String) -> MockHttpResponse;

// Helper to inspect captured request
fn request_header(headers: &[(String, String)], key: &str) -> Option<String>;
fn request_body_json(request: &MockHttpRequest) -> serde_json::Value;

// Drive a provider stream to completion and collect all events
fn collect_stream_events(provider, context, options) -> Vec<StreamEvent>;
```

### SSE Body Generators

Each provider has a dedicated SSE body generator function that produces valid SSE data matching the provider's wire format:

- `anthropic_simple_sse()` / `anthropic_tool_call_sse()`
- `openai_simple_sse()` / `openai_tool_call_sse()`
- `gemini_simple_sse()` / `gemini_tool_call_sse()`
- `cohere_simple_sse()` / `cohere_tool_call_sse()`
- `azure_simple_sse()` / `azure_tool_call_sse()`
- etc.

These generators produce the minimum valid SSE stream for the scenario. They are the canonical reference for each provider's wire format.

---

## Tier 3: Error Path Tests (VCR)

**Enforced by:** `tests/provider_error_paths.rs`
**Scope:** Deterministic offline testing of HTTP error handling and malformed SSE using VCR cassette playback.

### Required Error Scenarios

| # | Scenario | Status | Expected Behavior |
|---|----------|--------|-------------------|
| 1 | **HTTP 500** | 500 | `stream()` returns `Err` containing "HTTP 500" and response body text |
| 2 | **Wrong Content-Type** | 200 | `stream()` returns `Err` with "protocol error" and "content-type" |
| 3 | **Missing Content-Type** | 200 | `stream()` returns `Err` with "missing content-type" |
| 4 | **Invalid JSON in SSE** | 200 | Stream yields `Err` with "JSON" or "parse" |
| 5 | **Invalid UTF-8** | 200 | Stream yields `Err` with "SSE error" (uses base64 VCR body chunks) |

### VCR Cassette Helper

```rust
fn vcr_client(
    test_name: &str,           // Cassette file name
    url: &str,                 // Expected request URL
    request_body: Value,       // Expected request body (must match exactly)
    status: u16,               // HTTP response status
    response_headers: Vec<(String, String)>,
    response_chunks: Vec<String>,
) -> (Client, TempDir);       // Client with VCR + temp dir (keep alive!)
```

For invalid UTF-8 tests, use `vcr_client_bytes()` which accepts `Vec<Vec<u8>>` and encodes as base64 in the cassette.

### Provider-Specific Request Body Builders

Each provider has a body builder that produces the exact JSON the provider serializes:

```rust
fn anthropic_body(model: &str, prompt: &str) -> Value;  // messages + model + stream + max_tokens
fn openai_body(model: &str, prompt: &str) -> Value;     // messages + model + stream + stream_options
fn gemini_body(prompt: &str) -> Value;                   // contents + generationConfig
fn azure_body(prompt: &str) -> Value;                    // messages + stream + stream_options (no model)
```

These must exactly match what the provider's `stream()` method serializes, because VCR matching compares request bodies field-by-field.

---

## Tier 4: Streaming Conformance Tests (VCR Cassettes)

**Enforced by:** `tests/provider_streaming.rs` + per-provider sub-modules in `tests/provider_streaming/`
**Scope:** Full streaming round-trips with VCR cassette record/playback against real API formats.

### Sub-Module Structure

```
tests/provider_streaming.rs        # Root: shared helpers, VCR config, StreamOutcome/StreamSummary
tests/provider_streaming/
    anthropic.rs                   # Anthropic-specific streaming tests
    openai.rs                      # OpenAI Chat Completions tests
    openai_responses.rs            # OpenAI Responses API tests
    gemini.rs                      # Gemini streaming tests
    azure.rs                       # Azure OpenAI streaming tests
    cohere.rs                      # Cohere streaming tests
```

### Required Scenarios Per Provider

| # | Scenario | VCR Cassette Pattern | Validates |
|---|----------|---------------------|-----------|
| 1 | **Simple text** | `verify_{provider}_simple_text.json` | Basic text streaming: Start → TextDelta(s) → Done |
| 2 | **Tool call** | `verify_{provider}_tool_call_single.json` | Tool use: ToolCallStart → ToolCallDelta(s) → ToolCallEnd → Done |
| 3 | **Unicode text** | `verify_{provider}_unicode_text.json` | Non-ASCII content preservation through streaming |
| 4 | **Auth error (401)** | `verify_{provider}_error_auth_401.json` | Auth failure produces Error event or stream error |
| 5 | **Bad request (400)** | `verify_{provider}_error_bad_request_400.json` | Malformed request produces Error event |
| 6 | **Rate limit (429)** | `verify_{provider}_error_rate_limit_429.json` | Rate limit produces Error event |

### StreamEvent Sequence Validation

The `StreamSummary` struct tracks the complete event timeline:

```rust
pub struct StreamSummary {
    pub timeline: Vec<String>,    // Ordered event type names
    pub event_count: usize,
    pub has_start: bool,          // Must be true for successful streams
    pub has_done: bool,           // Must be true for completed streams
    pub has_error_event: bool,    // True for error scenarios
    pub text: String,             // Accumulated text content
    pub thinking: String,         // Accumulated thinking content
    pub tool_calls: Vec<ToolCall>,
    pub text_deltas: usize,       // Count of TextDelta events
    pub stop_reason: Option<StopReason>,
    pub stream_error: Option<String>,
}
```

### VCR Modes

| Mode | Env Var | Behavior |
|------|---------|----------|
| **Playback** (default) | `VCR_MODE=playback` or unset | Replay from cassette files |
| **Record** | `VCR_MODE=record` | Record real API interactions (requires API keys) |
| **Auto** | `VCR_MODE=auto` | Use cassette if available, record otherwise |
| **Strict** | `VCR_STRICT=1` | Fail on request mismatch instead of falling through |

### VCR Cassette Location

All VCR cassettes live in `tests/fixtures/vcr/` and follow the naming convention:
```
verify_{provider}_{scenario}.json
```

Current cassette inventory includes 90+ files covering the 10 native provider implementation modules plus OpenAI-compatible presets (alibaba-cn, kimi-for-coding, minimax, modelscope, moonshotai-cn, nebius, ovhcloud, scaleway, sap_ai_core, siliconflow, upstage, venice, zai, zhipuai-coding-plan, etc.).

---

## Tier 5: E2E Tests

**Enforced by:** `tests/e2e_*.rs`
**Scope:** Full agent loop with provider (VCR playback), verifying multi-turn conversations and tool use scenarios end-to-end.

### E2E Test Approach

- Use VCR playback (`VCR_MODE=playback`) to avoid real API calls
- Set `PI_TEST_MODE=1` for deterministic system prompts
- Use `--thinking off` for deterministic test behavior
- Use isolation flags: `--no-tools --no-extensions --no-skills --no-prompt-templates --no-themes`

### E2E Requirements for Native Providers

Each native provider should have at least one E2E test demonstrating:
1. A complete agent turn (user message → provider response → rendered output)
2. Correct `StreamEvent` translation through the agent loop
3. Session persistence of the interaction

---

## OpenAI-Compatible Preset Obligations

Providers with `onboarding: OpenAICompatiblePreset` route through the shared `OpenAIProvider` or `OpenAIResponsesProvider`. Their test obligations are lighter:

| Obligation | Required? | What to Provide |
|------------|-----------|-----------------|
| Unit (identity) | No (covered by shared OpenAI tests) | N/A |
| Unit (request mapping) | No (covered by shared OpenAI tests) | N/A |
| VCR cassettes | **Yes** | At minimum: `verify_{provider}_simple_text.json`, `verify_{provider}_error_auth_401.json`, `verify_{provider}_tool_call_single.json` |
| Error paths | No (covered by shared OpenAI error tests) | N/A |
| E2E | No (covered by shared OpenAI E2E tests) | N/A |

The VCR cassettes validate that the provider's actual API response format is compatible with the OpenAI parser. This catches providers that claim OpenAI compatibility but have subtle wire format differences.

---

## Test Helper Reference

### `tests/common/` Shared Infrastructure

| Module | Purpose |
|--------|---------|
| `common/harness.rs` | `TestHarness` with JSONL logging, artifact tracking, `MockHttpRequest`/`MockHttpResponse` |
| `common/mod.rs` | `run_async()` helper for blocking on async code in tests |

### Key Helper Functions

```rust
// Minimal context with one user message
fn minimal_context() -> Context;
fn simple_context() -> Context;

// Context with one or two ToolDef entries
fn context_with_tools() -> Context;

// StreamOptions with test API key
fn default_options() -> StreamOptions;
fn options_with_key(key: &str) -> StreamOptions;

// Count VCR cassettes matching a provider prefix
fn count_cassettes(provider_prefix: &str) -> usize;

// Collect all stream events until Done
fn collect_stream_events(provider, context, options) -> Vec<StreamEvent>;
async fn collect_events<S: Stream>(stream: S) -> StreamOutcome;

// Summarize event timeline for assertions
fn summarize_events(outcome: &StreamOutcome) -> StreamSummary;
```

---

## Running Provider Tests

```bash
# All provider tests (unit + contract + conformance + error paths)
cargo test provider

# Specific provider
cargo test anthropic
cargo test openai
cargo test gemini

# Unit checklist only
cargo test provider_unit_checklist

# Contract tests only
cargo test provider_native_contract

# Error path tests only
cargo test provider_error_paths

# Streaming conformance (VCR playback)
cargo test --test provider_streaming

# Streaming conformance for one provider (one module per provider)
cargo test --test provider_streaming anthropic::

# Record new VCR cassettes (requires API key)
ANTHROPIC_API_KEY=sk-ant-... VCR_MODE=record cargo test --test provider_streaming anthropic::
```

---

## Adding Tests for a New Provider: Step-by-Step

### For a Native Provider

1. **Unit checklist** (`tests/provider_unit_checklist.rs`):
   - Add to `checklist_all_native_providers_have_identity` providers vec
   - Add `checklist_request_mapping!` invocation
   - Add `checklist_tool_serialization!` invocation
   - Add auth flow-through test
   - Add to `checklist_providers_have_default_endpoint` known_providers vec
   - Add to `checklist_vcr_fixture_coverage_floor` required_providers vec
   - Increment count in `checklist_all_native_providers_enumerated`

2. **Contract tests** (`tests/provider_native_contract.rs`):
   - Add SSE body generator function (e.g., `your_provider_simple_sse()`)
   - Add simple text streaming test
   - Add tool call streaming test
   - Add auth header verification test

3. **Error path tests** (`tests/provider_error_paths.rs`):
   - Add request body builder (e.g., `your_provider_body()`)
   - Add HTTP 500 test
   - Add malformed SSE test

4. **Streaming conformance** (`tests/provider_streaming/`):
   - Create `tests/provider_streaming/your_provider.rs`
   - Add `#[path = "provider_streaming/your_provider.rs"] mod your_provider;` to `tests/provider_streaming.rs`
   - Record VCR cassettes for all 6 scenarios
   - Write tests that assert on `StreamSummary` fields

5. **VCR cassettes** (`tests/fixtures/vcr/`):
   - `verify_your_provider_simple_text.json`
   - `verify_your_provider_tool_call_single.json`
   - `verify_your_provider_unicode_text.json`
   - `verify_your_provider_error_auth_401.json`
   - `verify_your_provider_error_bad_request_400.json`
   - `verify_your_provider_error_rate_limit_429.json`

### For an OpenAI-Compatible Preset

1. **VCR cassettes** (`tests/fixtures/vcr/`):
   - `verify_your_provider_simple_text.json`
   - `verify_your_provider_tool_call_single.json`
   - `verify_your_provider_error_auth_401.json`

2. **Add to VCR coverage floor** in `checklist_vcr_fixture_coverage_floor` (if the provider is prominent enough to warrant CI enforcement)

---

## Common Pitfalls

1. **VCR body mismatch**: VCR matching compares request bodies exactly (after JSON normalization). If your provider adds extra fields (e.g., `stream_options`), your test body builder must include them too.

2. **Forgetting `_dir` in VCR tests**: The `TempDir` returned by `vcr_client()` must be kept alive for the test duration. If dropped early, the cassette file is deleted and playback fails.

3. **Missing `oauth_config: None`**: Every `ModelEntry` construction in tests must include `oauth_config: None`.

4. **Provider-specific URL patterns**: Some providers append paths differently. Gemini appends `?alt=sse&key=...` as query params. Azure uses a completely different URL structure. Ensure your test URLs match what the provider actually sends.

5. **Base64 body chunks for invalid UTF-8**: Standard VCR cassettes store response body as UTF-8 strings. For tests that need raw bytes (invalid UTF-8), use `vcr_client_bytes()` with `body_chunks_base64`.

6. **`common::run_async` vs `asupersync::test_utils::run_test`**: Contract and error path tests use `common::run_async()` for simplicity. Streaming conformance tests may use the full runtime. Both patterns are acceptable.
