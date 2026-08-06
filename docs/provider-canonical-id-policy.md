# Provider Canonical ID + Alias Policy (`bd-3uqg.1.3`)

Generated: `2026-02-10T04:38:00Z`
Last reviewed: `2026-08-06`
Depends on: bd-3uqg.1.1 (upstream snapshot), bd-3uqg.1.2 (baseline audit)

## Normalization Algorithm

When a user supplies a provider ID (CLI flag, config, env var), apply these steps in order:

1. **Trim** leading/trailing whitespace.
2. **Match** registered canonical IDs and aliases case-insensitively.
3. **Canonicalize** a match to the canonical ID stored in `PROVIDER_METADATA`.
4. **No built-in match** is returned for an unknown ID; custom and extension
   routing handles declared IDs outside this built-in lookup.

**Rationale**: [`src/provider_metadata.rs`](../src/provider_metadata.rs) is the
runtime authority. Underscore spellings are accepted only when explicitly
registered as aliases (for example, `azure_openai`); Pi does not apply a generic
underscore-to-hyphen rewrite.

## Conflict Resolution Rules

1. **Runtime metadata wins**: `PROVIDER_METADATA` defines active Pi canonical IDs and aliases.
2. **Verified upstream aliases**: an upstream spelling that differs from Pi's canonical ID can be registered as an alias after its routing and auth behavior are verified.
3. **Retired services stay retired**: an ID may remain in historical upstream snapshots without remaining in the active runtime registry.
4. **Regional variants**: IDs like `alibaba-cn`, `moonshotai-cn` are distinct canonical IDs, not aliases.
5. **Coding-plan variants**: IDs like `minimax-coding-plan` are distinct canonical IDs.
6. **Extension providers**: Declared-ID semantics are owned outside the built-in metadata lookup.

## Deprecation Posture

Only explicitly registered aliases are accepted. There are currently no active
canonical-ID deprecations. `fireworks` and `azure-openai` are the runtime
canonical IDs; `fireworks-ai` and the Azure spellings are aliases.

## Alias Lookup Table

| Alias | Canonical ID | Origin |
|-------|-------------|--------|
| `antigravity` | `google-antigravity` | Pi runtime |
| `atlas`, `atlas-cloud` | `atlascloud` | Pi runtime |
| `azure`, `azure_openai`, `azure-cognitive-services`, `azure-openai-responses` | `azure-openai` | Pi runtime |
| `bedrock` | `amazon-bedrock` | opencode |
| `codex`, `chatgpt-codex` | `openai-codex` | Pi runtime |
| `copilot`, `github-copilot-enterprise` | `github-copilot` | opencode + Pi runtime |
| `cursor-agent` | `cursor` | Pi runtime |
| `dashscope` | `alibaba` | Pi alias |
| `deep-infra` | `deepinfra` | Pi runtime |
| `deep-seek` | `deepseek` | Pi runtime |
| `fireworks-ai` | `fireworks` | Pi runtime |
| `gemini` | `google` | opencode |
| `gemini-cli` | `google-gemini-cli` | Pi runtime |
| `gitlab-duo` | `gitlab` | opencode |
| `glm`, `zhipu` | `zhipuai` | Pi runtime |
| `google-vertex-anthropic`, `vertexai` | `google-vertex` | models.dev + opencode |
| `grok`, `x-ai` | `xai` | Pi runtime |
| `hf`, `hugging-face` | `huggingface` | Pi runtime |
| `kimi` | `moonshotai` | Pi alias |
| `kimi-code`, `kimi-coding` | `kimi-for-coding` | Pi runtime |
| `llama-cpp`, `llama.cpp`, `llama-server` | `llamacpp` | Pi runtime |
| `lm-studio` | `lmstudio` | Pi runtime |
| `mistral-rs`, `mistral.rs` | `mistralrs` | Pi runtime |
| `mistralai` | `mistral` | Pi runtime |
| `moonshot` | `moonshotai` | Pi alias |
| `nanogpt` | `nano-gpt` | Pi runtime |
| `nim`, `nvidia-nim` | `nvidia` | Pi runtime |
| `novita` | `novita-ai` | Pi runtime |
| `open-router` | `openrouter` | Pi runtime |
| `pplx` | `perplexity` | Pi runtime |
| `qwen` | `alibaba` | Pi alias |
| `sap` | `sap-ai-core` | opencode |
| `silicon-flow` | `siliconflow` | Pi runtime |
| `together`, `together-ai` | `togetherai` | Pi runtime |
| `vercel-ai-gateway` | `vercel` | Pi runtime |

Total: 51 aliases across 33 canonical IDs.

## Canonical ID Registry (94 active IDs)

Mirrors the active runtime registry. The machine-readable source is
[`provider-canonical-id-table.json`](provider-canonical-id-table.json); the
policy JSON mirrors the same 94 IDs and 51 aliases.

| Canonical ID | Has Aliases | Source(s) |
|-------------|------------|-----------|
| 302ai | no | models.dev |
| abacus | no | models.dev |
| aihubmix | no | models.dev |
| alibaba | yes (dashscope, qwen) | models.dev |
| alibaba-cn | no | models.dev |
| alibaba-us | no | Pi runtime |
| amazon-bedrock | yes (bedrock) | models.dev + opencode |
| anthropic | no | all |
| atlascloud | yes (atlas, atlas-cloud) | Pi runtime |
| azure-openai | yes (azure, azure_openai, azure-cognitive-services, azure-openai-responses) | Pi runtime |
| bailing | no | models.dev |
| baseten | no | models.dev |
| berget | no | models.dev |
| cerebras | no | models.dev + opencode |
| chutes | no | models.dev |
| cloudflare-ai-gateway | no | models.dev + opencode |
| cloudflare-workers-ai | no | models.dev + opencode |
| cohere | no | models.dev |
| cortecs | no | models.dev |
| cursor | yes (cursor-agent) | Pi runtime |
| deepinfra | yes (deep-infra) | models.dev |
| deepseek | yes (deep-seek) | models.dev |
| fastrouter | no | models.dev |
| fireworks | yes (fireworks-ai) | Pi runtime |
| firmware | no | models.dev |
| friendli | no | models.dev |
| github-copilot | yes (copilot, github-copilot-enterprise) | models.dev + opencode |
| gitlab | yes (gitlab-duo) | models.dev + opencode |
| google | yes (gemini) | models.dev + opencode |
| google-antigravity | yes (antigravity) | Pi runtime |
| google-gemini-cli | yes (gemini-cli) | Pi runtime |
| google-vertex | yes (vertexai, google-vertex-anthropic) | models.dev + opencode |
| groq | no | models.dev + opencode |
| helicone | no | models.dev |
| huggingface | yes (hf, hugging-face) | models.dev |
| iflowcn | no | models.dev |
| inception | no | models.dev |
| inference | no | models.dev |
| io-net | no | models.dev |
| jiekou | no | models.dev |
| kimi-for-coding | yes (kimi-coding, kimi-code) | models.dev |
| llama | no | models.dev |
| llamacpp | yes (llama-cpp, llama.cpp, llama-server) | Pi runtime |
| lmstudio | yes (lm-studio) | models.dev + codex |
| lucidquery | no | models.dev |
| minimax | no | models.dev |
| minimax-cn | no | models.dev |
| minimax-cn-coding-plan | no | models.dev |
| minimax-coding-plan | no | models.dev |
| mistral | yes (mistralai) | models.dev |
| mistralrs | yes (mistral-rs, mistral.rs) | Pi runtime |
| moark | no | models.dev |
| modelscope | no | models.dev |
| moonshotai | yes (moonshot, kimi) | models.dev |
| moonshotai-cn | no | models.dev |
| morph | no | models.dev |
| nano-gpt | yes (nanogpt) | models.dev |
| nebius | no | models.dev |
| nova | no | models.dev |
| novita-ai | yes (novita) | models.dev |
| nvidia | yes (nim, nvidia-nim) | models.dev |
| ollama | no | codex |
| ollama-cloud | no | models.dev |
| openai | no | all |
| openai-codex | yes (codex, chatgpt-codex) | Pi runtime |
| opencode | no | models.dev + opencode |
| openrouter | yes (open-router) | models.dev + opencode |
| ovhcloud | no | models.dev |
| perplexity | yes (pplx) | models.dev |
| poe | no | models.dev |
| privatemode-ai | no | models.dev |
| requesty | no | models.dev |
| sap-ai-core | yes (sap) | models.dev + opencode |
| scaleway | no | models.dev |
| siliconflow | yes (silicon-flow) | models.dev |
| siliconflow-cn | no | models.dev |
| stackit | no | Pi runtime |
| submodel | no | models.dev |
| synthetic | no | models.dev |
| togetherai | yes (together, together-ai) | models.dev |
| upstage | no | models.dev |
| v0 | no | models.dev |
| venice | no | models.dev |
| vercel | yes (vercel-ai-gateway) | models.dev + opencode |
| vivgrid | no | models.dev |
| vultr | no | models.dev |
| wandb | no | models.dev |
| xai | yes (grok, x-ai) | models.dev + opencode |
| xiaomi | no | models.dev |
| zai | no | models.dev |
| zai-coding-plan | no | models.dev |
| zenmux | no | models.dev + opencode |
| zhipuai | yes (zhipu, glm) | models.dev |
| zhipuai-coding-plan | no | models.dev |

## Credential Resolution Precedence

`AuthStorage::resolve_api_key` applies this order:

1. explicit override;
2. stored unexpired OAuth access token or `BearerToken`;
3. provider environment variables in metadata order;
4. stored `ApiKey`;
5. a supported credential auto-detected from another local coding CLI, only
   when Pi's global auth storage is in use.

Canonical IDs and aliases share stored and external credential lookup. Alias
resolution is not a separate precedence tier. If the auth resolver returns no
credential, app-level model selection can still use an inline `models.json`
`apiKey` fallback.

## Retired Provider IDs

`github-models` is not an active canonical ID or alias. GitHub retired that
service on 2026-07-30. This does not remove or rename `github-copilot`, which is
a separate supported native provider.

## Implementation Guidance

Use the existing runtime metadata functions rather than maintaining a second
normalization implementation:

```rust
use pi::provider_metadata::{canonical_provider_id, provider_auth_env_keys};

let canonical = canonical_provider_id(user_input);
let auth_env_keys = provider_auth_env_keys(user_input);
```

Provider entry points should resolve through `provider_metadata()` or
`canonical_provider_id()` before selecting built-in routing or credentials.
Custom and extension routing handles the no-built-in-match case separately.
