# Provider Onboarding Playbook

This playbook is the execution-focused companion to `providers.md`.

Use it when you need to:
- onboard a provider configuration quickly,
- debug provider auth/routing failures without guesswork, and
- add or update provider support without creating metadata/factory drift.

Primary bead coverage:
- `bd-3uqg.9` (parent)
- working draft support for `bd-3uqg.9.2` and `bd-3uqg.9.3`
- `bd-3uqg.9.4.1` (checklist below)

## Quick checklist: adding a new provider

### Determine onboarding mode

| Mode | When | Dedicated `.rs` file? | Example |
|---|---|---|---|
| `OpenAICompatiblePreset` | Standard OpenAI-compatible API | No | groq, deepinfra, mistral |
| `BuiltInNative` | Proprietary API format | Yes | anthropic, google, cohere |
| `NativeAdapterRequired` | Special auth or routing | Yes | azure, bedrock, copilot, gitlab |

### Phase 1: Metadata (`src/provider_metadata.rs`)

- [ ] Add `ProviderMetadata` entry to `PROVIDER_METADATA` array
  - `canonical_id`: primary lowercase provider name
  - `aliases`: alternative names (e.g., `["gemini"]` for google)
  - `auth_env_keys`: env var chain in priority order (e.g., `["GROQ_API_KEY"]`)
  - `onboarding`: one of the three modes above
  - `routing_defaults`: `Some(ProviderRoutingDefaults { api, base_url, auth_header, reasoning, input, context_window, max_tokens })` or `None` for native adapters
  - `test_obligations`: typically `TEST_REQUIRED`
- [ ] Verify: `provider_metadata("your-id")` returns entry
- [ ] Verify: aliases resolve via `canonical_provider_id("alias")`

### Phase 2: Factory routing (`src/providers/mod.rs`) — native only

Skip this phase for `OpenAICompatiblePreset` providers.

- [ ] Add `pub mod {provider};` to module declarations (line ~25)
- [ ] Add variant to `ProviderRouteKind` enum (line ~70)
- [ ] Add variant string in `as_str()` match (line ~89)
- [ ] Add canonical ID pattern in `resolve_provider_route()` (line ~121)
- [ ] Add instantiation case in `create_provider()` (line ~679)

### Phase 3: Implementation (`src/providers/{provider}.rs`) — native only

Skip this phase for `OpenAICompatiblePreset` providers.

- [ ] Create struct with: `client: Client`, `model_id: String`, `provider_name: String`, `base_url: String`, `compat: Option<CompatConfig>`
- [ ] Implement builder methods: `new()`, `with_base_url()`, `with_provider_name()`, `with_compat()`, `with_client()`
- [ ] Implement `Provider` trait: `name()`, `api()`, `model_id()`, `stream()`
- [ ] Handle streaming response parsing (JSON -> `StreamEvent` variants)
- [ ] Handle error responses (auth, rate limit, server error)

### Phase 4: Authentication (`src/auth.rs`)

- [ ] Simple API key: no changes (metadata-driven via `auth_env_keys`)
- [ ] AWS SigV4 (Bedrock-style): use `resolve_aws_credentials()`
- [ ] OAuth / token exchange: implement in provider `.rs` or `auth.rs`

### Phase 5: Tests

- [ ] `tests/provider_factory.rs`: factory instantiation test
- [ ] `tests/provider_metadata_comprehensive.rs`: metadata lookup + alias tests
- [ ] `tests/provider_native_contract.rs`: VCR-backed streaming test (native providers)
- [ ] `tests/provider_native_verify.rs`: conformance verification (native providers)
- [ ] Create VCR cassettes in `tests/fixtures/vcr/` if needed
- [ ] Run: `cargo test --test provider_factory --test provider_metadata_comprehensive`

### Phase 6: Verification

- [ ] `cargo check --all-targets`
- [ ] `cargo clippy --all-targets -- -D warnings`
- [ ] `cargo fmt --check`
- [ ] `cargo test --lib` (all 3269+ tests pass)
- [ ] Provider appears in `pi --list-models` (with API key set)
- [ ] `pi --provider {id} --model {model} -p "test"` works (if live key available)

## Scope and source of truth

Use these files as authoritative:
- Provider metadata (canonical IDs, aliases, env keys, routing defaults): `../src/provider_metadata.rs`
- Runtime route selection and provider factory dispatch: `../src/providers/mod.rs`
- API key resolution precedence: `../src/app.rs`, `../src/auth.rs`, `../src/models.rs`
- Existing provider baseline and matrix: `providers.md`
- Error-hint taxonomy and remediation messages: `../src/error.rs`

Use these tests/artifacts as verification anchors:
- Factory/routing behavior: `../tests/provider_factory.rs`
- Metadata invariants and alias correctness: `../tests/provider_metadata_comprehensive.rs`
- Streaming/provider contracts: `../tests/provider_streaming.rs`
- Live parity/artifact lanes: `../tests/e2e_cross_provider_parity.rs`, `../tests/e2e_live_harness.rs`, `../tests/e2e_live.rs`

## Runtime model: how provider selection actually works

Selection pipeline:
1. Model entry is chosen (`--provider/--model`, defaults, or scoped models) in `../src/app.rs`.
2. API key resolution is attempted in this order:
   - `--api-key`
   - unexpired OAuth/bearer credential in `auth.json`
   - provider env vars (`provider_auth_env_keys` via `../src/provider_metadata.rs`)
   - stored API key in `auth.json`
   - supported external coding-CLI credential (global auth storage only)
   - `models.json` `providers.<id>.apiKey` (fallback)
3. Provider route is selected in `resolve_provider_route(...)` in `../src/providers/mod.rs`.
4. A concrete provider implementation is created in `create_provider(...)`.

Important caveat:
- `github-copilot` participates in the same generic resolution chain. Its
  provider env order is `GITHUB_COPILOT_API_KEY`, then `GITHUB_TOKEN`; a valid
  stored bearer/OAuth credential still outranks ambient env, and an inline
  `models.json` provider key is only the app-level fallback.
- `amazon-bedrock` and `sap-ai-core` are request-time/provider-managed auth
  routes. Do not document AWS region/profile/session-token components or SAP
  client credentials as raw bearer API keys.

## Provider family map

| Family | Typical canonical IDs | Route style | Core config surface |
|---|---|---|---|
| Built-in native | `anthropic`, `openai`, `google`, `cohere` | Native provider modules | Usually `--provider/--model` + env key |
| OpenAI-compatible presets | `openrouter`, `xai`, `deepseek`, `groq`, `cloudflare-ai-gateway`, `cloudflare-workers-ai`, etc. | API fallback to `openai-completions` | Provider metadata defaults + standard bearer auth |
| Native adapters | `azure-openai`, `google-vertex`, `github-copilot`, `gitlab`, `amazon-bedrock`, `sap-ai-core` | Dedicated adapter route in factory | Provider-specific env/config requirements |

## Copy-paste configuration examples

### 1) Built-in native providers (quick CLI)

```bash
export ANTHROPIC_API_KEY="..."
export OPENAI_API_KEY="..."
export GOOGLE_API_KEY="..."
export COHERE_API_KEY="..."

pi --provider anthropic --model claude-sonnet-4-5 -p "Say hello"
pi --provider openai --model gpt-4o-mini -p "Say hello"
pi --provider google --model gemini-2.5-flash -p "Say hello"
pi --provider cohere --model command-r-plus -p "Say hello"
```

Expected check:
- Command returns model text output with no provider/auth error.

### 2) OpenAI-compatible preset providers (`models.json` optional)

OpenRouter minimal path (env-only):

```bash
export OPENROUTER_API_KEY="..."
pi --provider openrouter --model openai/gpt-4o-mini -p "Say hello"
```

OpenRouter advanced path (explicit config + routing metadata + attribution overrides):

```json
{
  "providers": {
    "openrouter": {
      "baseUrl": "https://openrouter.ai/api/v1",
      "api": "openai-completions",
      "compat": {
        "openRouterRouting": {
          "provider": { "order": ["anthropic", "openai"] }
        }
      },
      "models": [
        {
          "id": "anthropic/claude-3.5-sonnet",
          "name": "OpenRouter Claude 3.5 Sonnet",
          "compat": {
            "customHeaders": {
              "X-Debug-Trace": "openrouter-doc-example"
            }
          }
        }
      ]
    }
  }
}
```

```bash
export OPENROUTER_API_KEY="..."
# Optional attribution overrides (defaults are injected if absent)
export OPENROUTER_HTTP_REFERER="https://example.com/pi-agent-rust"
export OPENROUTER_X_TITLE="Pi Agent Rust (Docs Example)"

# Provider alias and model alias are both supported:
pi --provider open-router --model claude-3.5-sonnet -p "Say hello"
```

Expected OpenRouter checks:
- OpenRouter resolves through `openai-completions` to `/chat/completions` route shape.
- Provider alias `open-router` resolves to canonical `openrouter`.
- Model alias forms (for example `claude-3.5-sonnet`) normalize to canonical IDs.
- `openRouterRouting` is forwarded when configured and must be a JSON object.
- `HTTP-Referer` and `X-Title` headers are injected by default unless already set by compat/per-request headers.

OpenRouter evidence anchors:
- `tests/provider_native_contract.rs` (`openrouter_contract::*`)
- `tests/provider_native_verify.rs` (`openrouter_conformance::*`)
- `tests/e2e_provider_scenarios.rs` (`e2e_openai_compatible_wave_presets`, `e2e_error_auth_all_families`, `e2e_error_rate_limit_all_families`, `e2e_error_schema_drift_all_families`)
- `src/providers/openai.rs` (`test_build_request_applies_openrouter_routing_overrides`, `test_stream_openrouter_injects_default_attribution_headers`, `test_stream_openrouter_respects_explicit_attribution_headers`)
- `tests/main_cli_selection.rs` (`select_model_and_thinking_resolves_model_flag_with_provider_prefixed_openrouter_id`, `select_model_and_thinking_resolves_openrouter_provider_alias_and_model_alias`)

Other preset explicit config example (Cloudflare AI Gateway):

```json
{
  "providers": {
    "cloudflare-ai-gateway": {
      "baseUrl": "https://gateway.ai.cloudflare.com/v1/<account_id>/<gateway_id>/openai",
      "models": [
        { "id": "gpt-4o-mini" }
      ]
    }
  }
}
```

```bash
export CLOUDFLARE_API_TOKEN="..."
pi --provider cloudflare-ai-gateway --model gpt-4o-mini -p "Say hello"
```

Expected check:
- Factory resolves to `openai-completions` route for these providers (see `../tests/provider_factory.rs`).

Wave A verification lock for the preset family (`bd-3uqg.4.4`):
- `wave_a_presets_resolve_openai_compat_defaults_and_factory_route`
- `wave_a_openai_compat_streams_use_chat_completions_path_and_bearer_auth`

### 2a) Alias migration example (`fireworks-ai` -> `fireworks`)

Legacy config (still supported):

```json
{
  "providers": {
    "fireworks-ai": {
      "models": [
        { "id": "accounts/fireworks/models/llama-v3p3-70b-instruct" }
      ]
    }
  }
}
```

Recommended config (canonical):

```json
{
  "providers": {
    "fireworks": {
      "models": [
        { "id": "accounts/fireworks/models/llama-v3p3-70b-instruct" }
      ]
    }
  }
}
```

Migration behavior guarantees:
- Both IDs resolve to `openai-completions` with base `https://api.fireworks.ai/inference/v1`.
- Both IDs use the same auth env mapping (`FIREWORKS_API_KEY`).
- Alias parity is lock-tested in `fireworks_ai_alias_migration_matches_fireworks_canonical_defaults`.

### 2b) Wave B1 canonical IDs (regional + coding-plan)

Batch B1 lock tests (`bd-3uqg.5.2`):
- `wave_b1_presets_resolve_metadata_defaults_and_factory_route`
- `wave_b1_alibaba_cn_openai_compat_streams_use_chat_completions_path_and_bearer_auth`
- `wave_b1_anthropic_compat_streams_use_messages_path_and_x_api_key`
- `wave_b1_family_coherence_with_existing_moonshot_and_alibaba_mappings`

Representative smoke/e2e checks (`provider_native_verify`):
- `wave_b1_smoke::b1_alibaba_cn_{simple_text,tool_call_single,error_auth_401}`
- `wave_b1_smoke::b1_kimi_for_coding_{simple_text,tool_call_single,error_auth_401}`
- `wave_b1_smoke::b1_minimax_{simple_text,tool_call_single,error_auth_401}`
- Command: `cargo test --test provider_native_verify b1_ -- --nocapture`
- Generated fixtures:
  `tests/fixtures/vcr/verify_alibaba-cn_*.json`,
  `tests/fixtures/vcr/verify_kimi-for-coding_*.json`,
  `tests/fixtures/vcr/verify_minimax_*.json`.

Key mapping decisions:
- `kimi` remains an alias of canonical `moonshotai`.
- `kimi-for-coding` is distinct and routes to Anthropic-compatible path with `KIMI_API_KEY`.
- `alibaba-cn` is distinct from `alibaba`/`dashscope` and uses CN DashScope base URL.
- `minimax*` variants are distinct canonical IDs with shared family auth/env mapping:
  `MINIMAX_API_KEY` for global, `MINIMAX_CN_API_KEY` for CN.

Representative `models.json` snippet:

```json
{
  "providers": {
    "alibaba-cn": {
      "models": [{ "id": "qwen-plus" }]
    },
    "kimi-for-coding": {
      "models": [{ "id": "k2p5" }]
    },
    "minimax-coding-plan": {
      "models": [{ "id": "MiniMax-M2.1" }]
    }
  }
}
```

### 2c) Wave B2 canonical IDs (regional + cloud OpenAI-compatible)

Batch B2 lock tests (`bd-3uqg.5.1`):
- `wave_b2_presets_resolve_metadata_defaults_and_factory_route`
- `wave_b2_openai_compat_streams_use_chat_completions_path_and_bearer_auth`
- `wave_b2_moonshot_cn_and_global_moonshot_mapping_are_distinct`

Representative smoke/e2e checks (`provider_native_verify`):
- `wave_b2_smoke::b2_modelscope_{simple_text,tool_call_single,error_auth_401}`
- `wave_b2_smoke::b2_moonshotai_cn_{simple_text,tool_call_single,error_auth_401}`
- `wave_b2_smoke::b2_nebius_{simple_text,tool_call_single,error_auth_401}`
- `wave_b2_smoke::b2_ovhcloud_{simple_text,tool_call_single,error_auth_401}`
- `wave_b2_smoke::b2_scaleway_{simple_text,tool_call_single,error_auth_401}`
- Command: `cargo test --test provider_native_verify b2_ -- --nocapture`
- Generated fixtures:
  `tests/fixtures/vcr/verify_modelscope_*.json`,
  `tests/fixtures/vcr/verify_moonshotai-cn_*.json`,
  `tests/fixtures/vcr/verify_nebius_*.json`,
  `tests/fixtures/vcr/verify_ovhcloud_*.json`,
  `tests/fixtures/vcr/verify_scaleway_*.json`.

Key mapping decisions:
- `modelscope`, `nebius`, `ovhcloud`, and `scaleway` are onboarded as canonical OpenAI-compatible preset IDs.
- `moonshotai-cn` is a distinct canonical regional ID and does not alias to `moonshotai`.
- `moonshotai` and `moonshotai-cn` intentionally share `MOONSHOT_API_KEY` while retaining distinct base URLs.

Representative `models.json` snippet:

```json
{
  "providers": {
    "modelscope": {
      "models": [{ "id": "ZhipuAI/GLM-4.5" }]
    },
    "moonshotai-cn": {
      "models": [{ "id": "kimi-k2-0905-preview" }]
    },
    "nebius": {
      "models": [{ "id": "NousResearch/hermes-4-70b" }]
    },
    "ovhcloud": {
      "models": [{ "id": "mixtral-8x7b-instruct-v0.1" }]
    },
    "scaleway": {
      "models": [{ "id": "qwen3-235b-a22b-instruct-2507" }]
    }
  }
}
```

### 2d) Wave B3 canonical IDs (regional + coding-plan OpenAI-compatible)

Batch B3 lock tests (`bd-3uqg.5.3`):
- `wave_b3_presets_resolve_metadata_defaults_and_factory_route`
- `wave_b3_openai_compat_streams_use_chat_completions_path_and_bearer_auth`
- `wave_b3_family_and_coding_plan_variants_are_distinct`
- `ad_hoc_batch_b3_defaults_resolve_expected_routes`
- `ad_hoc_batch_b3_coding_plan_and_regional_variants_remain_distinct`

Representative smoke/e2e checks (`provider_native_verify`):
- `wave_b3_smoke::b3_siliconflow_{simple_text,tool_call_single,error_auth_401}`
- `wave_b3_smoke::b3_siliconflow_cn_{simple_text,tool_call_single,error_auth_401}`
- `wave_b3_smoke::b3_upstage_{simple_text,tool_call_single,error_auth_401}`
- `wave_b3_smoke::b3_venice_{simple_text,tool_call_single,error_auth_401}`
- `wave_b3_smoke::b3_zai_{simple_text,tool_call_single,error_auth_401}`
- `wave_b3_smoke::b3_zai_coding_{simple_text,tool_call_single,error_auth_401}`
- `wave_b3_smoke::b3_zhipuai_{simple_text,tool_call_single,error_auth_401}`
- `wave_b3_smoke::b3_zhipuai_coding_{simple_text,tool_call_single,error_auth_401}`
- Command: `cargo test --test provider_native_verify b3_ -- --nocapture`
- Generated fixtures:
  `tests/fixtures/vcr/verify_siliconflow_*.json`,
  `tests/fixtures/vcr/verify_siliconflow-cn_*.json`,
  `tests/fixtures/vcr/verify_upstage_*.json`,
  `tests/fixtures/vcr/verify_venice_*.json`,
  `tests/fixtures/vcr/verify_zai_*.json`,
  `tests/fixtures/vcr/verify_zai-coding-plan_*.json`,
  `tests/fixtures/vcr/verify_zhipuai_*.json`,
  `tests/fixtures/vcr/verify_zhipuai-coding-plan_*.json`.

Key mapping decisions:
- `siliconflow` and `siliconflow-cn` are distinct canonical regional IDs with separate auth env keys (`SILICONFLOW_API_KEY`, `SILICONFLOW_CN_API_KEY`).
- `zai` and `zai-coding-plan` are distinct canonical IDs sharing `ZHIPU_API_KEY` but using different base URLs.
- `zhipuai` and `zhipuai-coding-plan` are distinct canonical IDs sharing `ZHIPU_API_KEY` but using different base URLs.

Representative `models.json` snippet:

```json
{
  "providers": {
    "siliconflow": {
      "models": [{ "id": "Qwen/Qwen3-Coder-480B-A35B-Instruct" }]
    },
    "upstage": {
      "models": [{ "id": "solar-pro2" }]
    },
    "venice": {
      "models": [{ "id": "venice-uncensored" }]
    },
    "zai-coding-plan": {
      "models": [{ "id": "glm-4.5" }]
    },
    "zhipuai-coding-plan": {
      "models": [{ "id": "glm-4.5" }]
    }
  }
}
```

### 2e) Wave C canonical IDs (local/self-hosted/gateway staging)

Source for defaults in this section:
- `https://models.dev/api.json` (queried on 2026-02-12)
- Extraction command:

```bash
curl -s https://models.dev/api.json | jq '{
  baseten: {api: ."baseten".api, env: ."baseten".env},
  llama: {api: ."llama".api, env: ."llama".env},
  lmstudio: {api: ."lmstudio".api, env: ."lmstudio".env},
  "ollama-cloud": {api: ."ollama-cloud".api, env: ."ollama-cloud".env},
  opencode: {api: ."opencode".api, env: ."opencode".env},
  vercel: {api: ."vercel".api, env: ."vercel".env},
  zenmux: {api: ."zenmux".api, env: ."zenmux".env}
}'
```

Current Wave C routing stance:
- `baseten`, `llama`, `lmstudio`, and `ollama-cloud` are onboarded as OpenAI-compatible presets (metadata + factory verified, VCR pending).
- `opencode` and `vercel` are onboarded as OpenAI-compatible presets with VCR verification (3 scenarios each).
- `zenmux` is onboarded as an Anthropic-compatible preset with VCR verification (3 scenarios).

Wave C defaults (from `models.dev`):

| Provider ID | API family target | Default base URL | Auth env |
|---|---|---|---|
| `baseten` | `openai-completions` | `https://inference.baseten.co/v1` | `BASETEN_API_KEY` |
| `llama` | `openai-completions` | `https://api.llama.com/compat/v1/` | `LLAMA_API_KEY` |
| `lmstudio` | `openai-completions` | `http://127.0.0.1:1234/v1` | `LMSTUDIO_API_KEY` |
| `ollama-cloud` | `openai-completions` | `https://ollama.com/v1` | `OLLAMA_API_KEY` |
| `opencode` | `openai-completions` | `https://opencode.ai/zen/v1` | `OPENCODE_API_KEY` |
| `vercel` | gateway-wrapper (`@ai-sdk/gateway`) | no static API URL in `models.dev` | `AI_GATEWAY_API_KEY` |
| `zenmux` | `anthropic-messages` target (Anthropic-style gateway) | `https://zenmux.ai/api/anthropic/v1` | `ZENMUX_API_KEY` |

Representative `models.json` for unblocked Wave C presets:

```json
{
  "providers": {
    "baseten": {
      "models": [{ "id": "moonshotai/Kimi-K2-Instruct-0905" }]
    },
    "llama": {
      "models": [{ "id": "llama-3.3-70b-instruct" }]
    },
    "lmstudio": {
      "models": [{ "id": "openai/gpt-oss-20b" }]
    },
    "ollama-cloud": {
      "models": [{ "id": "glm-4.7" }]
    }
  }
}
```

Special-routing status:
- `opencode`, `vercel`, and `zenmux` are now onboarded and VCR-verified as preset providers.
- VCR cassettes: `tests/fixtures/vcr/verify_opencode_*.json`, `tests/fixtures/vcr/verify_vercel_*.json`, `tests/fixtures/vcr/verify_zenmux_*.json`.

### 3) Azure OpenAI (`azure-openai` / aliases `azure`, `azure-cognitive-services`)

```json
{
  "providers": {
    "azure-openai": {
      "baseUrl": "https://<resource>.openai.azure.com",
      "models": [
        { "id": "gpt-4o" }
      ]
    }
  }
}
```

```bash
export AZURE_OPENAI_API_KEY="..."
# Optional overrides used by runtime resolver:
# export AZURE_OPENAI_RESOURCE="<resource>"
# export AZURE_OPENAI_DEPLOYMENT="<deployment>"
# export AZURE_OPENAI_API_VERSION="2024-08-01-preview"

pi --provider azure-openai --model gpt-4o -p "Say hello"
```

Expected check:
- Route is native Azure path.
- Missing deployment/resource failures include explicit remediation text from `resolve_azure_provider_runtime(...)` in `../src/providers/mod.rs`.

### 4) Google Vertex (`google-vertex` / alias `vertexai`)

Recommended explicit base URL shape:

```json
{
  "providers": {
    "google-vertex": {
      "baseUrl": "https://us-central1-aiplatform.googleapis.com/v1/projects/<project>/locations/us-central1/publishers/google/models/gemini-2.0-flash",
      "models": [
        { "id": "gemini-2.0-flash", "api": "google-vertex" }
      ]
    }
  }
}
```

```bash
export GOOGLE_CLOUD_API_KEY="..."   # or VERTEX_API_KEY
export GOOGLE_CLOUD_PROJECT="<project>"   # optional if embedded in baseUrl
export GOOGLE_CLOUD_LOCATION="us-central1" # optional if embedded in baseUrl

pi --provider google-vertex --model gemini-2.0-flash -p "Say hello"
```

Expected check:
- Provider route is native vertex.
- Missing project/auth errors match messages in `../src/providers/vertex.rs`.

### 5) GitHub Copilot (`github-copilot` / alias `copilot`)

```json
{
  "providers": {
    "github-copilot": {
      "baseUrl": "https://api.github.com",
      "models": [
        { "id": "gpt-4o" }
      ]
    }
  }
}
```

```bash
export GITHUB_TOKEN="..."   # or GITHUB_COPILOT_API_KEY
pi --provider github-copilot --model gpt-4o -p "Say hello"
```

Expected check:
- Provider performs token exchange against GitHub API before chat call.
- If token exchange fails, error contains Copilot-specific diagnostic context.

### 6) GitLab Duo (`gitlab` / alias `gitlab-duo`)

```json
{
  "providers": {
    "gitlab": {
      "baseUrl": "https://gitlab.com",
      "models": [
        { "id": "gitlab-duo-chat", "api": "gitlab-chat" }
      ]
    }
  }
}
```

```bash
export GITLAB_TOKEN="..."   # or GITLAB_API_KEY
pi --provider gitlab --model gitlab-duo-chat -p "Say hello"
```

Expected check:
- Provider sends request to `/api/v4/chat/completions` and returns a non-streaming done event path.

### 7) Bedrock / SAP AI Core (native adapters - VCR-verified)

Current status:
- `amazon-bedrock` and `sap-ai-core` are classified as `native-adapter-required` and are now VCR-verified.
- Auth/env mapping exists in `../src/provider_metadata.rs` and `../src/auth.rs`.
- VCR cassettes: `tests/fixtures/vcr/verify_bedrock_*.json` (4 scenarios), `tests/fixtures/vcr/verify_sap_ai_core_*.json` (6 scenarios).
- Parity evidence: [`docs/provider-native-parity-report.json`](provider-native-parity-report.json).

Bedrock auth:
- SigV4 credentials: `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`
- Bearer token alternative: `AWS_BEARER_TOKEN_BEDROCK`

SAP AI Core auth:
- OAuth2 client credentials: `SAP_AI_CORE_CLIENT_ID`, `SAP_AI_CORE_CLIENT_SECRET`, `SAP_AI_CORE_TOKEN_URL`

## Troubleshooting matrix (symptom -> action)

| Symptom | Fast diagnosis | Remediation |
|---|---|---|
| `Missing API key` / auth error at startup | Check provider env key mapping in `provider_auth_env_keys(...)` | Set provider env var, or `--api-key`, or persisted `auth.json`; re-run |
| `OpenAI API error (HTTP 401)` when provider is `openrouter` | Invalid/missing OpenRouter key (or wrong key routed to provider alias) | Set `OPENROUTER_API_KEY` (or `--api-key`) and re-run a known-good model (`openrouter/auto`, `openai/gpt-4o-mini`). Evidence: `tests/provider_native_contract.rs::openrouter_contract::error_401_auth_failure`, `tests/provider_native_verify.rs::openrouter_conformance::error_auth_401` |
| `OpenAI API error (HTTP 429)` when provider is `openrouter` | Provider/model quota or rate limit | Retry with backoff, reduce request/token size, or switch model/provider route. Evidence: `tests/provider_native_contract.rs::openrouter_contract::error_429_rate_limit`, `tests/provider_native_verify.rs::openrouter_conformance::error_rate_limit_429` |
| `openRouterRouting must be a JSON object when configured` | `compat.openRouterRouting` is not an object in `models.json` | Change `compat.openRouterRouting` to an object (for example `{ "provider": { "order": ["openai"] } }`). Evidence: runtime guard in `src/providers/openai.rs::apply_openrouter_routing_overrides`, behavior lock in `src/providers/openai.rs::test_build_request_applies_openrouter_routing_overrides` |
| `Provider not implemented (api: ...)` | Route fell through unknown provider/api in `resolve_provider_route(...)` | Fix provider ID/api in `models.json`; verify canonical ID or alias in `../src/provider_metadata.rs` |
| Azure missing resource/deployment | Resolver could not infer `resource` / `deployment` from base URL/env | Set `AZURE_OPENAI_RESOURCE`, `AZURE_OPENAI_DEPLOYMENT`, or include full Azure host/deployments path |
| Vertex missing project | Project not in base URL and not in env | Set `GOOGLE_CLOUD_PROJECT` or `VERTEX_PROJECT`; or encode project in base URL |
| Vertex missing token | No `api_key` and no `GOOGLE_CLOUD_API_KEY`/`VERTEX_API_KEY` | Set one of those env vars (bearer token/access token) |
| Copilot auth failure | GitHub token missing/invalid or token exchange rejected | Set `GITHUB_COPILOT_API_KEY`/`GITHUB_TOKEN`; verify Copilot entitlement |
| GitLab auth failure | Missing or invalid PAT/OAuth token | Set `GITLAB_TOKEN` or `GITLAB_API_KEY`; validate instance URL and scopes |
| 429/quota/5xx | Provider-side limit or outage | Retry policy tuning in settings, reduce request size, or switch model/provider |

## OAuth and login caveat

Interactive slash help now reflects the broader `/login` surface:
- Built-in OAuth providers: `anthropic`, `openai-codex`, `google-gemini-cli`, `google-antigravity`, `kimi-for-coding`, `github-copilot`, `gitlab`.
- Metadata-backed API key prompts are available for providers that support interactive API-key login.

For providers without a built-in OAuth flow or API-key prompt, prefer explicit env/auth.json setup. Extension providers require an `oauth_config` entry to enable `/login`.

## Mandatory test and logging obligations

This section defines the minimum required test and logging evidence for adding or modifying providers. Every provider entry in `PROVIDER_METADATA` has `test_obligations: TEST_REQUIRED`, which sets all four categories to `true`.

### Test obligation categories

The `ProviderTestObligations` struct (`src/provider_metadata.rs`) enforces four mandatory categories:

| Category | Test suite | What it proves |
|----------|-----------|----------------|
| **unit** | `tests/provider_factory.rs` | Factory dispatch resolves the correct `ProviderRouteKind`; base URL normalization is correct; API/provider type round-trips work |
| **contract** | `tests/provider_native_contract.rs` | HTTP request payload shape, auth header construction, tool schema translation, and SSE→`StreamEvent` decoding are correct (native adapters only; OpenAI-compatible presets inherit the OpenAI contract) |
| **conformance** | `tests/provider_native_verify.rs` | VCR-backed playback of canonical scenarios produces expected stream events, tool calls, error codes, and stop reasons |
| **e2e** | `tests/e2e_provider_scenarios.rs` | Multi-provider deterministic workflows including text generation, tool calling, error handling, event ordering, and request body stability |

Live parity (optional, CI-gated):

| Suite | Gate | What it proves |
|-------|------|----------------|
| `tests/e2e_cross_provider_parity.rs` | `CI_E2E_TESTS=1` | Real API calls across 10 providers produce consistent event sequences, token usage, and error semantics |

### Minimum test counts per provider type

**OpenAI-compatible preset** (metadata + factory only):
- 1 factory dispatch test (wave batch test covers this)
- 3 drift-prevention snapshot updates (canonical ID, aliases, base URL)
- 3 VCR conformance scenarios: `simple_text`, `tool_call_single`, `error_auth_401`

**Native adapter** (dedicated `.rs` file):
- All of the above, plus:
- Contract tests for request shape, auth headers, tool schema, and response decoding
- 3 additional VCR conformance scenarios: `unicode_text`, `error_bad_request_400`, `error_rate_limit_429`
- At least 1 E2E family-level scenario

### Canonical VCR scenarios

The verification harness (`tests/provider_native_verify.rs`) defines 7 canonical scenarios. Each scenario has a structured expectation:

| Scenario tag | Messages | Tools | Expectation type | Key assertions |
|-------------|----------|-------|-----------------|----------------|
| `simple_text` | 1 user message | none | `Stream` | `min_text_deltas >= 1`, stop reason `EndTurn` |
| `unicode_text` | 1 user message (JP/emoji) | none | `Stream` | `require_unicode = true`, non-ASCII in output |
| `tool_call_single` | 1 user message | 1 tool def | `Stream` | `min_tool_calls >= 1`, stop reason `ToolUse` |
| `tool_call_multiple` | 1 user message | 2 tool defs | `Stream` | `min_tool_calls >= 2` |
| `error_auth_401` | 1 user message | none | `Error` | HTTP 401 status |
| `error_bad_request_400` | malformed | none | `Error` | HTTP 400 status |
| `error_rate_limit_429` | 1 user message | none | `Error` | HTTP 429 status |

### VCR cassette naming convention

Format: `verify_{provider_id}_{scenario_tag}.json`

Examples:
```
tests/fixtures/vcr/verify_anthropic_simple_text.json
tests/fixtures/vcr/verify_openai_tool_call_single.json
tests/fixtures/vcr/verify_gemini_error_rate_limit_429.json
tests/fixtures/vcr/verify_groq_error_auth_401.json
```

Place all cassettes in `tests/fixtures/vcr/`. The harness discovers them by convention.

### Failure-path expectations

Every provider **must** demonstrate correct error handling for these failure modes:

| Failure mode | HTTP status | Required evidence |
|-------------|------------|-------------------|
| Invalid/missing API key | 401 | VCR cassette `verify_{provider}_error_auth_401.json`; provider returns error event (not panic) |
| Malformed request body | 400 | VCR cassette `verify_{provider}_error_bad_request_400.json`; error message is parseable |
| Rate limit exceeded | 429 | VCR cassette `verify_{provider}_error_rate_limit_429.json`; error includes rate-limit context |

Contract tests (`provider_native_contract.rs`) for native adapters additionally validate:
- Auth failure produces structured error (not raw HTTP body)
- Rate limit errors include retry-after hints when available
- Server errors (5xx) are propagated as `StreamEvent::Error`

### JSONL logging obligations

All provider tests that use `TestHarness` must emit JSONL logs conforming to these schemas:

**Log schema (`pi.test.log.v2`)** — mandatory fields:

| Field | Type | Description |
|-------|------|-------------|
| `schema` | string | Must be `"pi.test.log.v2"` |
| `type` | string | Must be `"log"` |
| `trace_id` | string | Trace correlation ID (required in v2) |
| `seq` | integer | Monotonically increasing sequence number |
| `ts` | string | ISO-8601 timestamp |
| `t_ms` | integer | Elapsed milliseconds since test start |
| `level` | string | One of: `debug`, `info`, `warn`, `error` |
| `category` | string | Structured category (e.g., `setup`, `action`, `verify`, `teardown`) |
| `message` | string | Human-readable log message |

Optional fields: `span_id`, `parent_span_id`, `test`, `context` (key-value map).

**Artifact schema (`pi.test.artifact.v1`)** — mandatory fields:

| Field | Type | Description |
|-------|------|-------------|
| `schema` | string | Must be `"pi.test.artifact.v1"` |
| `type` | string | Must be `"artifact"` |
| `seq` | integer | Sequence number |
| `ts` | string | ISO-8601 timestamp |
| `t_ms` | integer | Elapsed milliseconds |
| `name` | string | Logical artifact name (e.g., `verification_report`) |
| `path` | string | File path to the artifact |

Optional fields: `test`, `size_bytes`, `sha256`.

### Logging category conventions

Use these categories consistently across provider tests:

| Category | When to use |
|----------|------------|
| `setup` | Test harness initialization, mock server start, VCR cassette loading |
| `action` | Provider creation, stream start, request sending |
| `verify` | Assertion checks, event sequence validation |
| `teardown` | Cleanup, resource release |
| `stream` | Individual stream event processing |
| `error` | Error handling paths |

### JSONL normalization and redaction

When producing deterministic test output (for snapshot comparison or CI), use `TestLogger::dump_jsonl_normalized()` which replaces:

| Placeholder | Replaces |
|------------|---------|
| `<TIMESTAMP>` | All ISO-8601 timestamps |
| `<PROJECT_ROOT>` | Cargo manifest directory |
| `<TEST_ROOT>` | Temp directory path |
| `<RUN_ID>` | `run-{uuid}` patterns |
| `<UUID>` | UUID strings |
| `<PORT>` | `http://127.0.0.1:{port}` |
| `<TRACE_ID>` | Trace IDs |
| `<SPAN_ID>` | Span IDs |

Sensitive fields are automatically redacted in JSONL context maps: `api_key`, `authorization`, `bearer`, `cookie`, `credential`, `password`, `private_key`, `secret`, `token`.

### Event sequence validation rules

E2E tests (`e2e_provider_scenarios.rs`) validate that stream events follow this ordering:

```
Start? → (TextDelta | ThinkingDelta | ToolCallStart | ToolCallDelta)* → Done | Error
```

Specific rules:
- `Start` event (if present) must be first — Bedrock is exempted (`require_start_event: false`)
- `Done` event must have a valid `StopReason` (`EndTurn`, `ToolUse`, `MaxTokens`, `StopSequence`)
- `Error` events terminate the stream; no events follow
- `ToolCallStart` must precede any `ToolCallDelta` for the same tool index
- Token usage (`input_tokens`, `output_tokens`) must be reported in the `Done` event

### Cross-provider parity record schema

Live parity tests (`e2e_cross_provider_parity.rs`) emit `ParityRecord` entries with:

| Field | Required | Description |
|-------|----------|-------------|
| `check` | yes | Parity check name |
| `provider` | yes | Provider canonical ID |
| `status` | yes | `pass`, `fail`, or `skip` |
| `event_count` | yes | Total stream events received |
| `sequence_valid` | yes | Whether event ordering is correct |
| `sequence` | yes | Array of event type names in order |
| `usage_total_tokens` | yes | Total tokens reported |
| `elapsed_ms` | yes | Wall-clock time |

### Drift-prevention snapshot tests

These tests in `tests/provider_metadata_comprehensive.rs` use hard-coded snapshots that **must be updated** when adding or modifying providers:

| Test | Update trigger |
|------|---------------|
| `canonical_id_snapshot_detects_additions_and_removals` | Adding/removing any canonical ID |
| `alias_mapping_snapshot_is_current` | Any change to alias arrays |
| `base_url_snapshot_for_key_providers` | Changing base URL for key providers |
| `vcr_fixture_coverage_for_core_providers` | Adding a new core provider |
| `gap_providers_have_setup_documentation` | Adding a new gap-class provider |
| `no_accidental_duplicate_routing_defaults` | Adding provider with same (api, base_url) pair |

### Quick verification commands

After adding or modifying a provider, run these in order:

```bash
# 1. Conformance (VCR playback)
cargo test --test provider_native_verify {provider}_conformance:: -- --nocapture

# 2. Factory dispatch
cargo test --test provider_factory -- --nocapture

# 3. Metadata invariants + drift snapshots
cargo test --test provider_metadata_comprehensive -- --nocapture

# 4. Contract tests (native adapters only)
cargo test --test provider_native_contract {provider}_contract:: -- --nocapture

# 5. E2E scenarios
cargo test --test e2e_provider_scenarios -- --nocapture

# 6. Quality gates
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

## Validation commands for doc and onboarding changes

Targeted checks (fast):

```bash
cargo test provider_factory -- --nocapture
cargo test provider_metadata_comprehensive -- --nocapture
cargo test --test provider_native_contract openrouter_contract:: -- --nocapture
cargo test --test provider_native_verify openrouter_conformance:: -- --nocapture
cargo test --test e2e_provider_scenarios e2e_openai_compatible_wave_presets -- --nocapture
```

Broader quality gates:

```bash
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Live parity lanes (gated, real APIs):

```bash
CI_E2E_TESTS=1 cargo test e2e_cross_provider_parity -- --nocapture
CI_E2E_TESTS=1 cargo test e2e_live_harness -- --nocapture
```

## CI artifact retention and replay triage workflow (bd-3uqg.9.4.3)

Provider changes are not complete unless CI failures are reproducible from retained artifacts.
Use this contract and replay flow for every provider-facing PR:

### Required artifact outputs

Source of truth: `docs/provider_e2e_artifact_contract.json`

Per-suite artifacts (required):
- `output.log`
- `result.json`
- `test-log.jsonl` (`pi.test.log.v2`)
- `artifact-index.jsonl` (`pi.test.artifact.v1`)

Per-run artifacts (required):
- `summary.json`
- `environment.json`
- `evidence_contract.json`
- `replay_bundle.json`
- `failure_digest.json` (for each failed suite)

### Validation commands

```bash
# Contract and retention checks
cargo test --test ci_artifact_retention -- --nocapture

# Replay-bundle shape and command validity
cargo test --test e2e_replay_bundles -- --nocapture
cargo test --test e2e_replay_bundle_validation -- --nocapture

# Generate a fresh CI-style artifact set
./scripts/e2e/run_all.sh --profile ci
```

### Triage and replay sequence

1. Start from `tests/e2e_results/<timestamp>/summary.json` and inspect `failed_names` / `failed_unit_names`.
2. Read `tests/e2e_results/<timestamp>/replay_bundle.json` and run `one_command_replay`.
3. For each failing suite, open `failed_suites[].digest_path` (`failure_digest.json`) and run:
   - `remediation_pointer.suite_replay_command`
   - `remediation_pointer.targeted_test_replay_command`
4. Record root cause class and remediation in the bead thread before merging.
5. If auth-related failures are involved, cross-check redaction tests:
   - `tests/e2e_artifact_retention_triage.rs::log_redacts_api_keys`
   - `tests/e2e_artifact_retention_triage.rs::log_redacts_authorization_headers`

Operator reference: `docs/ci-operator-runbook.md`.

## Onboarding anti-patterns, guardrails, and rollback protocol (bd-3uqg.9.4.4)

This section is mandatory reading before merging provider metadata, factory, or adapter changes.

### Anti-pattern catalog with prevention checks

| Anti-pattern | Concrete example | Prevention check (must pass) |
|---|---|---|
| Alias drift between docs and runtime | `providers.md` says alias `open-router`, but `provider_metadata.rs` alias list was not updated | `cargo test --test provider_metadata_comprehensive alias_mapping_snapshot_is_current -- --nocapture` |
| Partial verification coverage | New provider added with only `simple_text` cassette, no auth/error scenarios | `cargo test --test provider_native_verify <provider>_conformance:: -- --nocapture` plus required scenarios (`simple_text`, `tool_call_single`, `error_auth_401`) |
| Non-redacted diagnostics in logs/artifacts | `Authorization` header or API key appears in JSONL or cassette output | `cargo test --test ci_artifact_retention -- --nocapture` and redaction checks in `tests/e2e_artifact_retention_triage.rs` |
| Stale docs/runtime mapping | Setup JSON lists wrong `auth_env` or base URL after runtime change | `cargo test --test provider_native_contract docs_runtime -- --nocapture` |
| Missing CI replayability | Failure digests exist without deterministic replay commands | `cargo test --test e2e_replay_bundles -- --nocapture` and `cargo test --test e2e_replay_bundle_validation -- --nocapture` |
| Routing-default collision | New provider copied existing `(api, base_url)` pair unintentionally | `cargo test --test provider_metadata_comprehensive no_accidental_duplicate_routing_defaults -- --nocapture` |
| Provider matrix/doc drift after merge | Provider works in code but matrix/evidence rows are stale | Update docs + run `cargo test --test provider_native_contract docs_runtime -- --nocapture` before merge |

### Required pre-merge guardrails

Run all of the following before closing a provider bead:

```bash
# Metadata + routing drift guards
cargo test --test provider_metadata_comprehensive -- --nocapture
cargo test --test provider_factory -- --nocapture

# Docs/runtime consistency
cargo test --test provider_native_contract docs_runtime -- --nocapture

# CI artifact + replay guarantees
cargo test --test ci_artifact_retention -- --nocapture
cargo test --test e2e_replay_bundles -- --nocapture
cargo test --test e2e_replay_bundle_validation -- --nocapture

# Native adapter / provider-specific conformance (if applicable)
cargo test --test provider_native_verify <provider>_conformance:: -- --nocapture

# Global quality gates
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Pre-merge block rule:
- Any redaction failure, missing replay command, or docs/runtime mismatch is a merge blocker.

### Required post-merge monitoring hooks

Within the first CI cycle after merge:
1. Confirm retained artifacts exist (`summary.json`, `replay_bundle.json`, `failure_digest.json` for failures).
2. Verify `replay_bundle.one_command_replay` executes locally from the CI artifact set.
3. Check failure taxonomy and remediation pointers for newly failing suites.
4. Confirm no secret leakage via artifact-retention redaction checks.
5. Post a bead-thread note with pass/fail status and artifact paths.

### Rollback protocol (actionable thresholds + handoff)

Severity thresholds:

| Severity | Trigger | Required action window |
|---|---|---|
| `SEV-1` | Secret exposure or redaction break in CI artifacts | Immediate containment and rollback in the same response window |
| `SEV-2` | Provider route/auth break causing broad request failure with no safe workaround | Rollback decision within 30 minutes after confirmation |
| `SEV-3` | Scoped regression with a documented workaround | Fix-forward or rollback decision within 1 business day |

Rollback steps:
1. **Classify and contain**: assign severity and stop further merges for the affected provider scope.
2. **Preserve evidence**: archive failing `summary.json`, `replay_bundle.json`, and `failure_digest.json` paths in the bead thread.
3. **Execute rollback**:
   - Preferred: targeted `git revert <commit>` for the offending provider change.
   - Alternative: temporary provider disable/path reroute with explicit follow-up bead.
4. **Re-run gates**: re-run artifact/replay and provider verification commands to confirm recovery.
5. **Handoff**: post a final incident note containing:
   - root cause class,
   - rollback commit/change reference,
   - verification command output summary,
   - follow-up bead IDs for permanent fix work.

## Contributor checklist (new provider or major provider update)

### Phase 1: Metadata registration

1. Add or update canonical metadata entry in `../src/provider_metadata.rs`:
   - `canonical_id`: lowercase, hyphenated (e.g., `my-provider`).
   - `aliases`: any common alternative names.
   - `auth_env_keys`: primary env var first, fallbacks after (e.g., `&["MY_PROVIDER_API_KEY"]`).
   - `onboarding`: one of `BuiltInNative`, `OpenAICompatiblePreset`, `NativeAdapterRequired`.
   - `routing_defaults`: required for OAI-compatible; set `api`, `base_url`, `auth_header`, etc.
   - `test_obligations`: set all to `true` for production providers.

2. **Update drift-prevention snapshots** — these tests will fail until updated:
   - `canonical_id_snapshot_detects_additions_and_removals` in `tests/provider_metadata_comprehensive.rs` — add the new ID to the sorted `EXPECTED` array.
   - `alias_mapping_snapshot_is_current` — add any new aliases to `EXPECTED_ALIASES`.
   - `base_url_snapshot_for_key_providers` — add the base URL if this is a key/gap provider.

3. Ensure alias resolution + env key mapping are covered by existing invariant tests:
   - `all_canonical_ids_are_unique`, `no_alias_collides_with_canonical_id` — automatic.
   - `auth_env_keys_are_screaming_snake_case` — automatic.

### Phase 2: Factory wiring and tests

4. Wire route and provider factory behavior in `../src/providers/mod.rs`.

5. Add/update provider-specific tests:
   - **Factory selection**: `tests/provider_factory.rs` (wave preset tests).
   - **Metadata invariants**: `tests/provider_metadata_comprehensive.rs` (automatic for structural tests).
   - **Streaming contract**: `tests/provider_streaming.rs` or `tests/provider_native_contract.rs`.

6. Add VCR fixtures in `tests/fixtures/vcr/`:
   - Minimum: `verify_<provider>_simple_text.json`
   - Recommended: `verify_<provider>_error_auth_401.json`, `verify_<provider>_tool_call_single.json`
   - If core provider, add to `vcr_fixture_coverage_for_core_providers` in `tests/provider_metadata_comprehensive.rs`.

### Phase 3: Documentation

7. Update provider documentation:
   - `providers.md` — matrix/status row.
   - This playbook — config example and troubleshooting entry.
   - For gap providers (groq, cerebras, openrouter, moonshotai, alibaba class): create a dedicated setup doc `docs/provider-<name>-setup.json` following schema `pi.provider_setup_guide.v1`.
   - Update `docs/provider-config-examples.json` with env vars, CLI examples, and caveats.
   - Update `docs/provider-migration-guide.md` if the provider has non-standard behavior.
   - Update `docs/provider-auth-troubleshooting.md` with auth failure modes.

8. **Verify docs/runtime consistency** — these tests catch doc drift:
   - `docs_runtime_consistency::setup_doc_auth_env_matches_runtime` in `tests/provider_native_contract.rs`.
   - `docs_runtime_consistency::setup_doc_base_url_matches_runtime_default`.
   - `docs_runtime_consistency::config_examples_env_vars_match_runtime`.

### Phase 4: Quality gates

9. Run quality gates before closing:

```bash
# Drift-prevention (must pass — will catch snapshot mismatches)
CARGO_TARGET_DIR=target/<agent> cargo test --test provider_metadata_comprehensive -- --nocapture

# Factory + routing
CARGO_TARGET_DIR=target/<agent> cargo test --test provider_factory -- --nocapture

# Docs/runtime consistency
CARGO_TARGET_DIR=target/<agent> cargo test --test provider_native_contract docs_runtime -- --nocapture

# Full lint/format
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

10. Attach evidence links (test output + artifact paths) before closing provider beads.

## Quality gate reference

### Drift-prevention tests (bd-3uqg.11.10.4)

These tests in `tests/provider_metadata_comprehensive.rs` use hard-coded snapshots to force intentional acknowledgment of metadata changes:

| Test | What it catches | Update when |
|---|---|---|
| `canonical_id_snapshot_detects_additions_and_removals` | Provider added/removed | Adding or removing any canonical_id |
| `alias_mapping_snapshot_is_current` | Alias added/removed/reassigned | Any change to alias arrays |
| `base_url_snapshot_for_key_providers` | Silent endpoint URL change | Changing base_url for key providers |
| `vcr_fixture_coverage_for_core_providers` | Core provider missing VCR fixtures | Adding a new core provider |
| `gap_providers_have_setup_documentation` | Gap provider missing setup doc | Adding a new gap-class provider |
| `no_accidental_duplicate_routing_defaults` | Copy-paste routing error | Adding provider with same (api, base_url) pair |

### Docs/runtime consistency tests (bd-3uqg.11.12.5)

These tests in `tests/provider_native_contract.rs` validate documentation cannot silently diverge from runtime:

| Test | What it catches |
|---|---|
| `setup_docs_exist_and_parse_as_valid_json` | Broken/missing JSON setup docs |
| `setup_doc_provider_ids_match_metadata` | Doc provider_id vs metadata mismatch |
| `setup_doc_auth_env_matches_runtime` | Doc auth_env vs runtime env keys |
| `setup_doc_base_url_matches_runtime_default` | Doc base_url vs runtime default |
| `config_examples_env_vars_match_runtime` | Config examples env vars vs runtime |
| `migration_guide_references_correct_env_vars` | Migration guide env var references |

Provider crosswalk freshness is also checked by
`python3 scripts/check_provider_discrepancy_ledger.py --compact`. Run it after
editing `src/provider_metadata.rs`, `docs/providers.md`, or
`docs/provider-auth-troubleshooting.md`; it compares canonical IDs, aliases, and
auth env keys against the checked crosswalk.

## Provider-specific documentation references

| Provider family | Setup doc | Config examples | Migration notes |
|---|---|---|---|
| Groq | `docs/provider-groq-setup.json` | `docs/provider-config-examples.json` | `docs/provider-migration-guide.md` |
| Cerebras | `docs/provider-cerebras-setup.json` | `docs/provider-config-examples.json` | `docs/provider-migration-guide.md` |
| OpenRouter | `docs/provider-openrouter-setup.json` | `docs/provider-config-examples.json` | `docs/provider-migration-guide.md` |
| Kimi (moonshotai) | `docs/provider-kimi-setup.json` | `docs/provider-config-examples.json` | `docs/provider-migration-guide.md` |
| Qwen (alibaba) | `docs/provider-qwen-setup.json` | `docs/provider-config-examples.json` | `docs/provider-migration-guide.md` |
| Auth troubleshooting (all) | `docs/provider-auth-troubleshooting.md` checked by `python3 scripts/check_provider_discrepancy_ledger.py --compact` | — | — |
| Longtail evidence | `docs/provider-longtail-evidence.md` | — | — |

## Current evidence-backed limits

The canonical matrix/evidence table in `providers.md` is under active parallel edits. Treat that file as the source for final matrix status, and this playbook as the operational implementation guide for onboarding and troubleshooting.
