# Models Configuration

Pi loads available models from a built-in registry and an optional user-defined `models.json`.

## Location

| Path | Description |
|------|-------------|
| `~/.pi/agent/models.json` | User-defined model overrides and custom providers |
| `~/.pi/agent/models.fetched.json` | Generated v2 live-catalog membership; managed only by `--persist-models` |

Do not hand-edit `models.fetched.json`. Its provider/model IDs are bound to the
fetching endpoint and transport shape by a non-secret fingerprint and timestamp. The
fingerprint excludes credential values, URL query values, and header values, so it
cannot serve as an offline credential verifier. It binds recognized credential-query
ordered name/presence shape (including exact query-name casing) and
case-insensitive header name/presence shape. A non-empty query/header value
outside a recognized credential channel may select a tenant or deployment; Pi
therefore refuses persistence for that route and ignores legacy generated membership
under such a current route. Pi ignores mismatched generated rows and asks you to
refresh them. Because the fingerprint deliberately excludes credential values,
switching accounts without changing the endpoint/transport shape does not
invalidate saved membership automatically; rerun
`--fetch-models <provider> --refresh-models --persist-models` after such a switch.
Inference requests still resolve the current account's credential, while only
the opt-in saved model list can remain stale. Hand-authored `models.json` remains
authoritative.

Legacy `pi.models.fetched.v1` files lack the endpoint and transport provenance
required by v2 and are preserved rather than overwritten automatically. Move
the legacy file aside to `models.fetched.v1.backup.json`, then run a verified
live `--fetch-models <provider> --refresh-models --persist-models` command to
create a v2 catalog.

## Schema

The root object contains a `providers` map.

```json
{
  "providers": {
    "openai": { ... },
    "anthropic": { ... },
    "ollama": { ... }
  }
}
```

### Provider Config

| Field | Type | Description |
|-------|------|-------------|
| `baseUrl` | string | Base API URL (e.g. `https://api.openai.com/v1`) |
| `api` | string | Protocol adapter (e.g. `openai-completions`, `openai-responses`, `anthropic-messages`, `google-generative-ai`, `google-vertex`) |
| `apiKey` | string | Fallback API key, env var name, or shell command after normal runtime credential resolution (see Secret Resolution) |
| `models` | object[] | List of models. If omitted, provider settings override built-in config for that provider. |
| `headers` | object | Custom HTTP headers |
| `authHeader` | boolean | If true, sends key in `Authorization: Bearer <key>` |
| `compat` | object | Compatibility flags |

If `models` is provided, built-in models for that provider are replaced with the list in `models.json`.

### Model Config

| Field | Type | Description |
|-------|------|-------------|
| `id` | string | Model ID sent to API |
| `name` | string | Display name |
| `contextWindow` | number | Context window size in tokens |
| `maxTokens` | number | Max output tokens |
| `reasoning` | boolean | True if model supports extended thinking |
| `input` | string[] | `["text", "image"]` |
| `cost` | object | Cost per million tokens |

### Compatibility Flags (`compat`)

| Field | Description |
|-------|-------------|
| `supportsStore` | Enable OpenAI `store` parameter (where supported) |
| `supportsDeveloperRole` | Use `developer` role instead of `system` (OpenAI o1/o3) |
| `supportsReasoningEffort` | Send `reasoning_effort` param (OpenAI) |
| `supportsUsageInStreaming` | Expect usage fields in streaming responses |
| `maxTokensField` | Override param name (e.g., `max_completion_tokens`) |
| `openRouterRouting` | OpenRouter routing metadata (JSON object) |
| `vercelGatewayRouting` | Vercel gateway routing metadata (JSON object) |

## Bundled Provider Registry

The canonical list of bundled providers, aliases, auth environment keys, and
onboarding modes, generated from `PROVIDER_METADATA` in
`src/provider_metadata.rs`. Do not edit the table by hand — regenerate with
`PI_BLESS_MODELS_DOC=1 cargo test --test provider_metadata_comprehensive docs_models_provider_table_matches_registry`.

<!-- PROVIDER_TABLE:BEGIN -->

| Provider ID | Name | Aliases | Auth env keys | Onboarding |
|---|---|---|---|---|
| `anthropic` | Anthropic | - | `ANTHROPIC_API_KEY` | native |
| `openai` | OpenAI | - | `OPENAI_API_KEY` | native |
| `openai-codex` | OpenAI Codex (ChatGPT) | codex, chatgpt-codex | - | native adapter |
| `google` | Google Gemini | gemini | `GOOGLE_API_KEY`, `GEMINI_API_KEY` | native |
| `google-gemini-cli` | Google Cloud Code Assist | gemini-cli | - | native adapter |
| `google-antigravity` | Google Antigravity | antigravity | - | native adapter |
| `cohere` | Cohere | - | `COHERE_API_KEY` | native |
| `cursor` | Cursor | cursor-agent | `CURSOR_API_KEY`, `CURSOR_ACCESS_TOKEN` | native |
| `groq` | Groq | - | `GROQ_API_KEY` | openai-compatible preset |
| `deepinfra` | Deep Infra | deep-infra | `DEEPINFRA_API_KEY` | openai-compatible preset |
| `cerebras` | Cerebras | - | `CEREBRAS_API_KEY` | openai-compatible preset |
| `atlascloud` | Atlas Cloud | atlas-cloud, atlas | `ATLASCLOUD_API_KEY`, `ATLAS_CLOUD_API_KEY` | openai-compatible preset |
| `openrouter` | OpenRouter | open-router | `OPENROUTER_API_KEY` | openai-compatible preset |
| `mistral` | Mistral AI | mistralai | `MISTRAL_API_KEY` | openai-compatible preset |
| `moonshotai` | Moonshot AI | moonshot, kimi | `MOONSHOT_API_KEY`, `KIMI_API_KEY` | openai-compatible preset |
| `alibaba` | Alibaba (Qwen) | dashscope, qwen | `DASHSCOPE_API_KEY`, `QWEN_API_KEY` | openai-compatible preset |
| `deepseek` | DeepSeek | deep-seek | `DEEPSEEK_API_KEY` | openai-compatible preset |
| `fireworks` | Fireworks AI | fireworks-ai | `FIREWORKS_API_KEY` | openai-compatible preset |
| `togetherai` | Together AI | together, together-ai | `TOGETHER_API_KEY`, `TOGETHER_AI_API_KEY` | openai-compatible preset |
| `perplexity` | Perplexity | pplx | `PERPLEXITY_API_KEY` | openai-compatible preset |
| `xai` | xAI (Grok) | grok, x-ai | `XAI_API_KEY` | openai-compatible preset |
| `302ai` | 302.AI | - | `302AI_API_KEY` | openai-compatible preset |
| `abacus` | Abacus AI | - | `ABACUS_API_KEY` | openai-compatible preset |
| `aihubmix` | AIHubMix | - | `AIHUBMIX_API_KEY` | openai-compatible preset |
| `bailing` | Bailing | - | `BAILING_API_TOKEN` | openai-compatible preset |
| `berget` | Berget | - | `BERGET_API_KEY` | openai-compatible preset |
| `chutes` | Chutes | - | `CHUTES_API_KEY` | openai-compatible preset |
| `cortecs` | Cortecs | - | `CORTECS_API_KEY` | openai-compatible preset |
| `fastrouter` | FastRouter | - | `FASTROUTER_API_KEY` | openai-compatible preset |
| `firmware` | Firmware | - | `FIRMWARE_API_KEY` | openai-compatible preset |
| `friendli` | Friendli | - | `FRIENDLI_TOKEN` | openai-compatible preset |
| `helicone` | Helicone | - | `HELICONE_API_KEY` | openai-compatible preset |
| `huggingface` | Hugging Face | hf, hugging-face | `HF_TOKEN` | openai-compatible preset |
| `iflowcn` | iFlow | - | `IFLOW_API_KEY` | openai-compatible preset |
| `inception` | Inception | - | `INCEPTION_API_KEY` | openai-compatible preset |
| `inference` | Inference | - | `INFERENCE_API_KEY` | openai-compatible preset |
| `io-net` | io.net | - | `IOINTELLIGENCE_API_KEY` | openai-compatible preset |
| `jiekou` | Jiekou | - | `JIEKOU_API_KEY` | openai-compatible preset |
| `lucidquery` | LucidQuery | - | `LUCIDQUERY_API_KEY` | openai-compatible preset |
| `moark` | Moark | - | `MOARK_API_KEY` | openai-compatible preset |
| `morph` | Morph | - | `MORPH_API_KEY` | openai-compatible preset |
| `nano-gpt` | NanoGPT | nanogpt | `NANO_GPT_API_KEY` | openai-compatible preset |
| `nova` | Nova | - | `NOVA_API_KEY` | openai-compatible preset |
| `novita-ai` | Novita AI | novita | `NOVITA_API_KEY` | openai-compatible preset |
| `nvidia` | NVIDIA NIM | nim, nvidia-nim | `NVIDIA_API_KEY` | openai-compatible preset |
| `poe` | Poe | - | `POE_API_KEY` | openai-compatible preset |
| `privatemode-ai` | PrivateMode AI | - | `PRIVATEMODE_API_KEY` | openai-compatible preset |
| `requesty` | Requesty | - | `REQUESTY_API_KEY` | openai-compatible preset |
| `submodel` | Submodel | - | `SUBMODEL_INSTAGEN_ACCESS_KEY` | openai-compatible preset |
| `synthetic` | Synthetic | - | `SYNTHETIC_API_KEY` | openai-compatible preset |
| `vivgrid` | Vivgrid | - | `VIVGRID_API_KEY` | openai-compatible preset |
| `vultr` | Vultr | - | `VULTR_API_KEY` | openai-compatible preset |
| `wandb` | Weights & Biases | - | `WANDB_API_KEY` | openai-compatible preset |
| `xiaomi` | Xiaomi | - | `XIAOMI_API_KEY` | openai-compatible preset |
| `alibaba-cn` | Alibaba China | - | `DASHSCOPE_API_KEY` | openai-compatible preset |
| `alibaba-us` | Alibaba US | - | `DASHSCOPE_API_KEY`, `QWEN_API_KEY` | openai-compatible preset |
| `kimi-for-coding` | Kimi for Coding | kimi-coding, kimi-code | `KIMI_API_KEY` | openai-compatible preset |
| `minimax` | MiniMax | - | `MINIMAX_API_KEY` | openai-compatible preset |
| `minimax-cn` | MiniMax China | - | `MINIMAX_CN_API_KEY` | openai-compatible preset |
| `minimax-coding-plan` | MiniMax Coding Plan | - | `MINIMAX_API_KEY` | openai-compatible preset |
| `minimax-cn-coding-plan` | MiniMax China Coding Plan | - | `MINIMAX_CN_API_KEY` | openai-compatible preset |
| `modelscope` | ModelScope | - | `MODELSCOPE_API_KEY` | openai-compatible preset |
| `moonshotai-cn` | Moonshot AI China | - | `MOONSHOT_API_KEY` | openai-compatible preset |
| `nebius` | Nebius | - | `NEBIUS_API_KEY` | openai-compatible preset |
| `ovhcloud` | OVHcloud | - | `OVHCLOUD_API_KEY` | openai-compatible preset |
| `scaleway` | Scaleway | - | `SCALEWAY_API_KEY` | openai-compatible preset |
| `stackit` | STACKIT | - | `STACKIT_API_KEY` | openai-compatible preset |
| `siliconflow` | SiliconFlow | silicon-flow | `SILICONFLOW_API_KEY` | openai-compatible preset |
| `siliconflow-cn` | SiliconFlow China | - | `SILICONFLOW_CN_API_KEY` | openai-compatible preset |
| `upstage` | Upstage | - | `UPSTAGE_API_KEY` | openai-compatible preset |
| `venice` | Venice AI | - | `VENICE_API_KEY` | openai-compatible preset |
| `zai` | Zai | - | `ZHIPU_API_KEY` | openai-compatible preset |
| `zai-coding-plan` | Zai Coding Plan | - | `ZHIPU_API_KEY` | openai-compatible preset |
| `zhipuai` | Zhipu AI | zhipu, glm | `ZHIPU_API_KEY` | openai-compatible preset |
| `zhipuai-coding-plan` | Zhipu AI Coding Plan | - | `ZHIPU_API_KEY` | openai-compatible preset |
| `baseten` | Baseten | - | `BASETEN_API_KEY` | openai-compatible preset |
| `llama` | Meta Llama | - | `LLAMA_API_KEY` | openai-compatible preset |
| `lmstudio` | LM Studio | lm-studio | `LMSTUDIO_API_KEY` | openai-compatible preset |
| `ollama` | Ollama | - | - | openai-compatible preset |
| `llamacpp` | llama.cpp | llama-cpp, llama.cpp, llama-server | - | openai-compatible preset |
| `mistralrs` | mistral.rs | mistral.rs, mistral-rs | - | openai-compatible preset |
| `ollama-cloud` | Ollama Cloud | - | `OLLAMA_API_KEY` | openai-compatible preset |
| `opencode` | OpenCode | opencode-zen | `OPENCODE_API_KEY` | openai-compatible preset |
| `vercel` | Vercel AI | vercel-ai-gateway | `AI_GATEWAY_API_KEY` | openai-compatible preset |
| `zenmux` | ZenMux | - | `ZENMUX_API_KEY` | openai-compatible preset |
| `gmi` | GMI Cloud | gmi-cloud, gmi-serving | `GMI_API_KEY` | openai-compatible preset |
| `coreweave` | CoreWeave Serverless Inference | coreweave-serverless | `COREWEAVE_API_KEY`, `WANDB_API_KEY` | openai-compatible preset |
| `sakana` | Sakana AI | sakana-ai | `SAKANA_API_KEY`, `FUGU_API_KEY` | openai-compatible preset |
| `wafer` | Wafer Serverless | wafer-serverless | `WAFER_SERVERLESS_API_KEY` | openai-compatible preset |
| `qianfan` | Qianfan (Baidu) | baidu-qianfan | `QIANFAN_API_KEY` | openai-compatible preset |
| `umans` | Umans AI Coding Plan | umans-ai | `UMANS_AI_CODING_PLAN_API_KEY` | openai-compatible preset |
| `kilo` | Kilo Gateway | kilo-gateway, kilo-ai | `KILO_API_KEY` | openai-compatible preset |
| `opencode-go` | OpenCode Go | - | `OPENCODE_API_KEY` | openai-compatible preset |
| `cloudflare-ai-gateway` | Cloudflare AI Gateway | - | `CLOUDFLARE_API_TOKEN` | openai-compatible preset |
| `cloudflare-workers-ai` | Cloudflare Workers AI | - | `CLOUDFLARE_API_TOKEN` | openai-compatible preset |
| `google-vertex` | Google Vertex AI | vertexai, google-vertex-anthropic | `GOOGLE_CLOUD_API_KEY`, `VERTEX_API_KEY` | native |
| `amazon-bedrock` | Amazon Bedrock | bedrock | `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`, `AWS_BEARER_TOKEN_BEDROCK`, `AWS_PROFILE`, `AWS_REGION` | native adapter |
| `sap-ai-core` | SAP AI Core | sap | `AICORE_SERVICE_KEY`, `SAP_AI_CORE_CLIENT_ID`, `SAP_AI_CORE_CLIENT_SECRET`, `SAP_AI_CORE_TOKEN_URL`, `SAP_AI_CORE_SERVICE_URL` | native adapter |
| `v0` | v0 by Vercel | - | `V0_API_KEY` | native adapter |
| `azure-openai` | Azure OpenAI | azure, azure_openai, azure-cognitive-services, azure-openai-responses | `AZURE_OPENAI_API_KEY` | native adapter |
| `github-copilot` | GitHub Copilot | copilot, github-copilot-enterprise | `GITHUB_COPILOT_API_KEY`, `GITHUB_TOKEN` | native adapter |
| `gitlab` | GitLab Duo | gitlab-duo | `GITLAB_TOKEN`, `GITLAB_API_KEY` | native adapter |

<!-- PROVIDER_TABLE:END -->

## Examples

### 1. Override OpenAI Base URL (e.g. for Groq)

```json
{
  "providers": {
    "openai": {
      "baseUrl": "https://api.groq.com/openai/v1",
      "apiKey": "gsk_...",
      "models": [
        {
          "id": "llama3-70b-8192",
          "name": "Groq Llama 3 70B",
          "contextWindow": 8192
        }
      ]
    }
  }
}
```

### 2. Azure OpenAI

Azure requires resource-specific URLs and `api-key` header instead of Bearer token.

```json
{
  "providers": {
    "azure-openai": {
      "api": "openai-completions",
      "baseUrl": "https://my-resource.openai.azure.com/openai/deployments/my-deployment",
      "apiKey": "...",
      "authHeader": false,
      "headers": {
        "api-key": "..."
      },
      "models": [
        {
          "id": "gpt-4",
          "contextWindow": 128000
        }
      ]
    }
  }
}
```

### 3. Local LLM (Ollama)

```json
{
  "providers": {
    "ollama": {
      "api": "openai-completions",
      "baseUrl": "http://localhost:11434/v1",
      "apiKey": "ollama",
      "models": [
        {
          "id": "llama3",
          "contextWindow": 8192
        }
      ]
    }
  }
}
```

## Secret Resolution

API keys can be plain strings, environment variables, or shell commands.

Normal runtime credentials—explicit overrides, provider environment variables,
stored auth, and supported external-provider credentials—take precedence over
the provider route's `apiKey`. The `models.json` value is used only when normal
runtime resolution finds no non-empty credential.

- **Environment Variable**: If the string matches an env var name (e.g. `OPENAI_API_KEY`), it is resolved.
- **Shell Command**: Prefix with `!` to execute a command.

```json
{
  "providers": {
    "openai": {
      "apiKey": "!pass show api/openai"
    }
  }
}
```

Shell commands run via `sh -c` on Unix and `cmd /C` on Windows.

### Local providers (no API key)

`ollama`, `llamacpp` (llama.cpp's `llama-server`), `mistralrs` (mistral.rs), and
`lmstudio` are recognized built-in **local** providers. `ollama`, `llamacpp`, and
`mistralrs` require **no API key** — they expose an OpenAI-compatible server on
localhost and are called without an `Authorization` header. They work
out-of-the-box without a `models.json` entry:

```bash
# Defaults: llama-server -> http://127.0.0.1:8080/v1, mistral.rs -> http://127.0.0.1:1234/v1
pi --provider llamacpp  --model ggml-org/gemma-4-E4B-it-GGUF -p "hi"
pi --provider mistralrs --model default -p "hi"
```

Provider aliases are accepted: `llama.cpp` / `llama-cpp` / `llama-server` ->
`llamacpp`, and `mistral.rs` / `mistral-rs` -> `mistralrs`.

To point at a non-default host/port, add a `models.json` entry (no `apiKey`
needed):

```json
{
  "providers": {
    "llamacpp": {
      "baseUrl": "http://127.0.0.1:9090/v1",
      "models": [ { "id": "my-model" } ]
    }
  }
}
```

## User Model Override (extending the bundled snapshot)

Pi ships with a snapshot of every provider's discovery endpoint at
`docs/provider-upstream-model-ids-snapshot.json`. The snapshot is regenerated
ahead of releases, but a new model from a provider (e.g. Anthropic shipping a
new Opus version) is invisible to `/model` until the next release.

Drop a JSON file at `<config_dir>/pi/models-override.json` to extend the
snapshot at runtime. The file uses the same shape as the bundled snapshot:

```json
{
  "anthropic": ["claude-opus-4-7"],
  "openrouter": ["anthropic/claude-opus-4-7"]
}
```

`<config_dir>` is whatever `dirs::config_dir()` reports — `~/.config` on Linux,
`~/Library/Application Support` on macOS, `%APPDATA%` on Windows. Set
`PI_MODELS_OVERRIDE=/path/to/file.json` in the environment to point pi at a
file outside the standard config directory.

Behavior:

- **Additive only.** Override entries union with the bundled snapshot. There
  is no way to *remove* a bundled model via the override file; the provider's
  next refresh will reintroduce anything you delete.
- **Survives upgrades.** The override file is in your user config directory,
  not in pi's binary, so model entries you add stay across releases until the
  bundled snapshot catches up — then they dedupe automatically.
- **Fail-safe.** A missing or malformed override file logs a debug/warning
  line and is treated as empty so a typo never breaks pi startup.
- **Provider IDs must match canonical names.** Use `anthropic`, `openai`,
  `openrouter`, etc. (the keys you see in
  `docs/provider-upstream-model-ids-snapshot.json`).

The override only affects the `/model` autocomplete catalog. To actually call
a model that pi does not yet have a built-in route for, also configure the
provider in `models.json` (sections above) — pi already routes any
`anthropic/<id>` value through the Anthropic API regardless of whether the ID
is in the snapshot.
