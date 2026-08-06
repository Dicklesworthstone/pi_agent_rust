# Provider Auth Troubleshooting Matrix (bd-3uqg.11.12.2)

Auth failure modes and exact remediation paths for each gap and longtail
provider, linked to test evidence and the error hint system in `src/error.rs`.

Reconciled against the live registry and auth runtime: 2026-08-06

## Quick Reference

| Provider | Primary Env Var | Fallback Env Var | Get Key |
|---|---|---|---|
| groq | `GROQ_API_KEY` | - | console.groq.com |
| cerebras | `CEREBRAS_API_KEY` | - | cloud.cerebras.ai |
| openrouter | `OPENROUTER_API_KEY` | - | openrouter.ai/keys |
| moonshotai | `MOONSHOT_API_KEY` | `KIMI_API_KEY` | platform.moonshot.cn |
| alibaba | `DASHSCOPE_API_KEY` | `QWEN_API_KEY` | dashscope.console.aliyun.com |
| stackit | `STACKIT_API_KEY` | - | portal.stackit.cloud |
| mistral | `MISTRAL_API_KEY` | - | console.mistral.ai |
| deepinfra | `DEEPINFRA_API_KEY` | - | deepinfra.com/dash |
| togetherai | `TOGETHER_API_KEY` | - | api.together.xyz |
| nvidia | `NVIDIA_API_KEY` | - | build.nvidia.com |
| huggingface | `HF_TOKEN` | - | huggingface.co/settings/tokens |
| ollama-cloud | `OLLAMA_API_KEY` | - | ollama.com |

## Provider name crosswalk (canonical ID / alias / env-key / endpoint)

This crosswalk maps every user-visible provider name (including upstream aliases from opencode and models.dev) to the Pi canonical ID, accepted aliases, auth env vars, and default endpoint. Use this when a user reports "missing provider" or confusion about which name to use.

**Total**: 94 registered canonical provider IDs and 51 aliases. Registration
coverage is not a claim that every ID has an executable native route; use
`pi --list-providers` and the implementation-mode evidence for runtime status.

### Native providers (dedicated adapter)

| Canonical ID | Aliases | Auth env vars | Default endpoint | API type |
|---|---|---|---|---|
| `anthropic` | — | `ANTHROPIC_API_KEY` | `https://api.anthropic.com/v1/messages` | anthropic-messages |
| `openai` | — | `OPENAI_API_KEY` | `https://api.openai.com/v1` | openai-responses |
| `openai-codex` | `codex`, `chatgpt-codex` | _(none; use `/login openai-codex`)_ | `https://chatgpt.com/backend-api/codex/responses` | openai-codex-responses |
| `google` | `gemini` | `GOOGLE_API_KEY`, `GEMINI_API_KEY` | `https://generativelanguage.googleapis.com/v1beta` | google-generative-ai |
| `google-gemini-cli` | `gemini-cli` | _(none; use `/login google-gemini-cli`)_ | _(project-scoped Code Assist endpoint)_ | google-gemini-cli |
| `google-antigravity` | `antigravity` | _(none; use `/login google-antigravity`)_ | _(project-scoped Antigravity endpoint)_ | google-gemini-cli |
| `cohere` | — | `COHERE_API_KEY` | `https://api.cohere.com/v2` | cohere-chat |
| `google-vertex` | `vertexai`, `google-vertex-anthropic` | `GOOGLE_CLOUD_API_KEY`, `VERTEX_API_KEY` | _(per-project URL)_ | google-vertex |
| `amazon-bedrock` | `bedrock` | `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`, `AWS_BEARER_TOKEN_BEDROCK`, `AWS_PROFILE`, `AWS_REGION` | _(per-region URL)_ | bedrock-converse-stream |
| `azure-openai` | `azure`, `azure_openai`, `azure-cognitive-services`, `azure-openai-responses` | `AZURE_OPENAI_API_KEY` | _(per-resource URL)_ | native-azure |
| `github-copilot` | `copilot`, `github-copilot-enterprise` | `GITHUB_COPILOT_API_KEY`, `GITHUB_TOKEN` | _(token exchange)_ | native-copilot |
| `gitlab` | `gitlab-duo` | `GITLAB_TOKEN`, `GITLAB_API_KEY` | _(configurable instance)_ | native-gitlab |
| `cursor` | `cursor-agent` | `CURSOR_API_KEY`, `CURSOR_ACCESS_TOKEN` | `https://api2.cursor.sh/agent.v1.AgentService/Run` | cursor-agent |
| `sap-ai-core` | `sap` | `AICORE_SERVICE_KEY`, `SAP_AI_CORE_CLIENT_ID`, `SAP_AI_CORE_CLIENT_SECRET`, `SAP_AI_CORE_TOKEN_URL`, `SAP_AI_CORE_SERVICE_URL` | _(per-instance)_ | native-sap |
| `v0` | — | `V0_API_KEY` | _(per-instance)_ | native-v0 |

### OpenAI-compatible presets (major)

| Canonical ID | Aliases | Auth env vars | Default endpoint |
|---|---|---|---|
| `groq` | — | `GROQ_API_KEY` | `https://api.groq.com/openai/v1` |
| `cerebras` | — | `CEREBRAS_API_KEY` | `https://api.cerebras.ai/v1` |
| `atlascloud` | `atlas-cloud`, `atlas` | `ATLASCLOUD_API_KEY`, `ATLAS_CLOUD_API_KEY` | `https://api.atlascloud.ai/v1` |
| `openrouter` | `open-router` | `OPENROUTER_API_KEY` | `https://openrouter.ai/api/v1` |
| `mistral` | `mistralai` | `MISTRAL_API_KEY` | `https://api.mistral.ai/v1` |
| `deepseek` | `deep-seek` | `DEEPSEEK_API_KEY` | `https://api.deepseek.com` |
| `deepinfra` | `deep-infra` | `DEEPINFRA_API_KEY` | `https://api.deepinfra.com/v1/openai` |
| `fireworks` | `fireworks-ai` | `FIREWORKS_API_KEY` | `https://api.fireworks.ai/inference/v1` |
| `togetherai` | `together`, `together-ai` | `TOGETHER_API_KEY`, `TOGETHER_AI_API_KEY` | `https://api.together.xyz/v1` |
| `perplexity` | `pplx` | `PERPLEXITY_API_KEY` | `https://api.perplexity.ai` |
| `xai` | `grok`, `x-ai` | `XAI_API_KEY` | `https://api.x.ai/v1` |
| `nvidia` | `nim`, `nvidia-nim` | `NVIDIA_API_KEY` | `https://integrate.api.nvidia.com/v1` |
| `huggingface` | `hf`, `hugging-face` | `HF_TOKEN` | `https://router.huggingface.co/v1` |
| `moonshotai` | `moonshot`, `kimi` | `MOONSHOT_API_KEY`, `KIMI_API_KEY` | `https://api.moonshot.ai/v1` |
| `alibaba` | `dashscope`, `qwen` | `DASHSCOPE_API_KEY`, `QWEN_API_KEY` | `https://dashscope-intl.aliyuncs.com/compatible-mode/v1` |

### OpenAI-compatible presets (regional + coding-plan)

| Canonical ID | Aliases | Auth env vars | Default endpoint |
|---|---|---|---|
| `alibaba-cn` | — | `DASHSCOPE_API_KEY` | `https://dashscope.aliyuncs.com/compatible-mode/v1` |
| `alibaba-us` | — | `DASHSCOPE_API_KEY`, `QWEN_API_KEY` | `https://dashscope-us.aliyuncs.com/compatible-mode/v1` |
| `moonshotai-cn` | — | `MOONSHOT_API_KEY` | `https://api.moonshot.cn/v1` |
| `siliconflow` | `silicon-flow` | `SILICONFLOW_API_KEY` | `https://api.siliconflow.com/v1` |
| `siliconflow-cn` | — | `SILICONFLOW_CN_API_KEY` | `https://api.siliconflow.cn/v1` |
| `modelscope` | — | `MODELSCOPE_API_KEY` | `https://api-inference.modelscope.cn/v1` |
| `nebius` | — | `NEBIUS_API_KEY` | `https://api.tokenfactory.nebius.com/v1` |
| `ovhcloud` | — | `OVHCLOUD_API_KEY` | `https://oai.endpoints.kepler.ai.cloud.ovh.net/v1` |
| `scaleway` | — | `SCALEWAY_API_KEY` | `https://api.scaleway.ai/v1` |
| `stackit` | — | `STACKIT_API_KEY` | `https://api.openai-compat.model-serving.eu01.onstackit.cloud/v1` |
| `upstage` | — | `UPSTAGE_API_KEY` | `https://api.upstage.ai/v1/solar` |
| `venice` | — | `VENICE_API_KEY` | `https://api.venice.ai/api/v1` |
| `zhipuai` | `zhipu`, `glm` | `ZHIPU_API_KEY` | `https://open.bigmodel.cn/api/paas/v4` |
| `zhipuai-coding-plan` | — | `ZHIPU_API_KEY` | `https://open.bigmodel.cn/api/coding/paas/v4` |
| `zai` | — | `ZHIPU_API_KEY` | `https://api.z.ai/api/paas/v4` |
| `zai-coding-plan` | — | `ZHIPU_API_KEY` | `https://api.z.ai/api/coding/paas/v4` |

### Anthropic-compatible presets

| Canonical ID | Aliases | Auth env vars | Default endpoint |
|---|---|---|---|
| `kimi-for-coding` | `kimi-coding`, `kimi-code` | `KIMI_API_KEY` | `https://api.kimi.com/coding/v1/messages` |
| `minimax` | — | `MINIMAX_API_KEY` | `https://api.minimax.io/anthropic/v1/messages` |
| `minimax-cn` | — | `MINIMAX_CN_API_KEY` | `https://api.minimaxi.com/anthropic/v1/messages` |
| `minimax-coding-plan` | — | `MINIMAX_API_KEY` | `https://api.minimax.io/anthropic/v1/messages` |
| `minimax-cn-coding-plan` | — | `MINIMAX_CN_API_KEY` | `https://api.minimaxi.com/anthropic/v1/messages` |
| `zenmux` | — | `ZENMUX_API_KEY` | `https://zenmux.ai/api/anthropic/v1/messages` |

`kimi-for-coding` has two intentional request-auth lanes. A `sk-*` direct API
key, including one supplied through `KIMI_API_KEY`, is sent as `x-api-key`.
Tokens obtained by `/login kimi-for-coding` or imported from the Kimi CLI are
sent as `Authorization: Bearer` with the required Kimi device headers. Do not
diagnose the absence of `x-api-key` as a failure when an OAuth/external token is
active.

### OpenAI-compatible presets (longtail)

| Canonical ID | Aliases | Auth env vars | Default endpoint |
|---|---|---|---|
| `302ai` | — | `302AI_API_KEY` | `https://api.302.ai/v1` |
| `abacus` | — | `ABACUS_API_KEY` | `https://routellm.abacus.ai/v1` |
| `aihubmix` | — | `AIHUBMIX_API_KEY` | `https://aihubmix.com/v1` |
| `bailing` | — | `BAILING_API_TOKEN` | `https://api.tbox.cn/api/llm/v1` |
| `baseten` | — | `BASETEN_API_KEY` | `https://inference.baseten.co/v1` |
| `berget` | — | `BERGET_API_KEY` | `https://api.berget.ai/v1` |
| `chutes` | — | `CHUTES_API_KEY` | `https://llm.chutes.ai/v1` |
| `cloudflare-ai-gateway` | — | `CLOUDFLARE_API_TOKEN` | `https://gateway.ai.cloudflare.com/v1/...` |
| `cloudflare-workers-ai` | — | `CLOUDFLARE_API_TOKEN` | `https://api.cloudflare.com/client/v4/accounts/.../ai/v1` |
| `cortecs` | — | `CORTECS_API_KEY` | `https://api.cortecs.ai/v1` |
| `fastrouter` | — | `FASTROUTER_API_KEY` | `https://go.fastrouter.ai/api/v1` |
| `firmware` | — | `FIRMWARE_API_KEY` | `https://app.firmware.ai/api/v1` |
| `friendli` | — | `FRIENDLI_TOKEN` | `https://api.friendli.ai/serverless/v1` |
| `helicone` | — | `HELICONE_API_KEY` | `https://ai-gateway.helicone.ai/v1` |
| `iflowcn` | — | `IFLOW_API_KEY` | `https://apis.iflow.cn/v1` |
| `inception` | — | `INCEPTION_API_KEY` | `https://api.inceptionlabs.ai/v1` |
| `inference` | — | `INFERENCE_API_KEY` | `https://inference.net/v1` |
| `io-net` | — | `IOINTELLIGENCE_API_KEY` | `https://api.intelligence.io.solutions/api/v1` |
| `jiekou` | — | `JIEKOU_API_KEY` | `https://api.jiekou.ai/openai` |
| `llama` | — | `LLAMA_API_KEY` | `https://api.llama.com/compat/v1` |
| `llamacpp` | `llama-cpp`, `llama.cpp`, `llama-server` | _(none; local llama-server)_ | `http://127.0.0.1:8080/v1` |
| `lmstudio` | `lm-studio` | `LMSTUDIO_API_KEY` | `http://127.0.0.1:1234/v1` |
| `lucidquery` | — | `LUCIDQUERY_API_KEY` | `https://lucidquery.com/api/v1` |
| `mistralrs` | `mistral-rs`, `mistral.rs` | _(none; local mistral.rs server)_ | `http://127.0.0.1:1234/v1` |
| `moark` | — | `MOARK_API_KEY` | `https://moark.com/v1` |
| `morph` | — | `MORPH_API_KEY` | `https://api.morphllm.com/v1` |
| `nano-gpt` | `nanogpt` | `NANO_GPT_API_KEY` | `https://nano-gpt.com/api/v1` |
| `nova` | — | `NOVA_API_KEY` | `https://api.nova.amazon.com/v1` |
| `novita-ai` | `novita` | `NOVITA_API_KEY` | `https://api.novita.ai/openai` |
| `ollama` | — | _(none)_ | `http://127.0.0.1:11434/v1` |
| `ollama-cloud` | — | `OLLAMA_API_KEY` | `https://ollama.com/v1` |
| `opencode` | — | `OPENCODE_API_KEY` | `https://opencode.ai/zen/v1` |
| `poe` | — | `POE_API_KEY` | `https://api.poe.com/v1` |
| `privatemode-ai` | — | `PRIVATEMODE_API_KEY` | `http://localhost:8080/v1` |
| `requesty` | — | `REQUESTY_API_KEY` | `https://router.requesty.ai/v1` |
| `submodel` | — | `SUBMODEL_INSTAGEN_ACCESS_KEY` | `https://llm.submodel.ai/v1` |
| `synthetic` | — | `SYNTHETIC_API_KEY` | `https://api.synthetic.new/v1` |
| `vercel` | `vercel-ai-gateway` | `AI_GATEWAY_API_KEY` | `https://ai-gateway.vercel.sh/v1` |
| `vivgrid` | — | `VIVGRID_API_KEY` | `https://api.vivgrid.com/v1` |
| `vultr` | — | `VULTR_API_KEY` | `https://api.vultrinference.com/v1` |
| `wandb` | — | `WANDB_API_KEY` | `https://api.inference.wandb.ai/v1` |
| `xiaomi` | — | `XIAOMI_API_KEY` | `https://api.xiaomimimo.com/v1` |

### Alias resolution summary

If a user types any of these aliases (left), Pi resolves to the canonical ID (right):

| User input | Resolves to |
|------------|------------|
| `gemini` | `google` |
| `codex`, `chatgpt-codex` | `openai-codex` |
| `gemini-cli` | `google-gemini-cli` |
| `antigravity` | `google-antigravity` |
| `open-router` | `openrouter` |
| `moonshot`, `kimi` | `moonshotai` |
| `dashscope`, `qwen` | `alibaba` |
| `deep-seek` | `deepseek` |
| `deep-infra` | `deepinfra` |
| `fireworks-ai` | `fireworks` |
| `together`, `together-ai` | `togetherai` |
| `pplx` | `perplexity` |
| `grok`, `x-ai` | `xai` |
| `nim`, `nvidia-nim` | `nvidia` |
| `hf`, `hugging-face` | `huggingface` |
| `mistralai` | `mistral` |
| `vertexai`, `google-vertex-anthropic` | `google-vertex` |
| `bedrock` | `amazon-bedrock` |
| `sap` | `sap-ai-core` |
| `azure`, `azure_openai`, `azure-cognitive-services`, `azure-openai-responses` | `azure-openai` |
| `copilot`, `github-copilot-enterprise` | `github-copilot` |
| `gitlab-duo` | `gitlab` |
| `silicon-flow` | `siliconflow` |
| `zhipu`, `glm` | `zhipuai` |
| `nanogpt` | `nano-gpt` |
| `novita` | `novita-ai` |
| `lm-studio` | `lmstudio` |
| `kimi-coding`, `kimi-code` | `kimi-for-coding` |
| `vercel-ai-gateway` | `vercel` |
| `atlas`, `atlas-cloud` | `atlascloud` |
| `cursor-agent` | `cursor` |
| `llama-cpp`, `llama.cpp`, `llama-server` | `llamacpp` |
| `mistral-rs`, `mistral.rs` | `mistralrs` |

### Shared env-key families

Some distinct canonical IDs share environment variables (intentional for provider families):

| Shared env var | Canonical IDs |
|---------------|--------------|
| `DASHSCOPE_API_KEY` | `alibaba`, `alibaba-cn` |
| `MOONSHOT_API_KEY` | `moonshotai`, `moonshotai-cn` |
| `ZHIPU_API_KEY` | `zhipuai`, `zhipuai-coding-plan`, `zai`, `zai-coding-plan` |
| `MINIMAX_API_KEY` | `minimax`, `minimax-coding-plan` |
| `MINIMAX_CN_API_KEY` | `minimax-cn`, `minimax-cn-coding-plan` |
| `CLOUDFLARE_API_TOKEN` | `cloudflare-ai-gateway`, `cloudflare-workers-ai` |

**Validation**: `cargo test --test provider_metadata_comprehensive provider_auth_reference_artifacts_match_runtime_metadata -- --exact`

The former `github-models` preset was removed after GitHub retired the GitHub
Models service on 2026-07-30. `github-copilot` is a separate native provider and
remains supported.

## Failure Mode Matrix

### 1. Missing API Key

**Symptom**: `Missing API key` or `No API key provided`

**Error hint summary**: "Provider API key is missing."

**Remediation by provider**:

| Provider | Fix |
|---|---|
| groq | `export GROQ_API_KEY=gsk_...` |
| cerebras | `export CEREBRAS_API_KEY=csk-...` |
| openrouter | `export OPENROUTER_API_KEY=sk-or-...` |
| moonshotai | `export MOONSHOT_API_KEY=sk-...` or `export KIMI_API_KEY=sk-...` |
| alibaba | `export DASHSCOPE_API_KEY=sk-...` or `export QWEN_API_KEY=sk-...` |
| stackit | `export STACKIT_API_KEY=...` |
| mistral | `export MISTRAL_API_KEY=...` |
| deepinfra | `export DEEPINFRA_API_KEY=...` |
| togetherai | `export TOGETHER_API_KEY=...` |
| nvidia | `export NVIDIA_API_KEY=nvapi-...` |
| huggingface | `export HF_TOKEN=hf_...` |
| ollama-cloud | `export OLLAMA_API_KEY=...` |

**Test evidence**: `cargo test --test provider_native_contract -- failure_taxonomy::all_providers_produce_hint_summary_for_missing_key`

### 2. Authentication Failure (HTTP 401)

**Symptom**: `401 Unauthorized`, `Invalid API key`, `API key expired`

**Error hint summary**: "Provider authentication failed."

**Common causes**:
- Typo in the API key
- Key was revoked or expired
- Wrong key for the provider (e.g., using Groq key with Cerebras)
- Key has restricted IP/referrer policies

**Remediation**:
1. Verify the key is set: `echo $GROQ_API_KEY` (or relevant var)
2. Test with curl: `curl -H "Authorization: Bearer $GROQ_API_KEY" https://api.groq.com/openai/v1/models`
3. Regenerate the key from the provider's dashboard
4. Check the key hasn't been restricted to specific IPs

**Test evidence**: `cargo test --test provider_native_contract -- failure_taxonomy::all_providers_produce_hint_for_auth_failure`

### 3. Rate Limiting (HTTP 429)

**Symptom**: `429 Too Many Requests`, `Rate limit exceeded`

**Error hint summary**: "Provider rate limited the request."

**Remediation**:
1. Wait and retry (providers typically have per-minute quotas)
2. Reduce `max_tokens` to lower compute per request
3. Check provider dashboard for current rate limits
4. Consider upgrading to a higher-tier plan

**Provider-specific rate limits**:

| Provider | Typical Limit | Notes |
|---|---|---|
| groq | 30 RPM (free tier) | Higher tiers available |
| cerebras | Varies by model | Check dashboard |
| openrouter | Depends on upstream provider | Rate limits cascade |
| moonshotai | Varies by plan | Regional limits may apply |
| alibaba | Varies by model | DashScope quota system |
| mistral | Varies by tier | API key dashboard shows limits |

**Test evidence**: `cargo test --test provider_native_contract -- failure_taxonomy::all_providers_produce_hint_for_rate_limit`

### 4. Forbidden (HTTP 403)

**Symptom**: `403 Forbidden`, `Access denied`

**Error hint summary**: "Provider access forbidden."

**Common causes**:
- Account doesn't have access to the requested model
- Organization/project restrictions
- Geographic restrictions

**Remediation**:
1. Verify the model ID is correct and available to your account
2. Check organization-level permissions
3. Contact the provider's support for access escalation

### 5. Quota Exceeded

**Symptom**: `insufficient_quota`, `billing hard limit`, `not enough credits`

**Error hint summary**: "Provider quota or billing limit reached."

**Remediation**:
1. Check billing status on the provider's dashboard
2. Add credits or update payment method
3. Review spending limits and adjust if needed

### 6. Overloaded (HTTP 529)

**Symptom**: `529 Overloaded`, `Service temporarily unavailable`

**Error hint summary**: "Provider is overloaded."

**Remediation**:
1. Wait and retry (typically resolves within minutes)
2. Consider switching to a less-loaded model
3. If persistent, check provider status page

## Env Var Precedence

For providers with multiple env vars, the precedence order is:

| Provider | Precedence (first found wins) |
|---|---|
| moonshotai | `MOONSHOT_API_KEY` > `KIMI_API_KEY` |
| alibaba | `DASHSCOPE_API_KEY` > `QWEN_API_KEY` |

All other providers have a single env var.

**Test evidence**: `cargo test --test provider_native_contract -- failure_taxonomy::provider_key_hints_reference_correct_env_var`

## Runtime Error Hint System

The error hint system in `src/error.rs` provides structured remediation:

```rust
// Example: creating a provider error
let err = Error::Provider {
    provider: "groq".to_string(),
    message: "401 Unauthorized".to_string(),
};
let hints = err.hints();
// hints.summary: "Provider authentication failed."
// hints.hints: ["Set `GROQ_API_KEY` for provider `groq`.", "If using OAuth, run `/login` again."]
// hints.context: [("provider", "groq"), ("details", "401 Unauthorized")]
```

The hint system is tested against all 12 failure-taxonomy provider IDs across 7 failure categories:
`cargo test --test provider_native_contract -- failure_taxonomy`

## Auth failure-signature catalog (native providers)

This section catalogs concrete auth failure signatures for each native provider family, with diagnostic codes, response body shapes, and VCR evidence links.

### Diagnostic code reference

The `AuthDiagnosticCode` enum (`src/error.rs:67-81`) provides stable machine codes. Each code has a wire string, remediation text, and redaction policy (`redact-secrets` for all codes).

| Code | Wire string | Triggers on |
|------|-------------|------------|
| `MissingApiKey` | `auth.missing_api_key` | Pre-request: no key in env/config/override |
| `InvalidApiKey` | `auth.invalid_api_key` | HTTP 401/403, "unauthorized", "invalid api key" |
| `QuotaExceeded` | `auth.quota_exceeded` | "insufficient_quota", "billing hard limit" |
| `OAuthTokenExchangeFailed` | `auth.oauth.token_exchange_failed` | "token exchange failed" |
| `OAuthTokenRefreshFailed` | `auth.oauth.token_refresh_failed` | "token refresh failed" |
| `MissingAzureDeployment` | `config.azure.missing_deployment` | "resource+deployment", "missing deployment" |
| `MissingRegion` | `config.auth.missing_region` | "missing region" |
| `MissingProject` | `config.auth.missing_project` | "missing project" |
| `MissingCredentialChain` | `auth.credential_chain.missing` | "credential chain", "aws_access_key_id" |

### Anthropic

**Auth mechanism**: API key via `x-api-key` header (not Bearer)
**Env vars**: `ANTHROPIC_API_KEY`
**OAuth**: Built-in (claude.ai authorize → console.anthropic.com token)

| Failure mode | HTTP status | Response body shape | Diagnostic code | VCR cassette |
|-------------|------------|--------------------|-----------------|----|
| Missing key | N/A | Pre-request validation | `MissingApiKey` | — |
| Invalid key | 401 | `{"type":"error","error":{"type":"authentication_error","message":"..."}}` | `InvalidApiKey` | `verify_anthropic_error_auth_401.json` |
| Rate limit | 429 | `{"type":"error","error":{"type":"rate_limit_error","message":"..."}}` | — | `verify_anthropic_error_rate_limit_429.json` |
| Bad request | 400 | `{"type":"error","error":{"type":"invalid_request_error","message":"..."}}` | — | `verify_anthropic_error_bad_request_400.json` |

**User-facing message**: `"Missing API key for Anthropic. Set ANTHROPIC_API_KEY or use 'pi auth'."`
**Source**: `src/providers/anthropic.rs:158-167`

### OpenAI

**Auth mechanism**: Bearer token via `Authorization` header
**Env vars**: `OPENAI_API_KEY`

| Failure mode | HTTP status | Response body shape | Diagnostic code | VCR cassette |
|-------------|------------|--------------------|-----------------|----|
| Missing key | N/A | Pre-request validation | `MissingApiKey` | — |
| Invalid key | 401 | `{"error":{"code":"invalid_api_key","message":"...","param":null,"type":"invalid_request_error"}}` | `InvalidApiKey` | `verify_openai_error_auth_401.json` |
| Rate limit | 429 | `{"error":{"code":"rate_limit_exceeded","message":"...","type":"requests"}}` | — | `verify_openai_error_rate_limit_429.json` |

**User-facing message**: `"Missing API key for OpenAI. Set OPENAI_API_KEY or configure in settings."`
**Source**: `src/providers/openai.rs:256-276`

### Gemini (Google Generative AI)

**Auth mechanism**: API key as URL query parameter (`?key=<key>`)
**Env vars**: `GOOGLE_API_KEY`, `GEMINI_API_KEY` (fallback)

| Failure mode | HTTP status | Response body shape | Diagnostic code | VCR cassette |
|-------------|------------|--------------------|-----------------|----|
| Missing key | N/A | Pre-request validation | `MissingApiKey` | — |
| Invalid key | 401 | `{"error":{"code":401,"message":"API key not valid...","status":"UNAUTHENTICATED"}}` | `InvalidApiKey` | `verify_gemini_error_auth_401.json` |
| Rate limit | 429 | `{"error":{"code":429,"message":"...","status":"RESOURCE_EXHAUSTED"}}` | — | `verify_gemini_error_rate_limit_429.json` |

**User-facing message**: `"Missing API key for Google/Gemini. Set GOOGLE_API_KEY or GEMINI_API_KEY."`
**Unique**: API key passed in URL, not headers.
**Source**: `src/providers/gemini.rs:152-162`

### Cohere

**Auth mechanism**: Bearer token via `Authorization` header
**Env vars**: `COHERE_API_KEY`

| Failure mode | HTTP status | Response body shape | Diagnostic code | VCR cassette |
|-------------|------------|--------------------|-----------------|----|
| Missing key | N/A | Pre-request validation | `MissingApiKey` | — |
| Invalid key | 401 | `{"message":"..."}` | `InvalidApiKey` | `verify_cohere_error_auth_401.json` |
| Rate limit | 429 | `{"message":"..."}` | — | `verify_cohere_error_rate_limit_429.json` |

**User-facing message**: `"Missing API key for Cohere. Set COHERE_API_KEY or configure in settings."`
**Source**: `src/providers/cohere.rs:118-133`

### Azure OpenAI

**Auth mechanism**: API key via `api-key` header (NOT `Authorization`)
**Env vars**: `AZURE_OPENAI_API_KEY`
**Additional required config**: resource name, deployment name, API version

| Failure mode | HTTP status | Response body shape | Diagnostic code | VCR cassette |
|-------------|------------|--------------------|-----------------|----|
| Missing key | N/A | Pre-request validation | `MissingApiKey` | — |
| Invalid key | 401 | `{"error":{"code":"401","message":"Access denied due to invalid subscription key..."}}` | `InvalidApiKey` | `verify_azure_error_auth_401.json` |
| Missing deployment | N/A | Pre-request validation | `MissingAzureDeployment` | — |
| Wrong endpoint | 401 | Same as invalid key (wrong resource returns 401) | `InvalidApiKey` | — |

**User-facing message**: `"Missing API key for Azure OpenAI. Set AZURE_OPENAI_API_KEY or configure in settings."`
**Unique**: Uses `api-key` header, not `Authorization`. Requires resource+deployment config.
**Source**: `src/providers/azure.rs:167, 188-196`

### Amazon Bedrock

**Auth mechanism**: An explicit `--api-key`/per-request bearer override wins;
otherwise use `AWS_BEARER_TOKEN_BEDROCK` or AWS SigV4 signing.
**Env vars**: `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY` (optionally
`AWS_SESSION_TOKEN`), `AWS_BEARER_TOKEN_BEDROCK`, `AWS_PROFILE` or
`AWS_DEFAULT_PROFILE`, and `AWS_REGION` or `AWS_DEFAULT_REGION`

| Failure mode | HTTP status | Response body shape | Diagnostic code | VCR cassette |
|-------------|------------|--------------------|-----------------|----|
| Missing credentials | N/A | Pre-request validation | `MissingCredentialChain` | — |
| Invalid credentials | 401 | `{"__type":"UnrecognizedClientException","message":"..."}` | `InvalidApiKey` | `verify_bedrock_error_auth_401.json` |
| Wrong region | 403 | `{"__type":"AccessDeniedException","message":"..."}` | `MissingRegion` | — |

**User-facing message**: `"Amazon Bedrock requires AWS credentials. Set AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY, AWS_BEARER_TOKEN_BEDROCK, AWS_PROFILE (static or SSO), or store amazon-bedrock credentials in auth.json. For SSO profiles, run: aws sso login --profile <name>. An explicit direct bearer token may be passed via --api-key or StreamOptions.api_key."`
**Unique**: A non-empty explicit direct bearer override takes precedence over
all automatic AWS sources. Without one, the resolver checks the Bedrock bearer
env token, complete env IAM keys, a static or SSO profile, then stored
credentials. A region alone, profile name alone, access-key ID alone, or orphan
session token is never treated as a bearer credential. SigV4 signing occurs at
request time.
**Source**: `src/providers/bedrock.rs` (`BedrockProvider::resolve_auth_context`,
request signing) and `src/auth.rs` (`resolve_aws_credentials_async`)

### GitHub Copilot

**Auth mechanism**: Two-step OAuth (GitHub token → Copilot session token exchange)
**Env vars**: `GITHUB_TOKEN`
**OAuth**: Built-in device code flow

| Failure mode | HTTP status | Response body shape | Diagnostic code | VCR cassette |
|-------------|------------|--------------------|-----------------|----|
| Missing token | N/A | Pre-request validation | `MissingApiKey` | — |
| Token exchange failed | 401 | `{"message":"..."}` at `/copilot_internal/v2/token` | `OAuthTokenExchangeFailed` | `verify_copilot_error_auth_401.json` |
| No Copilot access | 403 | GitHub API 403 during token exchange | `InvalidApiKey` | — |

**User-facing message**: `"Copilot token exchange failed (HTTP 401). Verify your GitHub token has Copilot access."`
**Unique**: Requires GitHub Copilot entitlement. Token exchange happens before every request.
**Source**: `src/providers/copilot.rs:134-213`

### GitLab Duo

**Auth mechanism**: PAT or OAuth Bearer token
**Env vars**: `GITLAB_TOKEN`, `GITLAB_API_KEY`
**OAuth**: Self-hosted-aware (configurable base URL)

| Failure mode | HTTP status | Response body shape | Diagnostic code | VCR cassette |
|-------------|------------|--------------------|-----------------|----|
| Missing token | N/A | Pre-request validation | `MissingApiKey` | — |
| Invalid token | 401 | `{"message":"..."}` | `InvalidApiKey` | `verify_gitlab_error_auth_401.json` |
| Wrong instance | N/A | Connection error (wrong base URL) | `MissingEndpoint` | — |

**User-facing message**: `"GitLab API token is required. Set GITLAB_TOKEN or GITLAB_API_KEY environment variable."`
**Unique**: Self-hosted support requires correct base URL. Default scopes: `api read_api read_user`.
**Source**: `src/providers/gitlab.rs:261-265, 292-296`

### Google Vertex AI

**Auth mechanism**: Bearer token with project/location resolution
**Env vars**: `GOOGLE_CLOUD_API_KEY`, `VERTEX_API_KEY`, `GOOGLE_CLOUD_PROJECT`, `VERTEX_PROJECT`, `GOOGLE_CLOUD_LOCATION`, `VERTEX_LOCATION`

| Failure mode | HTTP status | Response body shape | Diagnostic code | VCR cassette |
|-------------|------------|--------------------|-----------------|----|
| Missing token | N/A | Pre-request validation | `MissingApiKey` | — |
| Invalid token | 401 | `{"error":{"code":401,"message":"...","status":"UNAUTHENTICATED"}}` | `InvalidApiKey` | `verify_vertex_error_auth_401.json` |
| Missing project | N/A | Pre-request validation | `MissingProject` | — |
| Missing region | N/A | Pre-request validation | `MissingRegion` | — |

**User-facing message**: `"Missing Vertex AI API key / access token. Set GOOGLE_CLOUD_API_KEY or VERTEX_API_KEY."`
**Unique**: Requires project + location in addition to auth. Multiple env var fallback paths.
**Source**: `src/providers/vertex.rs:125-140, 256-267`

### Response body shape reference

Providers follow distinct error envelope formats:

| Provider family | Error envelope |
|----------------|---------------|
| Anthropic | `{"type":"error","error":{"type":"...","message":"..."}}` |
| OpenAI-compatible | `{"error":{"code":"...","message":"...","param":null,"type":"..."}}` |
| Google (Gemini/Vertex) | `{"error":{"code":N,"message":"...","status":"..."}}` |
| AWS Bedrock | `{"__type":"ExceptionName","message":"..."}` |
| Simple (Cohere/Copilot/GitLab) | `{"message":"..."}` |
| Azure OpenAI | `{"error":{"code":"...","message":"..."}}` (like OpenAI but `code` is string) |

### Auth credential resolution precedence

The auth system (`src/auth.rs`) resolves credentials in this order:

1. **Explicit override** — `--api-key` flag or per-request key
2. **Stored OAuth or bearer token** — an unexpired OAuth access token or a
   `BearerToken` entry in `~/.pi/agent/auth.json`
3. **Environment variables** — provider-specific vars from
   `provider_auth_env_keys()`, in their documented order
4. **Stored API key** — an `ApiKey` entry in `~/.pi/agent/auth.json`
5. **External coding-CLI credential** — supported credentials auto-detected
   from another local coding CLI, only when using Pi's global auth storage

Canonical IDs and aliases share credential lookup at each applicable tier; alias
resolution is not a separate, lower-priority credential source. If all five auth
sources are absent, model selection can still use an inline `models.json`
`apiKey` fallback.

**OAuth proactive refresh**: tokens refresh 10 minutes before expiry to avoid mid-request expiration.

**Credential types** (`src/auth.rs`):
- `ApiKey` — static key
- `OAuth` — access + refresh tokens with expiry metadata
- `AwsCredentials` — IAM keys + optional session token + region
- `BearerToken` — pre-authenticated bearer tokens
- `ServiceKey` — client credentials (client_id + client_secret → token exchange)

## Redaction and safe-diagnostics expectations

This section defines what sensitive data must never appear in logs, transcripts, or artifacts, how redaction is validated, and how operators can safely debug auth issues.

### What must never appear in output

The following data categories are classified as sensitive and must be redacted in all observable surfaces (JSONL logs, VCR cassettes, error messages, terminal output):

| Data category | Examples | Redaction placeholder |
|--------------|---------|----------------------|
| API keys | `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, any `*_API_KEY` | `[REDACTED]` |
| Bearer tokens | OAuth access tokens, refresh tokens, session tokens | `[REDACTED]` |
| Passwords | Client secrets, database passwords | `[REDACTED]` |
| Private keys | PEM keys, SSH keys | `[REDACTED]` |
| Session cookies | HTTP cookies with auth context | `[REDACTED]` |
| AWS credentials | `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN` | `[REDACTED]` |

### Redaction layers

Three independent redaction layers ensure defense-in-depth:

**Layer 1: VCR cassette redaction** (`src/vcr.rs`)
- **Headers**: `authorization`, `x-api-key`, `api-key`, `x-goog-api-key`, `x-azure-api-key`, `proxy-authorization`
- **JSON body fields**: Any key containing `api_key`, `apikey`, `authorization`, `token` (singular, not `tokens`), `access_tokens`, `refresh_tokens`, `id_tokens`, `secret`, `password`
- **Applied**: Automatically on cassette record (`redact_cassette()`) and on body comparison during playback (`redact_json()`)
- **Placeholder**: `"[REDACTED_BY_VCR]"`

**Layer 2: JSONL log context redaction** (`tests/common/logging.rs`)
- **Context map keys**: `api_key`, `api-key`, `authorization`, `bearer`, `cookie`, `credential`, `password`, `private_key`, `secret`, `token`
- **Applied**: Automatically when `TestLogger.info_ctx()` or similar methods are called
- **Matching**: Case-insensitive substring match (e.g., `MY_API_KEY_HEADER` matches `api_key`)
- **Placeholder**: `"[REDACTED]"`

**Layer 3: Live E2E header redaction** (`tests/common/harness.rs`)
- **Header keys**: Same 10 fragments as Layer 2
- **Applied**: `redact_sensitive_header_pairs()` on all live HTTP headers before logging
- **Placeholder**: `"[REDACTED]"`

**Layer 4: Error diagnostic redaction** (`src/error.rs`)
- **Policy**: All `AuthDiagnosticCode` variants return `redaction_policy: "redact-secrets"`
- **Applied**: Downstream consumers (error display, telemetry) must respect this policy
- **Effect**: Error messages include diagnostic codes and remediation text but never raw credentials

### Validation approach

**Automated test: `find_unredacted_keys()`** (`tests/common/logging.rs`)

This function recursively scans any JSON value and returns paths to sensitive keys whose values are NOT the redaction placeholder. Use as an assertion:

```rust
let leaks = find_unredacted_keys(&json_artifact);
assert!(leaks.is_empty(), "Unredacted sensitive data found: {leaks:?}");
```

**VCR cassette redaction tests** (`src/vcr.rs` test module):
- `redact_json_flat_object` — verifies `api_key` is redacted in flat objects
- `redact_json_nested` — verifies nested JSON bodies are recursively redacted
- `oauth_refresh_invalid_matches_after_redaction` — verifies real OAuth cassettes match after redaction
- `sensitive_key_token_but_not_tokens` — verifies `max_tokens` (count) is NOT redacted while `access_token` (auth) IS redacted

**JSONL redaction tests** (`tests/common/logging.rs` test module):
- `test_redaction` — verifies `Authorization` header value is replaced with `[REDACTED]`
- `redaction_case_insensitive_key_matching` — verifies case-insensitive matching
- `redaction_partial_key_match` — verifies substring matching (e.g., `x-api-key-header`)
- `redact_json_value_all_sensitive_key_patterns` — verifies all 10 key patterns

### Safe debugging workflow

When an auth failure occurs, operators can safely debug using these surfaces:

1. **Error diagnostic code**: The `AuthDiagnosticCode` wire string (e.g., `auth.missing_api_key`) identifies the failure class without exposing credentials

2. **Remediation text**: Each diagnostic code has a static remediation string (e.g., "Set the provider API key env var or run `/login <provider>`.") that guides resolution

3. **VCR cassette replay**: Recorded cassettes have all sensitive fields pre-redacted. Replay via `VCR_MODE=playback` reproduces the exact failure path without live credentials

4. **JSONL log inspection**: Test logs include structured categories (`setup`, `action`, `verify`, `error`) with context maps that have been auto-redacted. Safe to share with collaborators

5. **Provider-specific env check**: `echo $PROVIDER_API_KEY | wc -c` confirms a key is set without revealing its value

### CI artifact retention and replay for auth failures

When auth regressions fail in CI, debug from retained artifacts instead of rerunning blindly.

Required references:
- Artifact contract: `docs/provider_e2e_artifact_contract.json`
- Replay runbook: `docs/ci-operator-runbook.md`

Minimal replay path:

```bash
# Reproduce all failures captured by a prior CI run
./scripts/e2e/run_all.sh --rerun-from tests/e2e_results/<timestamp>/summary.json

# Replay retention and replay-contract checks directly
cargo test --test ci_artifact_retention -- --nocapture
cargo test --test e2e_replay_bundles -- --nocapture
cargo test --test e2e_replay_bundle_validation -- --nocapture
```

Auth-specific triage sequence:
1. Open `tests/e2e_results/<timestamp>/replay_bundle.json` and run `one_command_replay`.
2. For each failed provider suite, run `failed_suites[].targeted_replay`.
3. Confirm `failure_digest.json` contains auth-class root cause and replay pointers.
4. Verify redaction remained intact in retained logs (`test-log.jsonl`, `artifact-index.jsonl`) by checking:
   - `tests/e2e_artifact_retention_triage.rs::log_redacts_api_keys`
   - `tests/e2e_artifact_retention_triage.rs::log_redacts_authorization_headers`

### Anti-patterns (what NOT to do)

| Anti-pattern | Why it's dangerous | Safe alternative |
|-------------|-------------------|-----------------|
| Log full HTTP request body | May contain API keys in body fields | Use `redact_json()` before logging |
| Include `Authorization` header in error messages | Exposes bearer tokens | Log only status code + diagnostic code |
| Echo API key in remediation text | Prints secret to terminal | Print env var *name*, not value |
| Store raw cassettes without redaction | Cassette files may be committed to git | Always record through `VcrRecorder` which auto-redacts |
| Disable redaction for debugging | Secrets may persist in log files | Use `find_unredacted_keys()` assertion to verify |

## Related Artifacts

- Provider metadata: `src/provider_metadata.rs`
- Error hint system: `src/error.rs::provider_hints()`
- Auth diagnostic codes: `src/error.rs::AuthDiagnosticCode`
- Auth resolution: `src/auth.rs`
- Contract tests: `tests/provider_native_contract.rs::failure_taxonomy`
- Conformance cassettes: `tests/fixtures/vcr/verify_*_error_auth_401.json`
- Provider gap test matrix: `docs/provider-gaps-test-matrix.json`
- Longtail evidence: `docs/provider-longtail-evidence.md`
