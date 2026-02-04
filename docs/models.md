# Models Configuration

Pi loads available models from a built-in registry and an optional user-defined `models.json`.

## Location

| Path | Description |
|------|-------------|
| `~/.pi/agent/models.json` | User-defined model overrides and custom providers |

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
| `apiKey` | string | API key, env var name, or shell command (see Secret Resolution) |
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
      "api": "openai",
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
      "api": "openai",
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
