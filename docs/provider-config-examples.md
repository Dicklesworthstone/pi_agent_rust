# Provider Configuration Examples

**Bead:** bd-3uqg.9.2
**Updated:** 2026-02-13

Copy-paste-ready configuration examples for every provider family.
Each section shows the minimal setup, advanced options, aliases, and known pitfalls.

---

## Built-In Native Providers

These providers have dedicated Rust implementations with full streaming, tool calling,
and reasoning support.

### Anthropic

```bash
export ANTHROPIC_API_KEY="sk-ant-..."

pi --provider anthropic --model claude-sonnet-4-5
```

**Endpoint**: `https://api.anthropic.com/v1/messages`
**Auth**: `x-api-key` header via `ANTHROPIC_API_KEY`
**API family**: `anthropic-messages`
**Models**: claude-opus-4-5, claude-sonnet-4-5, claude-haiku-4-5, claude-sonnet-4-20250514
**Supports**: Text + image input, reasoning (thinking), tool calling, streaming

**Advanced**: Custom base URL (e.g., corporate proxy):
```bash
pi --provider anthropic --model claude-sonnet-4-5 --base-url "https://proxy.corp.example.com/anthropic"
```

### OpenAI

```bash
export OPENAI_API_KEY="sk-..."

pi --provider openai --model gpt-4o
```

**Endpoint**: `https://api.openai.com/v1`
**Auth**: Bearer token via `OPENAI_API_KEY`
**API family**: `openai-responses` (native), `openai-completions` (compat)
**Models**: gpt-5.1-codex, gpt-4o, gpt-4o-mini
**Supports**: Text + image input, reasoning, tool calling, streaming

### Google Gemini

```bash
# Either env var works
export GOOGLE_API_KEY="AIza..."
# or
export GEMINI_API_KEY="AIza..."

pi --provider google --model gemini-2.5-pro
# or with alias
pi --provider gemini --model gemini-2.5-flash
```

**Endpoint**: `https://generativelanguage.googleapis.com/v1beta`
**Auth**: API key via `GOOGLE_API_KEY` (primary) or `GEMINI_API_KEY` (fallback)
**API family**: `google-generative-ai`
**Models**: gemini-2.5-pro, gemini-2.5-flash, gemini-1.5-pro, gemini-1.5-flash
**Aliases**: `google`, `gemini`
**Supports**: Text + image input, reasoning, tool calling, streaming

### Google Vertex AI

```bash
# Either env var works
export GOOGLE_CLOUD_API_KEY="..."
# or
export VERTEX_API_KEY="..."

pi --provider google-vertex --model gemini-2.5-pro
# or with alias
pi --provider vertexai --model gemini-2.5-pro
```

**Endpoint**: Region-based (e.g., `https://us-central1-aiplatform.googleapis.com/...`)
**Auth**: API key via `GOOGLE_CLOUD_API_KEY` (primary) or `VERTEX_API_KEY` (fallback)
**API family**: `google-vertex`
**Aliases**: `google-vertex`, `vertexai`
**Context window**: Up to 1,000,000 tokens

**Caveat**: Base URL is constructed dynamically from region and project. Use
`--base-url` to override if needed:
```bash
pi --provider google-vertex --model gemini-2.5-pro \
  --base-url "https://europe-west4-aiplatform.googleapis.com/v1/projects/my-project/locations/europe-west4/publishers/google/models"
```

### Cohere

```bash
export COHERE_API_KEY="..."

pi --provider cohere --model command-r-plus
```

**Endpoint**: `https://api.cohere.com/v2`
**Auth**: Bearer token via `COHERE_API_KEY`
**API family**: `cohere-chat`
**Supports**: Text input only (no image), reasoning, tool calling, streaming

---

## Native Adapter Providers

These providers require specialized protocol or auth handling beyond
generic OpenAI compatibility.

### Amazon Bedrock

```bash
# AWS credentials (IAM or SSO)
export AWS_ACCESS_KEY_ID="AKIA..."
export AWS_SECRET_ACCESS_KEY="..."
export AWS_SESSION_TOKEN="..."       # if using temporary credentials
export AWS_REGION="us-east-1"

# Or use bearer token
export AWS_BEARER_TOKEN_BEDROCK="..."

pi --provider amazon-bedrock --model anthropic.claude-sonnet-4-20250514-v1:0
# or with alias
pi --provider bedrock --model anthropic.claude-sonnet-4-20250514-v1:0
```

**Endpoint**: AWS regional endpoint (constructed from `AWS_REGION`)
**Auth**: AWS SigV4 via `AWS_ACCESS_KEY_ID`+`AWS_SECRET_ACCESS_KEY` or bearer token
**API family**: `bedrock-converse-stream`
**Aliases**: `amazon-bedrock`, `bedrock`

**Caveats**:
- No single base URL; endpoint is region-dependent
- Model IDs use Bedrock format (e.g., `anthropic.claude-sonnet-4-20250514-v1:0`)
- `AWS_PROFILE` also supported for named profile auth
- Text input only (no image passthrough)

### Azure OpenAI

```bash
export AZURE_OPENAI_API_KEY="..."

pi --provider azure-openai --model gpt-4o
# or with alias
pi --provider azure --model gpt-4o
```

**Auth**: API key via `AZURE_OPENAI_API_KEY`
**Aliases**: `azure-openai`, `azure`, `azure-cognitive-services`

**Caveats**:
- Requires deployment-specific endpoint configuration
- Endpoint format: `https://{resource}.openai.azure.com/openai/deployments/{deployment}/chat/completions?api-version={version}`
- Model ID maps to deployment name, not the OpenAI model ID
- Configure via `models.json` or `--base-url`:
```bash
pi --provider azure --model my-gpt4o-deployment \
  --base-url "https://my-resource.openai.azure.com/openai/deployments/my-gpt4o-deployment/chat/completions?api-version=2024-02-15-preview"
```

### SAP AI Core

```bash
# Using service key
export AICORE_SERVICE_KEY='{"clientid":"...","clientsecret":"...","url":"...","serviceurls":{"AI_API_URL":"..."}}'

# Or individual credentials
export SAP_AI_CORE_CLIENT_ID="..."
export SAP_AI_CORE_CLIENT_SECRET="..."
export SAP_AI_CORE_TOKEN_URL="https://..."
export SAP_AI_CORE_SERVICE_URL="https://..."

pi --provider sap-ai-core --model gpt-4o
# or with alias
pi --provider sap --model gpt-4o
```

**Auth**: OAuth2 client credentials via service key or individual env vars
**Aliases**: `sap-ai-core`, `sap`

**Caveats**:
- Requires SAP BTP subscription with AI Core service
- Token exchange happens automatically using client credentials
- Model availability depends on your SAP AI Core resource group configuration

### GitHub Copilot

```bash
export GITHUB_COPILOT_API_KEY="..."
# or
export GITHUB_TOKEN="ghp_..."

pi --provider github-copilot --model gpt-4o
# or with alias
pi --provider copilot --model gpt-4o
```

**Auth**: Token via `GITHUB_COPILOT_API_KEY` or `GITHUB_TOKEN`
**Aliases**: `github-copilot`, `copilot`, `github-copilot-enterprise`

**Caveats**:
- Requires active GitHub Copilot subscription
- Token exchange against GitHub API happens before each session
- Enterprise version has separate token handling

### GitLab Duo

```bash
export GITLAB_TOKEN="glpat-..."
# or
export GITLAB_API_KEY="..."

pi --provider gitlab --model claude-sonnet-4
# or with alias
pi --provider gitlab-duo --model claude-sonnet-4
```

**Auth**: Token via `GITLAB_TOKEN` (primary) or `GITLAB_API_KEY` (fallback)
**Aliases**: `gitlab`, `gitlab-duo`

**Caveats**:
- Endpoint is your GitLab instance URL (configure via `--base-url`)
- Returns non-streaming done event (streaming may behave differently)
- Model availability depends on your GitLab subscription tier

---

## OpenAI-Compatible Preset Providers (Flagship)

All of these use the `openai-completions` API family and route through
the OpenAI-compatible adapter. Set the provider-specific API key and go.

### Groq

```bash
export GROQ_API_KEY="gsk_..."

pi --provider groq --model llama-3.3-70b-versatile
```

**Endpoint**: `https://api.groq.com/openai/v1/chat/completions`
**Models**: llama-3.3-70b-versatile, llama-3.1-8b-instant, mixtral-8x7b-32768

**Caveats**:
- Temperature 0 normalized to 1e-8 server-side
- `logprobs`, `logit_bias`, `messages[].name` silently ignored

### DeepSeek

```bash
export DEEPSEEK_API_KEY="sk-..."

pi --provider deepseek --model deepseek-chat
```

**Endpoint**: `https://api.deepseek.com`
**Models**: deepseek-chat, deepseek-coder, deepseek-reasoner
**Context window**: 128,000 tokens

### Cerebras

```bash
export CEREBRAS_API_KEY="csk-..."

pi --provider cerebras --model llama-3.3-70b
```

**Endpoint**: `https://api.cerebras.ai/v1/chat/completions`
**Models**: llama-3.3-70b, llama-3.1-8b, qwen-3-32b

**Caveats**:
- Tool calling only on `gpt-oss-120b`, `qwen-3-32b`, `zai-glm-4.7`
- Non-standard rate limit headers (per-day and per-minute)

### OpenRouter

```bash
export OPENROUTER_API_KEY="sk-or-..."

pi --provider openrouter --model openai/gpt-4o-mini
```

**Endpoint**: `https://openrouter.ai/api/v1/chat/completions`

**Advanced**: Access any model via `provider/model` format:
```bash
pi --provider openrouter --model anthropic/claude-sonnet-4
pi --provider openrouter --model meta-llama/llama-3.3-70b-instruct
```

**Caveats**:
- Model IDs use `org/model` format
- Mid-stream errors use HTTP 200 + SSE error payload (not standard error codes)
- Serving model may differ from requested (fallback routing)

### OrcaRouter

```bash
export ORCAROUTER_API_KEY="sk-orca-..."

pi --provider orcarouter --model orcarouter/auto
```

**Endpoint**: `https://api.orcarouter.ai/v1/chat/completions`

**Advanced**: Access any model via `provider/model` format:
```bash
pi --provider orcarouter --model deepseek/deepseek-v4-pro
pi --provider orcarouter --model openai/gpt-4o-mini
```

**Caveats**:
- Model IDs use `org/model` format; `orcarouter/auto` routes to the gateway's default model
- Get an API key at [orcarouter.ai](https://www.orcarouter.ai)

### Mistral

```bash
export MISTRAL_API_KEY="..."

pi --provider mistral --model mistral-large-latest
```

**Endpoint**: `https://api.mistral.ai/v1/chat/completions`
**Models**: mistral-large-latest, mistral-medium-latest, open-mistral-7b

### Moonshot AI (Kimi)

```bash
# Either env var works
export MOONSHOT_API_KEY="sk-..."
# or
export KIMI_API_KEY="sk-..."

# Global endpoint
pi --provider moonshotai --model moonshot-v1-128k
# China endpoint
pi --provider moonshotai-cn --model moonshot-v1-128k
# Coding-focused (uses Anthropic API)
pi --provider kimi-for-coding --model kimi-k2.5
```

**Endpoint**: `https://api.moonshot.ai/v1/chat/completions` (global)
**Aliases**: `moonshotai`, `moonshot`, `kimi`

**Caveats**:
- Three separate entries: `moonshotai` (.ai global), `moonshotai-cn` (.cn China), `kimi-for-coding` (Anthropic API)
- Keys NOT interchangeable between `.ai` and `.cn` endpoints
- `kimi-for-coding` uses `anthropic-messages` API, not `openai-completions`
- Temperature range 0-1 (not 0-2 like OpenAI)

### Alibaba (Qwen / DashScope)

```bash
# Either env var works
export DASHSCOPE_API_KEY="sk-..."
# or
export QWEN_API_KEY="sk-..."

pi --provider alibaba --model qwen-plus
# or with alias
pi --provider qwen --model qwen-turbo
```

**Endpoint**: `https://dashscope-intl.aliyuncs.com/compatible-mode/v1/chat/completions`
**Aliases**: `alibaba`, `dashscope`, `qwen`
**China region**: Use `alibaba-cn` provider ID

**Caveats**:
- Tool calling CANNOT be combined with streaming
- Two distinct 429 categories: `qps` (retryable) vs `quota` (non-retryable)
- `QWEN_API_KEY` fallback only for `alibaba` (intl), not `alibaba-cn`

### Fireworks AI

```bash
export FIREWORKS_API_KEY="..."

pi --provider fireworks --model accounts/fireworks/models/llama-v3p1-70b-instruct
# or with alias
pi --provider fireworks-ai --model accounts/fireworks/models/llama-v3p1-70b-instruct
```

**Endpoint**: `https://api.fireworks.ai/inference/v1`
**Aliases**: `fireworks`, `fireworks-ai`

### Perplexity

```bash
export PERPLEXITY_API_KEY="pplx-..."

pi --provider perplexity --model sonar-pro
```

**Endpoint**: `https://api.perplexity.ai`
**Models**: sonar-pro, sonar, sonar-reasoning

### xAI (Grok)

```bash
export XAI_API_KEY="xai-..."

pi --provider xai --model grok-2
```

**Endpoint**: `https://api.x.ai/v1`
**Models**: grok-2, grok-2-mini

### Together AI

```bash
export TOGETHER_API_KEY="..."

pi --provider togetherai --model meta-llama/Meta-Llama-3.1-70B-Instruct-Turbo
```

**Endpoint**: `https://api.together.xyz/v1/chat/completions`

### DeepInfra

```bash
export DEEPINFRA_API_KEY="..."

pi --provider deepinfra --model meta-llama/Meta-Llama-3.1-70B-Instruct
```

**Endpoint**: `https://api.deepinfra.com/v1/openai/chat/completions`

---

## Regional and Specialized Presets

### NVIDIA

```bash
export NVIDIA_API_KEY="nvapi-..."

pi --provider nvidia --model meta/llama-3.1-70b-instruct
```

**Endpoint**: `https://integrate.api.nvidia.com/v1/chat/completions`

### Hugging Face

```bash
export HF_TOKEN="hf_..."

pi --provider huggingface --model meta-llama/Meta-Llama-3.1-70B-Instruct
```

**Endpoint**: `https://router.huggingface.co/v1/chat/completions`

### STACKIT (EU)

```bash
export STACKIT_API_KEY="..."

pi --provider stackit --model <model-id>
```

**Endpoint**: `https://api.openai-compat.model-serving.eu01.onstackit.cloud/v1/chat/completions`
**Note**: EU-hosted with data residency compliance.

### Ollama Cloud

```bash
export OLLAMA_API_KEY="..."

pi --provider ollama-cloud --model llama3.1:70b
```

**Endpoint**: `https://ollama.com/v1/chat/completions`

---

## Verification

After configuring any provider, verify it works:

```bash
# Quick smoke test
pi --provider <provider-id> --model <model-id> -m "Hello, respond with just OK"

# Expected: A response containing "OK" or similar acknowledgment
```

Common auth issues and their fixes: [provider-auth-troubleshooting.md](provider-auth-troubleshooting.md)

---

## Related Docs

- Auth troubleshooting: [provider-auth-troubleshooting.md](provider-auth-troubleshooting.md)
- Longtail evidence: [provider-longtail-evidence.md](provider-longtail-evidence.md)
- Onboarding playbook: [provider-onboarding-playbook.md](provider-onboarding-playbook.md)
- Provider metadata source: `src/provider_metadata.rs`
