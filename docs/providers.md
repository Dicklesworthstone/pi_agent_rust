# Providers

This document is the canonical in-repo provider baseline for `pi_agent_rust`.
It summarizes provider IDs, aliases, API families, auth behavior, and current implementation mode.

Snapshot basis:
- `src/models.rs` (`built_in_models`, `ad_hoc_provider_defaults`)
- `src/auth.rs` (`env_keys_for_provider`)
- `src/providers/mod.rs` (`create_provider`, API fallback routing)
- `src/provider_metadata.rs` (`PROVIDER_METADATA`, aliases, onboarding modes)
- `src/providers/*.rs` native implementations
- Original snapshot timestamp: 2026-02-10
- Last source cross-check: 2026-08-03

Provider-count rule: Pi has 11 native provider implementation modules. Those modules are `anthropic`, `openai`, `openai_responses`, `gemini`, `cohere`, `azure`, `bedrock`, `vertex`, `copilot`, `gitlab`, and `cursor`. User-visible canonical IDs, aliases, OpenAI-compatible presets, VCR coverage families, and extension-provided providers are separate counts. The factory module `mod.rs` and shared live-catalog helper `model_fetch.rs` are support modules, not provider implementations, and are excluded.

This rule is guarded by `tests/traceability_staleness.rs::native_provider_module_inventory_matches_provider_docs`, which compares this list against the live `src/providers/*.rs` inventory.

| Native module | Primary user-visible surface | Notes |
|---------------|------------------------------|-------|
| `anthropic` | `anthropic` | Anthropic Messages API. |
| `openai` | `openai` and OpenAI-compatible presets | Chat Completions-compatible runtime path. |
| `openai_responses` | `openai`, `openai-codex` | Responses/Codex Responses request and streaming path. |
| `gemini` | `google`, `gemini` | Google Generative AI path. |
| `cohere` | `cohere` | Cohere chat path. |
| `azure` | `azure-openai`, `azure`, `azure_openai`, `azure-cognitive-services`, `azure-openai-responses` | Azure OpenAI deployment path. |
| `bedrock` | `amazon-bedrock`, `bedrock` | Amazon Bedrock Converse path. |
| `vertex` | `google-vertex`, `vertexai` | Vertex AI Gemini/Anthropic publisher path. |
| `copilot` | `github-copilot`, `copilot` | GitHub Copilot chat/completions path. |
| `gitlab` | `gitlab`, `gitlab-duo` | GitLab Duo chat path. |
| `cursor` | `cursor` | Cursor Connect RPC path. |

Extension-provided providers route through the extension provider bridge in `src/providers/mod.rs`; they are intentionally excluded from the native module count because they are discovered at runtime.

## Implementation Modes

| Mode | Meaning |
|------|---------|
| `native-implemented` | Provider has a direct runtime path in `create_provider` and is dispatchable now. |
| `native-partial` | Native module exists, but factory wiring or required config path is not fully integrated. |
| `oai-compatible-preset` | Provider resolves through OpenAI-compatible adapter (`openai-completions`) with preset base/auth defaults. |
| `alias-only` | Provider ID is a documented synonym of a canonical ID; no distinct runtime implementation. |
| `missing` | Provider ID is recognized in enums/auth mappings but has no usable runtime dispatch path yet. |

### Machine-Readable Classification (`bd-3uqg.1.4`)

Historical planning artifact: `docs/provider-implementation-modes.json`

That JSON freezes the 2026-02-13 planning decision and is not current runtime
authority. For the live tree, use `src/provider_metadata.rs` for registration
and `src/providers/mod.rs` plus factory tests for dispatch. Its historical mode
vocabulary was:

| Mode | Planning Meaning |
|------|------------------|
| `native-adapter-required` | Requires dedicated runtime adapter path (protocol/auth/tool semantics not safely covered by generic OAI routing). |
| `oai-compatible-preset` | Can route through OpenAI-compatible adapter with provider-specific base/auth presets. |
| `gateway-wrapper-routing` | Acts as gateway/meta-router/alias-routing surface; prioritize routing-policy and diagnostics guarantees. |
| `deferred` | Explicitly not in current implementation wave; retained for planning completeness. |

Historical snapshot coverage (`docs/provider-implementation-modes.json`):
- 93 upstream union IDs classified (no gaps)
- 6 supplemental Pi alias IDs classified
- 99 total entries with explicit profile, rationale, and runtime status
- 20 high-risk providers carry explicit prerequisite beads + required diagnostic artifacts

## Verification Evidence Legend

- Metadata and alias/routing lock: [`tests/provider_metadata_comprehensive.rs`](../tests/provider_metadata_comprehensive.rs)
- Factory and adapter selection lock: [`tests/provider_factory.rs`](../tests/provider_factory.rs)
- Native provider request-shape lock: [`tests/provider_backward_lock.rs`](../tests/provider_backward_lock.rs)
- Provider streaming contract suites: [`tests/provider_streaming.rs`](../tests/provider_streaming.rs)
- Live parity smoke lane: [`tests/e2e_cross_provider_parity.rs`](../tests/e2e_cross_provider_parity.rs)
- Live provider integration lane: [`tests/e2e_live.rs`](../tests/e2e_live.rs)
- Discrepancy-ledger freshness gate: `python3 scripts/check_provider_discrepancy_ledger.py --compact`

## Wave A Parity Verification (`bd-3uqg.4.4`)

Unit + request-shape verification for all currently tracked Wave A OpenAI-compatible preset IDs:
`groq`, `deepinfra`, `cerebras`, `openrouter`, `mistral`, `moonshotai`, `dashscope`, `deepseek`,
`fireworks`, `togetherai`, `perplexity`, `xai`, plus migration alias `fireworks-ai`.

Verification artifacts:
- Default/factory lock: [`tests/provider_factory.rs`](../tests/provider_factory.rs) (`wave_a_presets_resolve_openai_compat_defaults_and_factory_route`)
- Streaming path/auth lock: [`tests/provider_factory.rs`](../tests/provider_factory.rs) (`wave_a_openai_compat_streams_use_chat_completions_path_and_bearer_auth`)
- Alias migration lock: [`tests/provider_factory.rs`](../tests/provider_factory.rs) (`fireworks_ai_alias_migration_matches_fireworks_canonical_defaults`)

Provider-by-provider status (local verification via `cargo test --test provider_factory -- --nocapture`):

| Provider ID | Defaults + factory route lock | Streaming path/auth lock | Status |
|-------------|-------------------------------|--------------------------|--------|
| `groq` | yes | yes | pass |
| `deepinfra` | yes | yes | pass |
| `cerebras` | yes | yes | pass |
| `openrouter` | yes | yes | pass |
| `mistral` | yes | yes | pass |
| `moonshotai` | yes | yes | pass |
| `dashscope` | yes | yes | pass |
| `deepseek` | yes | yes | pass |
| `fireworks` | yes | yes | pass |
| `togetherai` | yes | yes | pass |
| `perplexity` | yes | yes | pass |
| `xai` | yes | yes | pass |
| `fireworks-ai` (alias) | yes | yes | pass |

Migration mapping decisions:
- `fireworks-ai` remains accepted as an alias of canonical `fireworks`.
- Route and auth behavior are parity-locked between `fireworks` and `fireworks-ai`.
- No compatibility shim layer is introduced; canonical configs should use `fireworks` going forward.

## Wave B1 Onboarding Verification (`bd-3uqg.5.2`)

Batch B1 provider IDs integrated and lock-tested:
`alibaba-cn`, `kimi-for-coding`, `minimax`, `minimax-cn`, `minimax-coding-plan`, `minimax-cn-coding-plan`.

Verification artifacts:
- Metadata + factory route lock: [`tests/provider_factory.rs`](../tests/provider_factory.rs) (`wave_b1_presets_resolve_metadata_defaults_and_factory_route`)
- OpenAI-compatible stream path/auth lock (`alibaba-cn`): [`tests/provider_factory.rs`](../tests/provider_factory.rs) (`wave_b1_alibaba_cn_openai_compat_streams_use_chat_completions_path_and_bearer_auth`)
- Anthropic-compatible direct-key stream path/auth lock (`kimi-for-coding`, `minimax*`): [`tests/provider_factory.rs`](../tests/provider_factory.rs) (`wave_b1_anthropic_compat_streams_use_messages_path_and_x_api_key`); Kimi device/external-token bearer behavior is locked in [`src/providers/anthropic.rs`](../src/providers/anthropic.rs) (`test_stream_kimi_oauth_uses_bearer_and_kimi_headers`)
- Family coherence lock (`moonshot`/`kimi` alias vs `kimi-for-coding`, `alibaba` vs `alibaba-cn`): [`tests/provider_factory.rs`](../tests/provider_factory.rs) (`wave_b1_family_coherence_with_existing_moonshot_and_alibaba_mappings`)
- Representative smoke/e2e artifacts (offline VCR harness): [`tests/provider_native_verify.rs`](../tests/provider_native_verify.rs) (`wave_b1_smoke::b1_alibaba_cn_*`, `wave_b1_smoke::b1_kimi_for_coding_*`, `wave_b1_smoke::b1_minimax_*`) with fixtures under [`tests/fixtures/vcr/verify_alibaba-cn_*.json`](../tests/fixtures/vcr/verify_alibaba-cn_simple_text.json), [`tests/fixtures/vcr/verify_kimi-for-coding_*.json`](../tests/fixtures/vcr/verify_kimi-for-coding_simple_text.json), and [`tests/fixtures/vcr/verify_minimax_*.json`](../tests/fixtures/vcr/verify_minimax_simple_text.json)

Provider-by-provider status (local verification via `cargo test --test provider_factory -- --nocapture`):

| Provider ID | API family | Route lock | Stream/auth lock | Status |
|-------------|------------|------------|------------------|--------|
| `alibaba-cn` | `openai-completions` | yes | yes | pass |
| `kimi-for-coding` | `anthropic-messages` | yes | yes | pass |
| `minimax` | `anthropic-messages` | yes | yes | pass |
| `minimax-cn` | `anthropic-messages` | yes | yes | pass |
| `minimax-coding-plan` | `anthropic-messages` | yes | yes | pass |
| `minimax-cn-coding-plan` | `anthropic-messages` | yes | yes | pass |

Representative smoke/e2e verification run:
- `cargo test --test provider_native_verify b1_ -- --nocapture`
- Passed: `b1_alibaba_cn_{simple_text,tool_call_single,error_auth_401}`,
  `b1_kimi_for_coding_{simple_text,tool_call_single,error_auth_401}`,
  `b1_minimax_{simple_text,tool_call_single,error_auth_401}`.

Canonical mapping decisions:
- `kimi` remains an alias of canonical `moonshotai`.
- `kimi-for-coding` is a distinct canonical ID and does not alias to `moonshotai`.
- `alibaba-cn` is distinct from `alibaba`/`dashscope`/`qwen` and uses CN DashScope routing defaults.
- `minimax-cn`, `minimax-coding-plan`, and `minimax-cn-coding-plan` inherit representative smoke coverage via
  shared family behavior plus explicit route/auth lock tests.

## Wave B2 Onboarding Verification (`bd-3uqg.5.1`)

Batch B2 provider IDs integrated and lock-tested:
`modelscope`, `moonshotai-cn`, `nebius`, `ovhcloud`, `scaleway`.

Verification artifacts:
- Metadata + factory route lock: [`tests/provider_factory.rs`](../tests/provider_factory.rs) (`wave_b2_presets_resolve_metadata_defaults_and_factory_route`)
- OpenAI-compatible stream path/auth lock: [`tests/provider_factory.rs`](../tests/provider_factory.rs) (`wave_b2_openai_compat_streams_use_chat_completions_path_and_bearer_auth`)
- Family coherence lock (`moonshotai`/`moonshot` aliases vs `moonshotai-cn`): [`tests/provider_factory.rs`](../tests/provider_factory.rs) (`wave_b2_moonshot_cn_and_global_moonshot_mapping_are_distinct`)
- Representative smoke/e2e artifacts (offline VCR harness): [`tests/provider_native_verify.rs`](../tests/provider_native_verify.rs) (`wave_b2_smoke::b2_modelscope_*`, `wave_b2_smoke::b2_moonshotai_cn_*`, `wave_b2_smoke::b2_nebius_*`, `wave_b2_smoke::b2_ovhcloud_*`, `wave_b2_smoke::b2_scaleway_*`) with fixtures under [`tests/fixtures/vcr/verify_modelscope_*.json`](../tests/fixtures/vcr/verify_modelscope_simple_text.json), [`tests/fixtures/vcr/verify_moonshotai-cn_*.json`](../tests/fixtures/vcr/verify_moonshotai-cn_simple_text.json), [`tests/fixtures/vcr/verify_nebius_*.json`](../tests/fixtures/vcr/verify_nebius_simple_text.json), [`tests/fixtures/vcr/verify_ovhcloud_*.json`](../tests/fixtures/vcr/verify_ovhcloud_simple_text.json), and [`tests/fixtures/vcr/verify_scaleway_*.json`](../tests/fixtures/vcr/verify_scaleway_simple_text.json)

Provider-by-provider status (local verification via `cargo test --test provider_factory -- --nocapture`):

| Provider ID | API family | Route lock | Stream/auth lock | Status |
|-------------|------------|------------|------------------|--------|
| `modelscope` | `openai-completions` | yes | yes | pass |
| `moonshotai-cn` | `openai-completions` | yes | yes | pass |
| `nebius` | `openai-completions` | yes | yes | pass |
| `ovhcloud` | `openai-completions` | yes | yes | pass |
| `scaleway` | `openai-completions` | yes | yes | pass |

Representative smoke/e2e verification run:
- `cargo test --test provider_native_verify b2_ -- --nocapture`
- Passed: `b2_modelscope_{simple_text,tool_call_single,error_auth_401}`,
  `b2_moonshotai_cn_{simple_text,tool_call_single,error_auth_401}`,
  `b2_nebius_{simple_text,tool_call_single,error_auth_401}`,
  `b2_ovhcloud_{simple_text,tool_call_single,error_auth_401}`,
  `b2_scaleway_{simple_text,tool_call_single,error_auth_401}`.

Canonical mapping decisions:
- `modelscope`, `nebius`, `ovhcloud`, and `scaleway` are canonical OpenAI-compatible preset IDs.
- `moonshotai-cn` is a distinct canonical regional ID and does not alias to `moonshotai`.
- `moonshotai` and `moonshotai-cn` intentionally share `MOONSHOT_API_KEY` while retaining distinct base URLs.

## Wave B3 Onboarding Verification (`bd-3uqg.5.3`)

Batch B3 provider IDs integrated and lock-tested:
`siliconflow`, `siliconflow-cn`, `upstage`, `venice`, `zai`, `zai-coding-plan`, `zhipuai`, `zhipuai-coding-plan`.

Verification artifacts:
- Metadata + ad-hoc default route lock: [`src/provider_metadata.rs`](../src/provider_metadata.rs) (`batch_b3_*` tests), [`src/models.rs`](../src/models.rs) (`ad_hoc_batch_b3_*`)
- Factory route lock: [`tests/provider_factory.rs`](../tests/provider_factory.rs) (`wave_b3_presets_resolve_metadata_defaults_and_factory_route`)
- OpenAI-compatible stream path/auth lock: [`tests/provider_factory.rs`](../tests/provider_factory.rs) (`wave_b3_openai_compat_streams_use_chat_completions_path_and_bearer_auth`)
- Family/coding-plan distinctness lock: [`tests/provider_factory.rs`](../tests/provider_factory.rs) (`wave_b3_family_and_coding_plan_variants_are_distinct`)
- Representative smoke/e2e artifacts (offline VCR harness): [`tests/provider_native_verify.rs`](../tests/provider_native_verify.rs) (`wave_b3_smoke::b3_*`) with fixtures under:
  [`tests/fixtures/vcr/verify_siliconflow_*.json`](../tests/fixtures/vcr/verify_siliconflow_simple_text.json),
  [`tests/fixtures/vcr/verify_siliconflow-cn_*.json`](../tests/fixtures/vcr/verify_siliconflow-cn_simple_text.json),
  [`tests/fixtures/vcr/verify_upstage_*.json`](../tests/fixtures/vcr/verify_upstage_simple_text.json),
  [`tests/fixtures/vcr/verify_venice_*.json`](../tests/fixtures/vcr/verify_venice_simple_text.json),
  [`tests/fixtures/vcr/verify_zai_*.json`](../tests/fixtures/vcr/verify_zai_simple_text.json),
  [`tests/fixtures/vcr/verify_zai-coding-plan_*.json`](../tests/fixtures/vcr/verify_zai-coding-plan_simple_text.json),
  [`tests/fixtures/vcr/verify_zhipuai_*.json`](../tests/fixtures/vcr/verify_zhipuai_simple_text.json),
  [`tests/fixtures/vcr/verify_zhipuai-coding-plan_*.json`](../tests/fixtures/vcr/verify_zhipuai-coding-plan_simple_text.json)

Provider-by-provider status (local verification via `cargo test --test provider_factory -- --nocapture`):

| Provider ID | API family | Route lock | Stream/auth lock | Status |
|-------------|------------|------------|------------------|--------|
| `siliconflow` | `openai-completions` | yes | yes | pass |
| `siliconflow-cn` | `openai-completions` | yes | yes | pass |
| `upstage` | `openai-completions` | yes | yes | pass |
| `venice` | `openai-completions` | yes | yes | pass |
| `zai` | `openai-completions` | yes | yes | pass |
| `zai-coding-plan` | `openai-completions` | yes | yes | pass |
| `zhipuai` | `openai-completions` | yes | yes | pass |
| `zhipuai-coding-plan` | `openai-completions` | yes | yes | pass |

Representative smoke/e2e verification run:
- `cargo test --test provider_native_verify b3_ -- --nocapture`
- Passed: `b3_siliconflow_{simple_text,tool_call_single,error_auth_401}`,
  `b3_siliconflow_cn_{simple_text,tool_call_single,error_auth_401}`,
  `b3_upstage_{simple_text,tool_call_single,error_auth_401}`,
  `b3_venice_{simple_text,tool_call_single,error_auth_401}`,
  `b3_zai_{simple_text,tool_call_single,error_auth_401}`,
  `b3_zai_coding_{simple_text,tool_call_single,error_auth_401}`,
  `b3_zhipuai_{simple_text,tool_call_single,error_auth_401}`,
  `b3_zhipuai_coding_{simple_text,tool_call_single,error_auth_401}`.

Canonical mapping decisions:
- `siliconflow` and `siliconflow-cn` are distinct canonical regional IDs with separate auth env keys (`SILICONFLOW_API_KEY`, `SILICONFLOW_CN_API_KEY`).
- `zai` and `zai-coding-plan` are distinct canonical IDs that intentionally share `ZHIPU_API_KEY` while retaining distinct base URLs.
- `zhipuai` and `zhipuai-coding-plan` are distinct canonical IDs that intentionally share `ZHIPU_API_KEY` while retaining distinct base URLs.

## Wave C Staging Snapshot (`bd-3uqg.6`)

Source of truth for provisional Wave C defaults:
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

Wave C execution status:

| Provider ID | API family target | Default base URL | Auth env | Current tracking status |
|-------------|-------------------|------------------|----------|-------------------------|
| `baseten` | `openai-completions` | `https://inference.baseten.co/v1` | `BASETEN_API_KEY` | Wave C preset candidate (`bd-3uqg.6.1`) |
| `llama` | `openai-completions` | `https://api.llama.com/compat/v1/` | `LLAMA_API_KEY` | Wave C preset candidate (`bd-3uqg.6.2`) |
| `lmstudio` | `openai-completions` | `http://127.0.0.1:1234/v1` | `LMSTUDIO_API_KEY` | Wave C preset candidate (`bd-3uqg.6.2`) |
| `ollama-cloud` | `openai-completions` | `https://ollama.com/v1` | `OLLAMA_API_KEY` | Wave C preset candidate (`bd-3uqg.6.2`) |
| `opencode` | `openai-completions` | `https://opencode.ai/zen/v1` | `OPENCODE_API_KEY` | Special routing pending (`bd-3uqg.3.9`, `bd-3uqg.6.3`) |
| `vercel` | gateway-wrapper (`@ai-sdk/gateway`) | no static API URL in `models.dev` | `AI_GATEWAY_API_KEY` | Classification/routing pending (`bd-3uqg.3.9`, `bd-3uqg.6.1`) |
| `zenmux` | `anthropic-messages` target (gateway) | `https://zenmux.ai/api/anthropic/v1` | `ZENMUX_API_KEY` | Special routing pending (`bd-3uqg.3.9`, `bd-3uqg.6.3`) |

## Canonical Provider Matrix (Current Baseline + Evidence Links)

| Canonical ID | Aliases | Capability flags | API family | Base URL template | Auth mode | Mode | Runtime status | Verification evidence (unit + e2e) |
|--------------|---------|------------------|------------|-------------------|-----------|------|----------------|------------------------------------|
| `anthropic` | - | text + image + thinking + tool-calls | `anthropic-messages` | `https://api.anthropic.com/v1/messages` | `x-api-key` (`ANTHROPIC_API_KEY`) or `auth.json` OAuth/API key | `native-implemented` | Implemented and dispatchable | [unit](../tests/provider_streaming/anthropic.rs), [contract](../tests/provider_backward_lock.rs), [e2e](../tests/e2e_provider_streaming.rs), [cassette](../tests/fixtures/vcr/anthropic_simple_text.json) |
| `openai` | - | text + image + reasoning + tool-calls | `openai-responses` (default), `openai-completions` (compat) | `https://api.openai.com/v1` (normalized to `/responses` or `/chat/completions`) | `Authorization: Bearer` (`OPENAI_API_KEY`) | `native-implemented` | Implemented and dispatchable | [unit](../tests/provider_streaming/openai.rs), [responses](../tests/provider_streaming/openai_responses.rs), [contract](../tests/provider_backward_lock.rs), [e2e](../tests/e2e_cross_provider_parity.rs), [cassette](../tests/fixtures/vcr/openai_simple_text.json) |
| `openai-codex` | `codex`, `chatgpt-codex` | text + reasoning | `openai-codex-responses` | `https://chatgpt.com/backend-api/codex/responses` | ChatGPT/Codex OAuth via `/login openai-codex` | `native-adapter-required` | Dispatchable through the Codex Responses path | [responses](../src/providers/openai_responses.rs), [models](../src/models.rs), [metadata](../tests/provider_metadata_comprehensive.rs) |
| `google` | `gemini` | text + image + reasoning + tool-calls | `google-generative-ai` | `https://generativelanguage.googleapis.com/v1beta` | query key (`GOOGLE_API_KEY`, fallback `GEMINI_API_KEY`) | `native-implemented` | Implemented and dispatchable | [unit](../tests/provider_streaming/gemini.rs), [contract](../tests/provider_backward_lock.rs), [e2e](../tests/e2e_cross_provider_parity.rs), [cassette](../tests/fixtures/vcr/gemini_simple_text.json) |
| `google-gemini-cli` | `gemini-cli` | text + image + reasoning | `google-gemini-cli` | Project-scoped Code Assist endpoint | Google OAuth via `/login google-gemini-cli` plus project ID | `native-adapter-required` | Dispatchable through the Gemini CLI path | [gemini](../src/providers/gemini.rs), [models](../src/models.rs), [metadata](../tests/provider_metadata_comprehensive.rs) |
| `google-antigravity` | `antigravity` | text + image + reasoning | `google-gemini-cli` | Project-scoped Antigravity endpoint | Google OAuth via `/login google-antigravity` plus project ID | `native-adapter-required` | Dispatchable through the Gemini CLI path with Antigravity routing | [gemini](../src/providers/gemini.rs), [models](../src/models.rs), [metadata](../tests/provider_metadata_comprehensive.rs) |
| `google-vertex` | `vertexai` | text + image + reasoning + tool-calls | `google-vertex` | `https://{region}-aiplatform.googleapis.com/v1/projects/{project}/locations/{region}/publishers/{publisher}/models/{model}` | `Authorization: Bearer` (`GOOGLE_CLOUD_API_KEY`, alt `VERTEX_API_KEY`) | `native-implemented` | Implemented and dispatchable; supports Google (Gemini) and Anthropic publishers | [unit](../src/providers/vertex.rs), [factory](../src/providers/mod.rs), [metadata](../tests/provider_metadata_comprehensive.rs) |
| `cohere` | - | text + tool-calls | `cohere-chat` | `https://api.cohere.com/v2` (normalized to `/chat`) | `Authorization: Bearer` (`COHERE_API_KEY`) | `native-implemented` | Implemented and dispatchable | [unit](../tests/provider_streaming/cohere.rs), [contract](../tests/provider_backward_lock.rs), [cassette](../tests/fixtures/vcr/verify_cohere_simple_text.json), e2e expansion tracked in `bd-3uqg.8.4` |
| `azure-openai` | `azure`, `azure_openai`, `azure-cognitive-services`, `azure-openai-responses` | text + tool-calls | Azure chat/completions path | `https://{resource}.openai.azure.com/openai/deployments/{deployment}/chat/completions?api-version={version}` or `https://{resource}.cognitiveservices.azure.com/openai/deployments/{deployment}/chat/completions?api-version={version}` | `api-key` header (`AZURE_OPENAI_API_KEY`) | `native-implemented` | Dispatchable through provider factory with deterministic resource/deployment/api-version resolution from env + model/base_url | [unit](../tests/provider_streaming/azure.rs), [contract](../tests/provider_backward_lock.rs), [e2e](../tests/e2e_live.rs), [cassette](../tests/fixtures/vcr/azure_simple_text.json) |
| `groq` | - | text (+ OAI-compatible tools) | `openai-completions` | `https://api.groq.com/openai/v1` | `Authorization: Bearer` (`GROQ_API_KEY`) | `oai-compatible-preset` | Dispatchable through OpenAI-compatible fallback | [metadata](../tests/provider_metadata_comprehensive.rs), [factory](../tests/provider_factory.rs), e2e expansion tracked in `bd-3uqg.8.4` |
| `deepinfra` | - | text (+ OAI-compatible tools) | `openai-completions` | `https://api.deepinfra.com/v1/openai` | `Authorization: Bearer` (`DEEPINFRA_API_KEY`) | `oai-compatible-preset` | Dispatchable through OpenAI-compatible fallback | [metadata](../tests/provider_metadata_comprehensive.rs), [factory](../tests/provider_factory.rs), e2e expansion tracked in `bd-3uqg.8.4` |
| `cerebras` | - | text (+ OAI-compatible tools) | `openai-completions` | `https://api.cerebras.ai/v1` | `Authorization: Bearer` (`CEREBRAS_API_KEY`) | `oai-compatible-preset` | Dispatchable through OpenAI-compatible fallback | [metadata](../tests/provider_metadata_comprehensive.rs), [factory](../tests/provider_factory.rs), e2e expansion tracked in `bd-3uqg.8.4` |
| `openrouter` | `open-router` | text (+ OAI-compatible tools) | `openai-completions` | `https://openrouter.ai/api/v1` | `Authorization: Bearer` (`OPENROUTER_API_KEY`) | `oai-compatible-preset` | Dispatchable through OpenAI-compatible fallback; model/provider aliases normalize to canonical IDs | [metadata](../tests/provider_metadata_comprehensive.rs), [factory](../tests/provider_factory.rs), [contract](../tests/provider_native_contract.rs), [e2e](../tests/e2e_cross_provider_parity.rs), [scenario-e2e](../tests/e2e_provider_scenarios.rs) |
| `mistral` | - | text (+ OAI-compatible tools) | `openai-completions` | `https://api.mistral.ai/v1` | `Authorization: Bearer` (`MISTRAL_API_KEY`) | `oai-compatible-preset` | Dispatchable through OpenAI-compatible fallback | [metadata](../tests/provider_metadata_comprehensive.rs), [factory](../tests/provider_factory.rs), e2e expansion tracked in `bd-3uqg.8.4` |
| `moonshotai` | `moonshot`, `kimi` | text (+ OAI-compatible tools) | `openai-completions` | `https://api.moonshot.ai/v1` | `Authorization: Bearer` (`MOONSHOT_API_KEY`, fallback `KIMI_API_KEY`) | `oai-compatible-preset` (`moonshot`,`kimi` are `alias-only`) | Dispatchable through OpenAI-compatible fallback | [metadata](../tests/provider_metadata_comprehensive.rs), [factory](../tests/provider_factory.rs), [alias-roundtrip](../tests/provider_factory.rs), e2e expansion tracked in `bd-3uqg.8.4` |
| `moonshotai-cn` | - | text (+ OAI-compatible tools) | `openai-completions` | `https://api.moonshot.cn/v1` | `Authorization: Bearer` (`MOONSHOT_API_KEY`) | `oai-compatible-preset` | Dispatchable through OpenAI-compatible fallback | [metadata](../tests/provider_metadata_comprehensive.rs), [factory](../tests/provider_factory.rs), [native-verify harness](../tests/provider_native_verify.rs), [cassette](../tests/fixtures/vcr/verify_moonshotai-cn_simple_text.json) |
| `kimi-for-coding` | - | text + image (Anthropic-compatible) | `anthropic-messages` | `https://api.kimi.com/coding/v1/messages` | `x-api-key` for `sk-*` direct keys; `Authorization: Bearer` plus Kimi device headers for `/login` and external Kimi tokens (`KIMI_API_KEY` is the direct-env source) | `oai-compatible-preset` (preset fallback) | Dispatchable through Anthropic API fallback route | [metadata](../tests/provider_metadata_comprehensive.rs), [factory](../tests/provider_factory.rs), [bearer/direct-key unit tests](../src/providers/anthropic.rs), [native-verify harness](../tests/provider_native_verify.rs), [cassette](../tests/fixtures/vcr/verify_kimi-for-coding_simple_text.json) |
| `dashscope` | `alibaba`, `qwen` | text (+ OAI-compatible tools) | `openai-completions` | `https://dashscope-intl.aliyuncs.com/compatible-mode/v1` | `Authorization: Bearer` (`DASHSCOPE_API_KEY`, fallback `QWEN_API_KEY`) | `oai-compatible-preset` (`alibaba`,`qwen` are `alias-only`) | Dispatchable through OpenAI-compatible fallback | [metadata](../tests/provider_metadata_comprehensive.rs), [factory](../tests/provider_factory.rs), e2e expansion tracked in `bd-3uqg.8.4` |
| `alibaba-cn` | - | text (+ OAI-compatible tools) | `openai-completions` | `https://dashscope.aliyuncs.com/compatible-mode/v1` | `Authorization: Bearer` (`DASHSCOPE_API_KEY`) | `oai-compatible-preset` | Dispatchable through OpenAI-compatible fallback | [metadata](../tests/provider_metadata_comprehensive.rs), [factory](../tests/provider_factory.rs), [native-verify harness](../tests/provider_native_verify.rs), [cassette](../tests/fixtures/vcr/verify_alibaba-cn_simple_text.json) |
| `modelscope` | - | text (+ OAI-compatible tools) | `openai-completions` | `https://api-inference.modelscope.cn/v1` | `Authorization: Bearer` (`MODELSCOPE_API_KEY`) | `oai-compatible-preset` | Dispatchable through OpenAI-compatible fallback | [metadata](../tests/provider_metadata_comprehensive.rs), [factory](../tests/provider_factory.rs), [native-verify harness](../tests/provider_native_verify.rs), [cassette](../tests/fixtures/vcr/verify_modelscope_simple_text.json) |
| `nebius` | - | text (+ OAI-compatible tools) | `openai-completions` | `https://api.tokenfactory.nebius.com/v1` | `Authorization: Bearer` (`NEBIUS_API_KEY`) | `oai-compatible-preset` | Dispatchable through OpenAI-compatible fallback | [metadata](../tests/provider_metadata_comprehensive.rs), [factory](../tests/provider_factory.rs), [native-verify harness](../tests/provider_native_verify.rs), [cassette](../tests/fixtures/vcr/verify_nebius_simple_text.json) |
| `ovhcloud` | - | text (+ OAI-compatible tools) | `openai-completions` | `https://oai.endpoints.kepler.ai.cloud.ovh.net/v1` | `Authorization: Bearer` (`OVHCLOUD_API_KEY`) | `oai-compatible-preset` | Dispatchable through OpenAI-compatible fallback | [metadata](../tests/provider_metadata_comprehensive.rs), [factory](../tests/provider_factory.rs), [native-verify harness](../tests/provider_native_verify.rs), [cassette](../tests/fixtures/vcr/verify_ovhcloud_simple_text.json) |
| `scaleway` | - | text (+ OAI-compatible tools) | `openai-completions` | `https://api.scaleway.ai/v1` | `Authorization: Bearer` (`SCALEWAY_API_KEY`) | `oai-compatible-preset` | Dispatchable through OpenAI-compatible fallback | [metadata](../tests/provider_metadata_comprehensive.rs), [factory](../tests/provider_factory.rs), [native-verify harness](../tests/provider_native_verify.rs), [cassette](../tests/fixtures/vcr/verify_scaleway_simple_text.json) |
| `deepseek` | - | text (+ OAI-compatible tools) | `openai-completions` | `https://api.deepseek.com` | `Authorization: Bearer` (`DEEPSEEK_API_KEY`) | `oai-compatible-preset` | Dispatchable through OpenAI-compatible fallback | [metadata](../tests/provider_metadata_comprehensive.rs), [factory](../tests/provider_factory.rs), [e2e](../tests/e2e_cross_provider_parity.rs) |
| `fireworks` | `fireworks-ai` | text (+ OAI-compatible tools) | `openai-completions` | `https://api.fireworks.ai/inference/v1` | `Authorization: Bearer` (`FIREWORKS_API_KEY`) | `oai-compatible-preset` | Dispatchable through OpenAI-compatible fallback | [metadata](../tests/provider_metadata_comprehensive.rs), [factory](../tests/provider_factory.rs), e2e expansion tracked in `bd-3uqg.8.4` |
| `togetherai` | - | text (+ OAI-compatible tools) | `openai-completions` | `https://api.together.xyz/v1` | `Authorization: Bearer` (`TOGETHER_API_KEY`, alt `TOGETHER_AI_API_KEY`) | `oai-compatible-preset` | Dispatchable through OpenAI-compatible fallback | [metadata](../tests/provider_metadata_comprehensive.rs), [factory](../tests/provider_factory.rs), e2e expansion tracked in `bd-3uqg.8.4` |
| `perplexity` | - | text (+ OAI-compatible tools) | `openai-completions` | `https://api.perplexity.ai` | `Authorization: Bearer` (`PERPLEXITY_API_KEY`) | `oai-compatible-preset` | Dispatchable through OpenAI-compatible fallback | [metadata](../tests/provider_metadata_comprehensive.rs), [factory](../tests/provider_factory.rs), e2e expansion tracked in `bd-3uqg.8.4` |
| `xai` | - | text (+ OAI-compatible tools) | `openai-completions` | `https://api.x.ai/v1` | `Authorization: Bearer` (`XAI_API_KEY`) | `oai-compatible-preset` | Dispatchable through OpenAI-compatible fallback | [metadata](../tests/provider_metadata_comprehensive.rs), [factory](../tests/provider_factory.rs), [e2e](../tests/e2e_cross_provider_parity.rs) |
| `minimax` | - | text (Anthropic-compatible) | `anthropic-messages` | `https://api.minimax.io/anthropic/v1/messages` | `x-api-key` (`MINIMAX_API_KEY`) | `oai-compatible-preset` (preset fallback) | Dispatchable through Anthropic API fallback route | [metadata](../tests/provider_metadata_comprehensive.rs), [factory](../tests/provider_factory.rs), [native-verify harness](../tests/provider_native_verify.rs), [cassette](../tests/fixtures/vcr/verify_minimax_simple_text.json) |
| `minimax-cn` | - | text (Anthropic-compatible) | `anthropic-messages` | `https://api.minimaxi.com/anthropic/v1/messages` | `x-api-key` (`MINIMAX_CN_API_KEY`) | `oai-compatible-preset` (preset fallback) | Dispatchable through Anthropic API fallback route | [metadata](../tests/provider_metadata_comprehensive.rs), [factory](../tests/provider_factory.rs), family representative smoke via [`verify_minimax_simple_text.json`](../tests/fixtures/vcr/verify_minimax_simple_text.json) |
| `minimax-coding-plan` | - | text (Anthropic-compatible) | `anthropic-messages` | `https://api.minimax.io/anthropic/v1/messages` | `x-api-key` (`MINIMAX_API_KEY`) | `oai-compatible-preset` (preset fallback) | Dispatchable through Anthropic API fallback route | [metadata](../tests/provider_metadata_comprehensive.rs), [factory](../tests/provider_factory.rs), family representative smoke via [`verify_minimax_simple_text.json`](../tests/fixtures/vcr/verify_minimax_simple_text.json) |
| `minimax-cn-coding-plan` | - | text (Anthropic-compatible) | `anthropic-messages` | `https://api.minimaxi.com/anthropic/v1/messages` | `x-api-key` (`MINIMAX_CN_API_KEY`) | `oai-compatible-preset` (preset fallback) | Dispatchable through Anthropic API fallback route | [metadata](../tests/provider_metadata_comprehensive.rs), [factory](../tests/provider_factory.rs), family representative smoke via [`verify_minimax_simple_text.json`](../tests/fixtures/vcr/verify_minimax_simple_text.json) |
| `siliconflow` | - | text (+ OAI-compatible tools) | `openai-completions` | `https://api.siliconflow.com/v1` | `Authorization: Bearer` (`SILICONFLOW_API_KEY`) | `oai-compatible-preset` | Dispatchable through OpenAI-compatible fallback | [metadata](../tests/provider_metadata_comprehensive.rs), [factory](../tests/provider_factory.rs), [native-verify](../tests/provider_native_verify.rs), [cassette](../tests/fixtures/vcr/verify_siliconflow_simple_text.json) |
| `siliconflow-cn` | - | text (+ OAI-compatible tools) | `openai-completions` | `https://api.siliconflow.cn/v1` | `Authorization: Bearer` (`SILICONFLOW_CN_API_KEY`) | `oai-compatible-preset` | Dispatchable through OpenAI-compatible fallback | [metadata](../tests/provider_metadata_comprehensive.rs), [factory](../tests/provider_factory.rs), [native-verify](../tests/provider_native_verify.rs), [cassette](../tests/fixtures/vcr/verify_siliconflow-cn_simple_text.json) |
| `upstage` | - | text (+ OAI-compatible tools) | `openai-completions` | `https://api.upstage.ai/v1/solar` | `Authorization: Bearer` (`UPSTAGE_API_KEY`) | `oai-compatible-preset` | Dispatchable through OpenAI-compatible fallback | [metadata](../tests/provider_metadata_comprehensive.rs), [factory](../tests/provider_factory.rs), [native-verify](../tests/provider_native_verify.rs), [cassette](../tests/fixtures/vcr/verify_upstage_simple_text.json) |
| `venice` | - | text (+ OAI-compatible tools) | `openai-completions` | `https://api.venice.ai/api/v1` | `Authorization: Bearer` (`VENICE_API_KEY`) | `oai-compatible-preset` | Dispatchable through OpenAI-compatible fallback | [metadata](../tests/provider_metadata_comprehensive.rs), [factory](../tests/provider_factory.rs), [native-verify](../tests/provider_native_verify.rs), [cassette](../tests/fixtures/vcr/verify_venice_simple_text.json) |
| `zai` | - | text (+ OAI-compatible tools) | `openai-completions` | `https://api.z.ai/api/paas/v4` | `Authorization: Bearer` (`ZHIPU_API_KEY`) | `oai-compatible-preset` | Dispatchable through OpenAI-compatible fallback | [metadata](../tests/provider_metadata_comprehensive.rs), [factory](../tests/provider_factory.rs), [native-verify](../tests/provider_native_verify.rs), [cassette](../tests/fixtures/vcr/verify_zai_simple_text.json) |
| `zai-coding-plan` | - | text (+ OAI-compatible tools) | `openai-completions` | `https://api.z.ai/api/coding/paas/v4` | `Authorization: Bearer` (`ZHIPU_API_KEY`) | `oai-compatible-preset` | Dispatchable through OpenAI-compatible fallback | [metadata](../tests/provider_metadata_comprehensive.rs), [factory](../tests/provider_factory.rs), [native-verify](../tests/provider_native_verify.rs), [cassette](../tests/fixtures/vcr/verify_zai-coding-plan_simple_text.json) |
| `zhipuai` | - | text (+ OAI-compatible tools) | `openai-completions` | `https://open.bigmodel.cn/api/paas/v4` | `Authorization: Bearer` (`ZHIPU_API_KEY`) | `oai-compatible-preset` | Dispatchable through OpenAI-compatible fallback | [metadata](../tests/provider_metadata_comprehensive.rs), [factory](../tests/provider_factory.rs), [native-verify](../tests/provider_native_verify.rs), [cassette](../tests/fixtures/vcr/verify_zhipuai_simple_text.json) |
| `zhipuai-coding-plan` | - | text (+ OAI-compatible tools) | `openai-completions` | `https://open.bigmodel.cn/api/coding/paas/v4` | `Authorization: Bearer` (`ZHIPU_API_KEY`) | `oai-compatible-preset` | Dispatchable through OpenAI-compatible fallback | [metadata](../tests/provider_metadata_comprehensive.rs), [factory](../tests/provider_factory.rs), [native-verify](../tests/provider_native_verify.rs), [cassette](../tests/fixtures/vcr/verify_zhipuai-coding-plan_simple_text.json) |
| `amazon-bedrock` | `bedrock` | text + tool-calls | `bedrock-converse-stream` | Region-based AWS endpoint | Explicit `--api-key`/request bearer override first; otherwise `AWS_BEARER_TOKEN_BEDROCK` or SigV4 from a complete env key pair, static/SSO profile, or stored credential (`AWS_REGION`/`AWS_DEFAULT_REGION`) | `native-adapter-required` | VCR-verified (4 scenarios) | [metadata](../tests/provider_metadata_comprehensive.rs), [native-verify](../tests/provider_native_verify.rs), [cassette](../tests/fixtures/vcr/verify_bedrock_simple_text.json), [parity-report](provider-native-parity-report.json) |
| `sap-ai-core` | `sap` | text + tool-calls | OAuth2 + OpenAI-compatible | SAP AI Core service URL | OAuth2 client credentials (`AICORE_SERVICE_KEY`, `SAP_AI_CORE_CLIENT_ID`, `SAP_AI_CORE_CLIENT_SECRET`, `SAP_AI_CORE_TOKEN_URL`, `SAP_AI_CORE_SERVICE_URL`) | `native-adapter-required` | VCR-verified (6 scenarios) | [metadata](../tests/provider_metadata_comprehensive.rs), [native-verify](../tests/provider_native_verify.rs), [cassette](../tests/fixtures/vcr/verify_sap_ai_core_simple_text.json), [parity-report](provider-native-parity-report.json) |
| `github-copilot` | `copilot` | text + tool-calls | Copilot chat/completions | `https://api.githubcopilot.com` | `Authorization: Bearer` (`GITHUB_COPILOT_API_KEY`, `GITHUB_TOKEN`) | `native-adapter-required` | VCR-verified (6 scenarios) | [metadata](../tests/provider_metadata_comprehensive.rs), [native-verify](../tests/provider_native_verify.rs), [cassette](../tests/fixtures/vcr/verify_copilot_simple_text.json), [parity-report](provider-native-parity-report.json) |
| `gitlab` | `gitlab-duo` | text | GitLab AI API | GitLab instance URL | `Authorization: Bearer` (`GITLAB_TOKEN`, `GITLAB_API_KEY`) | `native-adapter-required` | VCR-verified (5 scenarios, no tool_call) | [metadata](../tests/provider_metadata_comprehensive.rs), [native-verify](../tests/provider_native_verify.rs), [cassette](../tests/fixtures/vcr/verify_gitlab_simple_text.json), [parity-report](provider-native-parity-report.json) |
| `opencode` | - | text (+ OAI-compatible tools) | `openai-completions` | `https://opencode.ai/zen/v1` | `Authorization: Bearer` (`OPENCODE_API_KEY`) | `oai-compatible-preset` | VCR-verified (3 scenarios) | [metadata](../tests/provider_metadata_comprehensive.rs), [factory](../tests/provider_factory.rs), [native-verify](../tests/provider_native_verify.rs), [cassette](../tests/fixtures/vcr/verify_opencode_simple_text.json) |
| `vercel` | `vercel-ai-gateway` | text (+ OAI-compatible tools) | `openai-completions` | `https://ai-gateway.vercel.sh/v1` | `Authorization: Bearer` (`AI_GATEWAY_API_KEY`) | `oai-compatible-preset` | VCR-verified (3 scenarios) | [metadata](../tests/provider_metadata_comprehensive.rs), [factory](../tests/provider_factory.rs), [native-verify](../tests/provider_native_verify.rs), [cassette](../tests/fixtures/vcr/verify_vercel_simple_text.json) |
| `zenmux` | - | text (Anthropic-compatible) | `anthropic-messages` | `https://zenmux.ai/api/anthropic/v1/messages` | `x-api-key` (`ZENMUX_API_KEY`) | `oai-compatible-preset` | VCR-verified (3 scenarios) | [metadata](../tests/provider_metadata_comprehensive.rs), [factory](../tests/provider_factory.rs), [native-verify](../tests/provider_native_verify.rs), [cassette](../tests/fixtures/vcr/verify_zenmux_simple_text.json) |

## Historical Verification Status Summary

The status table below is historical evidence from `docs/provider-native-parity-report.json` (`report.generated_at`: `2026-02-12T16:45:00Z`). It should not be used as the current native-module inventory. For the current source inventory, use the provider-count rule above and the governance test that checks `src/providers/*.rs`.

The same historical boundary applies to `provider-baseline-audit.json`,
`provider-parity-checklist.json`, `provider-parity-reconciliation*.json`,
`provider-gaps-audit-report.json`, and `provider-closure-truth-table.json`.
Those frozen artifacts retain their original bytes and era-specific counts for
provenance; none is current release authority.

At the time of that report, all native and preset providers had at least metadata + factory verification. The VCR-verified provider count in that snapshot was 29 out of 85 canonical IDs.

| Category | Count | VCR Coverage | Status |
|----------|-------|-------------|--------|
| Native-implemented metadata IDs | 6 | 6/6 (100%) | Full 6-scenario VCR suites |
| Native adapter required | 4 | 4/4 (100%) | 4-6 scenario VCR suites |
| Wave B1-B3 preset | 19 | 19/19 (100%) | 3-scenario VCR suites |
| Wave C special routing | 3 | 3/3 (100%) | 3-scenario VCR suites |
| Batch A1-A4 preset | 34 | 0/34 (0%) | Metadata + factory verified; individual VCR fixtures pending (`bd-3uqg.8.4`) |
| Local/self-hosted preset | 4 | 0/4 (0%) | Metadata + factory verified; VCR pending |
| Cloudflare gateway | 2 | 0/2 (0%) | Metadata + factory verified; VCR pending |

Consolidated parity report: [`docs/provider-native-parity-report.json`](provider-native-parity-report.json)

The historical deferred/high-risk inventory lives in
`docs/provider-implementation-modes.json`; it must not override the live
registry or factory.

## Historical Already-Covered vs Missing Snapshot

Historical snapshot coverage (85 canonical IDs registered in `PROVIDER_METADATA` at report time):
- 6 native-implemented metadata IDs: `anthropic`, `openai`, `google` (gemini), `cohere`, `azure-openai`, `google-vertex`.
- 4 native adapter providers with VCR verification: `amazon-bedrock`, `sap-ai-core`, `github-copilot`, `gitlab`.
- 12 Wave A OpenAI-compatible preset providers: `groq`, `deepinfra`, `cerebras`, `openrouter`, `mistral`, `moonshotai`, `alibaba` (dashscope), `deepseek`, `fireworks`, `togetherai`, `perplexity`, `xai`.
- 34 Batch A1-A4 OpenAI-compatible preset providers (metadata + factory verified).
- 6 Wave B1 regional/coding-plan providers: `alibaba-cn`, `kimi-for-coding`, `minimax`, `minimax-cn`, `minimax-coding-plan`, `minimax-cn-coding-plan`.
- 5 Wave B2 regional/cloud providers: `modelscope`, `moonshotai-cn`, `nebius`, `ovhcloud`, `scaleway`.
- 8 Wave B3 providers: `siliconflow`, `siliconflow-cn`, `upstage`, `venice`, `zai`, `zai-coding-plan`, `zhipuai`, `zhipuai-coding-plan`.
- 4 Wave C1 local/self-hosted preset providers: `baseten`, `llama`, `lmstudio`, `ollama-cloud`.
- 3 Wave C special routing providers: `opencode`, `vercel`, `zenmux`.
- 2 Cloudflare gateway providers: `cloudflare-ai-gateway`, `cloudflare-workers-ai`.
- Alias coverage: `moonshot`/`kimi` -> `moonshotai`, `alibaba`/`qwen` -> `dashscope`, `fireworks-ai` -> `fireworks`, `gemini` -> `google`, `bedrock` -> `amazon-bedrock`, `copilot` -> `github-copilot`, `azure`/`azure_openai`/`azure-cognitive-services`/`azure-openai-responses` -> `azure-openai`, `vertexai` -> `google-vertex`, `sap` -> `sap-ai-core`, `gitlab-duo` -> `gitlab`, `vercel-ai-gateway` -> `vercel`.

Remaining gaps:
- Batch A1-A4 providers need individual VCR fixture expansion (tracked in `bd-3uqg.8.4`).
- `v0` (Vercel) deferred: no API endpoint published in models.dev.

## Deferred Providers and Rationale

The following providers are recognized in upstream catalogs but explicitly deferred from onboarding. Each entry includes a user-impact statement and an active follow-up bead with an explicit assignee.

| Provider ID | Classification | Deferral Reason | User Impact (Current) | Graduation Condition | Tracking (Owner) |
|-------------|---------------|-----------------|-----------------------|----------------------|------------------|
| `v0` | deferred-watchlist | No API endpoint published in `models.dev`; Vercel's `@ai-sdk/gateway` does not expose a static base URL suitable for preset routing. | Users cannot target `v0` directly from Pi provider config; nearest fallback is `vercel` gateway preset where applicable. | Vercel publishes a stable REST API endpoint with documented auth flow. | `bd-3uqg.11.10.8` (`CopperCreek`) |
| `google-vertex-anthropic` | native-new-high-risk | Hybrid Vertex + Anthropic publisher surface requires dedicated protocol/auth path distinct from the Google-publisher Vertex adapter. | Anthropic-on-Vertex publisher semantics are not first-class; users should use current `google-vertex` (Google publisher) or direct `anthropic` instead. | Anthropic publisher endpoint is validated with streaming + tool-call parity against the existing `google-vertex` Google publisher path. | `bd-3uqg.11.10.9` (`CopperCreek`) |
| `azure-cognitive-services` | native-new-high-risk | Distinct Cognitive Services path requires separate auth/routing semantics beyond current `azure-openai` handling. Already reachable through the `azure-openai` alias family for shared deployments. | Users requiring Cognitive-Services-specific routing guarantees may encounter alias-only behavior; recommended path is canonical `azure-openai` config. | Confirmed need for separate routing path vs. alias sufficiency; if alias-only, close as won't-fix. | `bd-3uqg.11.10.10` (`CopperCreek`) |
| `local` | native-new-high-risk | Generic local runtime mode requires explicit process/model lifecycle integration (start, health-check, shutdown). | No first-class local process lifecycle provider is available; users should use tested local-compatible presets (`lmstudio`) where possible. | Local provider lifecycle adapter is implemented with process management tests. | `bd-3uqg.11.10.11` (`CopperCreek`) |
| `ollama` | native-new-high-risk | Local OSS provider requires dedicated process/orchestration adapter and lifecycle tests distinct from `ollama-cloud`. | Local Ollama daemon is not managed as a native provider path in this wave; users can use `ollama-cloud` where cloud semantics are acceptable. | Ollama process lifecycle adapter is implemented; distinct from cloud variant. | `bd-3uqg.11.10.11` (`CopperCreek`) |

### Batch A1-A4 VCR Gap

33 providers are fully metadata-registered and factory-verified but lack individual VCR fixtures: `302ai`, `abacus`, `aihubmix`, `bailing`, `berget`, `chutes`, `cortecs`, `fastrouter`, `firmware`, `friendli`, `helicone`, `huggingface`, `iflowcn`, `inception`, `inference`, `io-net`, `jiekou`, `lucidquery`, `moark`, `morph`, `nano-gpt`, `nova`, `novita-ai`, `nvidia`, `poe`, `privatemode-ai`, `requesty`, `submodel`, `synthetic`, `vivgrid`, `vultr`, `wandb`, `xiaomi`.

These providers are dispatchable through the OpenAI-compatible fallback and pass metadata + factory tests. Individual VCR fixture expansion is tracked in `bd-3uqg.8.4`.

`github-models` is intentionally absent from the active provider registry:
GitHub retired the GitHub Models service on 2026-07-30. This does not affect the
separate `github-copilot` native provider.

### Implementation Modes Reference

Historical deferred classification profiles are documented in
[`docs/provider-implementation-modes.json`](provider-implementation-modes.json).
Key profile definitions at the time were:

| Profile | Meaning |
|---------|---------|
| `deferred-watchlist` | No validated protocol/auth route; awaiting upstream evidence. |
| `native-new-high-risk` | Requires dedicated native adapter; high test burden (unit + contract + conformance + e2e). |
| `gateway-wrapper-high-risk` | Gateway/router/wrapper requiring routing semantics and upstream provider provenance. |
| `alias-forwarder` | Alias-only resolution; no distinct implementation needed. |

## Matrix Maintenance Guide

When updating the Canonical Provider Matrix table or related sections, follow these annotations to keep the documentation accurate and consistent.

### Adding a New Provider Row

1. Add the canonical metadata entry in `src/provider_metadata.rs` first.
2. Run `cargo test --test provider_metadata_comprehensive -- --nocapture`, then
   `cargo test --test provider_factory -- --nocapture`, to confirm metadata and
   factory behavior independently.
3. Add the provider row to the matrix table above, filling all columns:
   - **Canonical ID**: from `PROVIDER_METADATA.canonical_id`
   - **Aliases**: from `PROVIDER_METADATA.aliases` (dash-separated list, or `-` if none)
   - **Capability flags**: `text`, `+ image`, `+ tool-calls`, etc. based on API family
   - **API family**: one of `anthropic-messages`, `openai-completions`, `openai-responses`, `google-generative-ai`, `google-vertex`, `cohere-chat`, `bedrock-converse-stream`, or provider-specific
   - **Base URL template**: from `PROVIDER_METADATA.routing_defaults.base_url`
   - **Auth mode**: from `PROVIDER_METADATA.auth_env_keys` (header style + env var names)
   - **Mode**: `native-implemented`, `oai-compatible-preset`, `native-adapter-required`, or `alias-only`
   - **Runtime status**: current dispatchability (e.g., "Dispatchable through OpenAI-compatible fallback")
   - **Verification evidence**: links to test files, VCR cassettes, and reports
4. Update the **Verification Status Summary** counts if the VCR coverage changed.
5. Update the **Already-Covered vs Missing Snapshot** category counts.

### Updating VCR Coverage Counts

When new VCR fixtures are added in `tests/fixtures/vcr/verify_*.json`:
1. Count fixtures: `ls tests/fixtures/vcr/verify_*.json | wc -l`
2. Update the "VCR Coverage" column in the Verification Status Summary table.
3. Move the provider from "Metadata + factory verified" to "VCR-verified (N scenarios)" in its matrix row.

### Graduating a Deferred Provider

1. Confirm the graduation condition in the Deferred Providers table is met.
2. Remove the provider from the Deferred Providers table.
3. Add it to the Canonical Provider Matrix with full evidence links.
4. If a new planning snapshot is intentionally commissioned, replace the
   historical implementation-mode artifact as a whole with fresh provenance;
   do not edit it piecemeal and present mixed-era counts as current.
5. Close the associated tracking bead.

### Source-of-Truth Cross-References

| Artifact | Path | Purpose |
|----------|------|---------|
| Provider metadata (canonical IDs, aliases, env keys, routing) | `src/provider_metadata.rs` | Authoritative provider registry |
| Provider factory (route selection, dispatch) | `src/providers/mod.rs` | Runtime provider creation |
| API key resolution | `src/app.rs`, `src/auth.rs`, `src/models.rs` | Credential resolution precedence |
| Metadata invariant tests | `tests/provider_metadata_comprehensive.rs` | Canonical ID, alias, routing, and onboarding invariants |
| Factory routing tests | `tests/provider_factory.rs` | 144 assertions covering factory dispatch |
| Native verify harness (VCR) | `tests/provider_native_verify.rs` | 206 offline streaming/error replay tests |
| Parity report | `docs/provider-native-parity-report.json` | Consolidated per-provider pass/fail matrix |
| Historical implementation modes | `docs/provider-implementation-modes.json` | Frozen 2026-02-13 classification provenance; not live runtime authority |
| Onboarding playbook | `docs/provider-onboarding-playbook.md` | Execution guide for adding/configuring providers |

## Alias Migration Notes

This section documents all alias-to-canonical-ID mappings with migration guidance. Aliases are permanently supported for backward compatibility; no breaking changes are introduced by alias normalization.

### Migration Guarantee

All aliases resolve transparently to their canonical ID at provider-selection time. This means:
- Config files using an alias (`"provider": "gemini"`) continue to work identically to the canonical form (`"provider": "google"`).
- Auth env vars or account-backed credentials are shared: the alias and canonical ID use the same auth source.
- API routing is identical: both resolve to the same base URL, API family, and streaming behavior.
- No deprecation warnings are emitted for alias usage.

### Alias-to-Canonical Mapping Table

| Alias | Canonical ID | API Family | Shared Auth Env Key(s) | Notes |
|-------|-------------|------------|----------------------|-------|
| `gemini` | `google` | `google-generative-ai` | `GOOGLE_API_KEY`, `GEMINI_API_KEY` | Gemini is the model family; `google` is the canonical provider ID. |
| `codex` | `openai-codex` | `openai-codex-responses` | `/login openai-codex` | Short form for the ChatGPT/Codex account bridge. |
| `chatgpt-codex` | `openai-codex` | `openai-codex-responses` | `/login openai-codex` | Explicit ChatGPT/Codex naming; routes to the same account-backed provider. |
| `gemini-cli` | `google-gemini-cli` | `google-gemini-cli` | `/login google-gemini-cli` | Google Cloud Code Assist account-backed provider. |
| `antigravity` | `google-antigravity` | `google-gemini-cli` | `/login google-antigravity` | Google Antigravity account-backed provider using the Gemini CLI API family. |
| `moonshot` | `moonshotai` | `openai-completions` | `MOONSHOT_API_KEY`, `KIMI_API_KEY` | `moonshot` was the original ID; `moonshotai` is canonical per upstream. |
| `kimi` | `moonshotai` | `openai-completions` | `MOONSHOT_API_KEY`, `KIMI_API_KEY` | Kimi is the product name; routes to same endpoint as `moonshotai`. Note: `kimi-for-coding` is a **distinct** canonical ID with its own Anthropic-compatible route. |
| `dashscope` | `alibaba` | `openai-completions` | `DASHSCOPE_API_KEY`, `QWEN_API_KEY` | DashScope is the API platform name; `alibaba` is canonical. Note: `alibaba-cn` is a **distinct** canonical ID with a separate CN base URL. |
| `qwen` | `alibaba` | `openai-completions` | `DASHSCOPE_API_KEY`, `QWEN_API_KEY` | Qwen is the model family; routes to same endpoint as `alibaba`/`dashscope`. |
| `fireworks-ai` | `fireworks` | `openai-completions` | `FIREWORKS_API_KEY` | Legacy naming convention; `fireworks` is canonical per upstream. |
| `vertexai` | `google-vertex` | `google-vertex` | `GOOGLE_CLOUD_API_KEY`, `VERTEX_API_KEY` | Alternative naming for Vertex AI; `google-vertex` is canonical. |
| `bedrock` | `amazon-bedrock` | `bedrock-converse-stream` | `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`, `AWS_BEARER_TOKEN_BEDROCK`, `AWS_PROFILE`, `AWS_REGION` | Short form; `amazon-bedrock` is canonical per AWS naming. |
| `sap` | `sap-ai-core` | OAuth2 + OpenAI-compatible | `AICORE_SERVICE_KEY`, `SAP_AI_CORE_CLIENT_ID`, `SAP_AI_CORE_CLIENT_SECRET`, `SAP_AI_CORE_TOKEN_URL`, `SAP_AI_CORE_SERVICE_URL` | Short form; `sap-ai-core` is canonical per SAP naming. |
| `azure` | `azure-openai` | Azure chat/completions | `AZURE_OPENAI_API_KEY` | Short form; `azure-openai` is canonical. |
| `azure_openai` | `azure-openai` | Azure chat/completions | `AZURE_OPENAI_API_KEY` | Underscore spelling; routes identically to `azure-openai`. |
| `azure-cognitive-services` | `azure-openai` | Azure chat/completions | `AZURE_OPENAI_API_KEY` | Legacy Azure branding; routes identically to `azure-openai`. |
| `azure-openai-responses` | `azure-openai` | Azure chat/completions | `AZURE_OPENAI_API_KEY` | Responses-style Azure spelling; routes identically to `azure-openai`. |
| `copilot` | `github-copilot` | Copilot chat/completions | `GITHUB_COPILOT_API_KEY`, `GITHUB_TOKEN` | Short form; `github-copilot` is canonical. |
| `gitlab-duo` | `gitlab` | GitLab AI API | `GITLAB_TOKEN`, `GITLAB_API_KEY` | Product name; `gitlab` is canonical. |
| `vercel-ai-gateway` | `vercel` | `openai-completions` | `AI_GATEWAY_API_KEY` | Vercel AI Gateway product spelling; `vercel` is canonical. |

### Config Migration Examples

**Before (alias)**:
```json
{
  "providers": {
    "fireworks-ai": {
      "models": [{ "id": "accounts/fireworks/models/llama-v3p3-70b-instruct" }]
    }
  }
}
```

**After (canonical, recommended)**:
```json
{
  "providers": {
    "fireworks": {
      "models": [{ "id": "accounts/fireworks/models/llama-v3p3-70b-instruct" }]
    }
  }
}
```

Both configs produce identical runtime behavior. The alias form continues to work indefinitely.

**CLI migration** (equivalent commands):
```bash
# Alias (still supported)
pi --provider gemini --model gemini-2.5-flash -p "Hello"
# Canonical (recommended)
pi --provider google --model gemini-2.5-flash -p "Hello"
```

### Common Pitfalls

- **`kimi` vs `kimi-for-coding`**: `kimi` is an alias for `moonshotai` (OpenAI-compatible). `kimi-for-coding` is a distinct canonical ID that routes through `anthropic-messages`. A `sk-*` direct key (including via `KIMI_API_KEY`) uses `x-api-key`; `/login kimi-for-coding` and external Kimi access tokens use `Authorization: Bearer` plus Kimi device headers. Do not conflate the providers or the two Kimi wire-auth lanes.
- **`alibaba` vs `alibaba-cn`**: `alibaba` routes to the international DashScope endpoint. `alibaba-cn` is a distinct canonical ID routing to the CN endpoint (`dashscope.aliyuncs.com`). Both use `DASHSCOPE_API_KEY`.
- **`moonshotai` vs `moonshotai-cn`**: Same auth key (`MOONSHOT_API_KEY`), but distinct base URLs (`api.moonshot.ai` vs `api.moonshot.cn`). `moonshotai-cn` is a distinct canonical ID, not an alias.
- **`openrouter` vs `open-router`**: `open-router` is an alias that resolves to canonical `openrouter`. OpenRouter-specific model aliases (for example `gpt-4o-mini`, `claude-3.5-sonnet`) normalize to canonical upstream IDs during lookup/ad-hoc resolution.

### Verification Evidence

Alias resolution is tested by:
- `every_alias_resolves_to_its_canonical_id` in [`tests/provider_metadata_comprehensive.rs`](../tests/provider_metadata_comprehensive.rs): confirms each alias maps to its canonical ID.
- `no_alias_collides_with_canonical_id` in [`tests/provider_metadata_comprehensive.rs`](../tests/provider_metadata_comprehensive.rs): confirms no alias shadows a different provider's canonical ID.
- `fireworks_ai_alias_migration_matches_fireworks_canonical_defaults` in [`tests/provider_factory.rs`](../tests/provider_factory.rs): validates fireworks-ai -> fireworks migration produces identical routing.
- `create_provider_azure_cognitive_services_alias_routes_natively` in [`tests/provider_factory.rs`](../tests/provider_factory.rs): validates azure-cognitive-services -> azure-openai alias routing.

## Provider Selection and Configuration

Credential resolution precedence (runtime):
1. explicit CLI override (`--api-key`)
2. persisted `auth.json` OAuth or bearer credential (an unexpired OAuth `access_token` or `BearerToken`)
3. provider env vars from metadata (ordered; includes shared fallbacks like `GOOGLE_API_KEY` then `GEMINI_API_KEY`)
4. persisted `auth.json` API key
5. supported credentials auto-detected from another local coding CLI (global auth storage only)

After the auth resolver exhausts those five sources, model selection can use an
inline `models.json` `apiKey` fallback (resolved from literal/env/file/shell
sources). Canonical IDs and aliases share the same stored credential lookup;
canonical alias resolution is not a later precedence tier.

Auth diagnostics and redaction contract:
- All auth diagnostics emit `redaction_policy=redact-secrets` and never include raw secrets in user-facing hints.
- Provider missing-key hints are derived from `provider_auth_env_keys(...)`, so aliases (`kimi`, `qwen`) inherit canonical key lists and ordering.
- Key tests: `e2e_all_diagnostic_codes_have_redact_secrets_policy`, `e2e_hints_enrichment_completeness`, `e2e_alias_env_key_consistency` in [`src/error.rs`](../src/error.rs), plus `test_resolve_api_key_*` precedence cases in [`src/auth.rs`](../src/auth.rs).

Choose provider/model via:
- CLI flags: `pi --provider openai --model gpt-4o "Hello"`
- Env vars: `PI_PROVIDER`, `PI_MODEL`
- Settings: `default_provider`, `default_model` in `~/.pi/agent/settings.json`

Custom endpoints and overrides should be configured in `models.json`:
- See [models.md](models.md) for schema and examples.

Example key exports:

```bash
export ANTHROPIC_API_KEY="..."
export OPENAI_API_KEY="..."
export GOOGLE_API_KEY="..."
export COHERE_API_KEY="..."
```
