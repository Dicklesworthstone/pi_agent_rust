# Provider Onboarding Checklist

> Canonical step-by-step guide for adding or maintaining a provider in pi_agent_rust.

---

## Prerequisites

Before starting, determine your provider's **onboarding mode** (defined in `src/provider_metadata.rs`):

| Mode | When to Use | Examples |
|------|-------------|---------|
| `OpenAICompatiblePreset` | Provider exposes an OpenAI-compatible `/v1/chat/completions` endpoint | Groq, Cerebras, OpenRouter, Mistral, DeepSeek, Together, Fireworks |
| `BuiltInNative` | Provider has a non-OpenAI wire format requiring a dedicated implementation | Anthropic, Google Gemini, Cohere |
| `NativeAdapterRequired` | Provider needs custom auth flows or non-standard request/response handling | Azure OpenAI, Amazon Bedrock, GitHub Copilot, GitLab Duo |

Most new providers are **OpenAI-compatible presets** and require no Rust code changes beyond metadata registration.

---

## Path A: OpenAI-Compatible Preset (No New Rust Code)

### Step 1: Add Provider Metadata

**File:** `src/provider_metadata.rs` (in the `PROVIDER_METADATA` array)

Add a new `ProviderMetadata` entry:

```rust
ProviderMetadata {
    canonical_id: "your-provider",           // Lowercase, hyphen-separated
    aliases: &["alias1", "alias2"],          // Alternative names users might type
    auth_env_keys: &["YOUR_PROVIDER_API_KEY"], // Env var(s) for API key lookup
    onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
    routing_defaults: Some(ProviderRoutingDefaults {
        api: "openai-completions",           // Or "openai-responses" if supported
        base_url: "https://api.your-provider.com/v1",
        auth_header: true,                   // true = Authorization: Bearer <key>
        reasoning: true,                     // Does the provider support reasoning models?
        input: &INPUT_TEXT,                  // Or &INPUT_TEXT_IMAGE if multimodal
        context_window: 128_000,             // Default context window
        max_tokens: 16_384,                  // Default max output tokens
    }),
    test_obligations: TEST_REQUIRED,
}
```

**Placement:** Add alphabetically within the appropriate batch section (Batch A1, A2, A3, etc.).

**Key decisions:**
- `api`: Use `"openai-completions"` for standard Chat Completions API, `"openai-responses"` for OpenAI Responses API
- `auth_header`: `true` means the key is sent as `Authorization: Bearer <key>`. `false` means provider-specific auth (e.g., query param)
- `input`: `&INPUT_TEXT` for text-only, `&INPUT_TEXT_IMAGE` for multimodal

### Step 2: Add Provider Enum Variant (if needed)

**File:** `src/provider.rs`

If you want the provider to appear in the `KnownProvider` enum for type-safe matching:

```rust
// In KnownProvider enum
YourProvider,

// In Display impl
Self::YourProvider => write!(f, "your-provider"),

// In FromStr impl
"your-provider" => Ok(Self::YourProvider),
```

> **Note:** This step is optional for OpenAI-compatible presets. The `Custom(String)` fallback handles unknown providers automatically.

### Step 3: Add Environment Variable to README

**File:** `README.md` (Environment Variables table)

```markdown
| `YOUR_PROVIDER_API_KEY` | Your Provider API key |
```

### Step 4: Add Model Entries (Optional)

**File:** User's `~/.pi/agent/models.json` or built-in registry

If the provider has well-known models, add entries in `models.json`:

```json
{
  "providers": {
    "your-provider": {
      "models": [
        {
          "id": "your-model-v1",
          "name": "Your Model v1",
          "reasoning": true,
          "contextWindow": 128000,
          "maxTokens": 16384,
          "cost": {
            "input": 1.0,
            "output": 3.0,
            "cacheRead": 0.1,
            "cacheWrite": 1.5
          }
        }
      ]
    }
  }
}
```

The model registry (`src/models.rs`) merges user `models.json` with built-in defaults. Provider-level fields (`baseUrl`, `api`, `apiKey`, `headers`, `authHeader`, `compat`) cascade to all models under that provider.

### Step 5: Verify Routing

Run a quick smoke test to confirm the routing resolves correctly:

```bash
cargo test provider_metadata::tests -- --nocapture
```

Check that `canonical_provider_id("your-provider")` returns `Some("your-provider")` and `provider_routing_defaults("your-provider")` returns the expected defaults.

### Step 6: Run Quality Gates

```bash
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

---

## Path B: Built-In Native Provider (New Rust Implementation)

Complete **all steps from Path A**, then:

### Step 7: Create Provider Module

**File:** `src/providers/<name>.rs`

Implement the `Provider` trait:

```rust
use crate::error::{Error, Result};
use crate::http::client::Client;
use crate::model::{AssistantMessage, ContentBlock, StopReason, StreamEvent, TextContent, Usage};
use crate::models::CompatConfig;
use crate::provider::{Context, Provider, StreamOptions};
use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;

pub struct YourProvider {
    model_id: String,
    base_url: String,
    provider_name: String,
    client: Client,
    compat: Option<CompatConfig>,
}

impl YourProvider {
    pub fn new(model_id: String) -> Self {
        Self {
            model_id,
            base_url: "https://api.your-provider.com/v1".to_string(),
            provider_name: "your-provider".to_string(),
            client: Client::new(),
            compat: None,
        }
    }

    // Builder methods following the established pattern:
    pub fn with_base_url(mut self, url: String) -> Self {
        self.base_url = url;
        self
    }

    pub fn with_provider_name(mut self, name: String) -> Self {
        self.provider_name = name;
        self
    }

    pub fn with_compat(mut self, compat: Option<CompatConfig>) -> Self {
        self.compat = compat;
        self
    }

    pub fn with_client(mut self, client: Client) -> Self {
        self.client = client;
        self
    }
}

#[async_trait]
impl Provider for YourProvider {
    fn name(&self) -> &str { &self.provider_name }
    fn api(&self) -> &str { "your-api-type" }
    fn model_id(&self) -> &str { &self.model_id }

    async fn stream(
        &self,
        context: &Context,
        options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        // 1. Build provider-specific request body from context + options
        // 2. Resolve API key from options.api_key
        // 3. Send HTTP request via self.client
        // 4. Parse SSE/streaming response into StreamEvent items
        // 5. Return as a Stream
        todo!()
    }
}
```

**Required `StreamEvent` variants to emit:**
- `StreamEvent::TextDelta { text }` - For each text chunk
- `StreamEvent::ThinkingDelta { text }` - For thinking/reasoning tokens (if supported)
- `StreamEvent::ToolCall { id, name, arguments }` - For tool use
- `StreamEvent::Done { reason, message }` - Final event with complete `AssistantMessage`

### Step 8: Register in Provider Factory

**File:** `src/providers/mod.rs`

1. Add module declaration:
   ```rust
   pub mod your_provider;
   ```

2. Add route variant to `ProviderRouteKind`:
   ```rust
   NativeYourProvider,
   ```

3. Add `as_str()` match arm:
   ```rust
   Self::NativeYourProvider => "native:your-provider",
   ```

4. Add routing in `resolve_provider_route()`:
   ```rust
   "your-provider" => ProviderRouteKind::NativeYourProvider,
   ```

5. Add construction in `create_provider()`:
   ```rust
   ProviderRouteKind::NativeYourProvider => Ok(Arc::new(
       your_provider::YourProvider::new(entry.model.id.clone())
           .with_base_url(entry.model.base_url.clone())
           .with_compat(entry.compat.clone())
           .with_client(client),
   )),
   ```

### Step 9: Add URL Normalization (If Needed)

If the provider's base URL needs normalization (e.g., appending `/chat/completions`), add a helper:

```rust
pub fn normalize_your_provider_base(base_url: &str) -> String {
    // See normalize_openai_base() and normalize_cohere_base() for patterns
}
```

---

## Path C: Native Adapter Required (Custom Auth/Runtime)

Complete **all steps from Path B**, then:

### Step 10: Implement Custom Auth

**File:** `src/auth.rs`

If the provider uses non-standard auth (OAuth, service keys, device flow):

1. Add auth constants (client ID, URLs, scopes)
2. Implement auth flow functions (e.g., `start_your_provider_oauth()`, `complete_your_provider_oauth()`)
3. Add credential type variant to `AuthCredential` enum if needed

**Existing auth patterns to follow:**
- **OAuth (browser-based):** See Anthropic OAuth (`start_anthropic_oauth`, `complete_anthropic_oauth`)
- **Device flow:** See GitHub Copilot (`start_github_device_flow`)
- **Service key (client credentials):** See SAP AI Core (`resolve_sap_credentials`)
- **AWS IAM:** See Bedrock (`resolve_aws_credentials_async`)

### Step 11: Add Runtime Resolution

**File:** `src/providers/mod.rs`

If the provider needs runtime configuration beyond base URL (e.g., Azure needs resource/deployment/api-version):

```rust
fn resolve_your_provider_runtime(entry: &ModelEntry) -> Result<YourProviderRuntime> {
    // Extract config from entry.model.base_url, env vars, etc.
}
```

See `resolve_azure_provider_runtime()` and `vertex::resolve_vertex_provider_runtime()` for patterns.

---

## Auth Resolution Chain

The API key resolution follows this precedence (first match wins):

1. **CLI override:** `--api-key <KEY>` flag
2. **Environment variable:** Provider-specific env vars from `auth_env_keys` in metadata
3. **Auth storage (auth.json):** Saved credentials from `/login` command
4. **Canonical fallback:** If provider has aliases, tries canonical ID in auth storage

**Code path:** `AuthStorage::resolve_api_key()` in `src/auth.rs:316` → `app::resolve_api_key()` in `src/app.rs:629`

---

## `CompatConfig` Reference

When a provider's wire format deviates from the standard, use `CompatConfig` overrides in `models.json`:

| Field | Type | Purpose |
|-------|------|---------|
| `supports_store` | `bool` | Provider supports `store: true` in requests |
| `supports_developer_role` | `bool` | Provider accepts `developer` role instead of `system` |
| `supports_reasoning_effort` | `bool` | Provider supports `reasoning_effort` parameter |
| `supports_usage_in_streaming` | `bool` | Usage stats arrive in stream events (not just final) |
| `supports_tools` | `bool` | Provider supports tool use |
| `supports_streaming` | `bool` | Provider supports streaming responses |
| `supports_parallel_tool_calls` | `bool` | Provider can invoke multiple tools in one turn |
| `max_tokens_field` | `String` | Override field name (e.g., `"max_completion_tokens"` for o1) |
| `system_role_name` | `String` | Override system role (e.g., `"developer"` for some providers) |
| `stop_reason_field` | `String` | Override stop-reason field name in responses |

---

## Testing Requirements

Each provider must satisfy the test obligations defined in its metadata (`ProviderTestObligations`):

### Unit Tests

**Location:** `src/providers/<name>.rs` (inline `#[cfg(test)] mod tests`)

- Request body construction (verify JSON matches provider spec)
- Response parsing (valid and malformed responses)
- URL normalization
- Auth header injection

### Contract Tests

**Location:** `tests/provider_native_contract.rs` or `tests/provider_streaming.rs`

- VCR cassette-based tests verifying real API wire format
- Tool call round-trips
- Error response handling (auth errors, rate limits, malformed responses)

### Conformance Tests

**Location:** `tests/fixtures/provider_streaming/`

- Fixture files with recorded API interactions
- Verify `StreamEvent` sequence matches expectations

### E2E Tests

**Location:** `tests/e2e_*.rs` or `scripts/e2e/`

- Full agent loop with provider (VCR playback)
- Multi-turn conversations
- Tool use scenarios

### Running Provider Tests

```bash
# All provider tests
cargo test provider

# Specific provider
cargo test anthropic
cargo test openai
cargo test gemini

# Contract tests
cargo test provider_native_contract

# Streaming conformance
cargo test provider_streaming
```

---

## Evidence and Artifact Updates

After adding a provider, update these artifacts:

| Artifact | Location | What to Update |
|----------|----------|----------------|
| Provider metadata tests | `src/provider_metadata.rs` (inline tests) | Add test for new canonical/alias resolution |
| Provider routing tests | `src/providers/mod.rs` (inline tests) | Add test for route resolution |
| README env vars | `README.md` | Add env var to table |
| models.json schema | User-facing docs | Document available models |
| CI scripts | `.github/workflows/` | Add env var to test matrix if needed |

---

## Common Pitfalls

1. **Forgetting `auth_env_keys`**: The auth resolver won't find env vars unless they're listed in metadata.

2. **Wrong `api` field**: OpenAI-compatible providers must use `"openai-completions"` or `"openai-responses"`, not custom strings. The routing falls through to `Api::Custom` which fails.

3. **Missing URL normalization**: The OpenAI completions provider appends `/chat/completions` to base URLs that don't already end with it. If your provider's base URL includes the path, this can cause double paths.

4. **`oauth_config: None`**: Every `ModelEntry` construction site must include `oauth_config: None` (or `Some(...)` for OAuth providers). There are ~9 construction sites in `src/models.rs` and `src/extensions.rs`.

5. **Case sensitivity in provider IDs**: `provider_metadata()` uses `eq_ignore_ascii_case` for lookup, but canonical IDs should always be lowercase.

6. **Clippy strictness**: The project uses `-D warnings` with pedantic + nursery lints. Common issues:
   - `doc_markdown`: Put type names in backticks in doc comments
   - `too_many_lines`: Add `#[allow(clippy::too_many_lines)]` if unavoidable
   - `needless_borrows_for_generic_args`: Don't `&format!(...)` when `format!(...)` works

7. **VCR cassette matching**: Tests using VCR playback require exact body JSON match. If your provider adds extra fields, cassettes must be re-recorded.

---

## Checklist Summary

### OpenAI-Compatible Preset
- [ ] Add `ProviderMetadata` entry in `src/provider_metadata.rs`
- [ ] Add env var to `README.md`
- [ ] Add model entries to `models.json` (if well-known models exist)
- [ ] Run `cargo test provider_metadata::tests`
- [ ] Run `cargo check --all-targets && cargo clippy --all-targets -- -D warnings && cargo fmt --check`

### Built-In Native
- [ ] All items from OpenAI-Compatible Preset above
- [ ] Create `src/providers/<name>.rs` implementing `Provider` trait
- [ ] Add `pub mod <name>` to `src/providers/mod.rs`
- [ ] Add `ProviderRouteKind` variant and `as_str()` match
- [ ] Add routing in `resolve_provider_route()`
- [ ] Add construction in `create_provider()`
- [ ] Add URL normalization function (if needed)
- [ ] Write unit tests in provider module
- [ ] Write contract/conformance tests with VCR cassettes

### Native Adapter Required
- [ ] All items from Built-In Native above
- [ ] Implement custom auth flow in `src/auth.rs`
- [ ] Add runtime resolution function in `src/providers/mod.rs`
- [ ] Test auth flow end-to-end
- [ ] Document auth setup in troubleshooting docs
