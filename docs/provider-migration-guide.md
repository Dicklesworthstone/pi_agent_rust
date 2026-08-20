# Provider Selection and Migration Guide (bd-3uqg.11.12.3)

How to choose among providers and migrate configurations safely.

Generated: 2026-02-13

## Provider Selection Guide

### By Use Case

| Use Case | Recommended Provider | Why |
|---|---|---|
| General chat | openai, anthropic | Widest model selection, best quality |
| Fast inference | groq, cerebras | Hardware-accelerated, low latency |
| Cost-optimized | deepinfra, togetherai | Competitive pricing for open models |
| Open models | huggingface, nvidia, togetherai | Access to Llama, Mistral, etc. |
| Model aggregation | openrouter, orcarouter | Single API key for multiple providers |
| EU data residency | stackit | EU-hosted endpoints |
| Chinese models | alibaba (qwen), moonshotai (kimi) | Access to Qwen, Kimi models |
| Self-hosted | ollama (local) | Private, no data leaves your machine |
| Code-specialized | mistral | Codestral and code-optimized models |
| Coding agents | kimi-for-coding | Kimi K2.5 via Anthropic Messages API |

### By Priority

1. **Reliability**: anthropic, openai, google (native adapters, most tested)
2. **Performance**: groq, cerebras (purpose-built hardware)
3. **Flexibility**: openrouter (model marketplace, 300+ models)
4. **Cost**: deepinfra, togetherai, huggingface (open model hosting)
5. **Privacy**: ollama (fully local)

### Gap Provider Comparison Matrix

| Feature | Groq | Cerebras | OpenRouter | Kimi (moonshotai) | Qwen (alibaba) |
|---|:---:|:---:|:---:|:---:|:---:|
| API type | openai-completions | openai-completions | openai-completions | openai-completions | openai-completions |
| Tool calling | Yes | Selective (3 models) | Yes | Yes (K2+) | Selective |
| Streaming | Yes | Yes | Yes | Yes | Yes |
| Streaming + tools | Yes | Yes (non-reasoning) | Yes | Yes | No (older models) |
| Max context | 128K | 131K | Model-dependent | 262K | 1M |
| Temperature range | 0-2 | 0-1.5 | Model-dependent | 0-1 | 0-2 |
| `n` parameter | n=1 only | n=1 only | Yes | Yes | Limited |
| Parallel tool calls | Yes | Limited | Yes | Yes (K2+) | Yes |
| Rate limit free tier | 30 RPM | 30 RPM | Varies | 3 RPM | Varies |
| Region variants | No | No | No | .ai (global) / .cn (China) | intl / cn |

## Migration Between Providers

### Switching providers

All OpenAI-compatible providers share the same wire format. Switching
between them requires only changing the env var and provider flag:

```bash
# From Groq to Cerebras
# Before:
export GROQ_API_KEY="gsk_..."
pi --provider groq --model llama-3.3-70b-versatile

# After:
export CEREBRAS_API_KEY="csk-..."
pi --provider cerebras --model llama-3.3-70b
```

```bash
# From direct provider to OpenRouter
# Before:
export GROQ_API_KEY="gsk_..."
pi --provider groq --model llama-3.3-70b-versatile

# After:
export OPENROUTER_API_KEY="sk-or-v1-..."
pi --provider openrouter --model meta-llama/llama-3.3-70b-instruct
# Note: OpenRouter uses org/model format for model IDs
```

```bash
# Between Kimi regional endpoints
# Before (global):
export MOONSHOT_API_KEY="sk-global-key"
pi --provider moonshotai --model kimi-k2.5

# After (China):
export MOONSHOT_API_KEY="sk-china-key"  # Different key!
pi --provider moonshotai-cn --model kimi-k2.5
# WARNING: Keys are NOT interchangeable between .ai and .cn endpoints
```

### Model ID differences

Different providers use different model ID formats for the same model:

| Model | Groq | Cerebras | DeepInfra | Together AI | NVIDIA | OpenRouter |
|---|---|---|---|---|---|---|
| Llama 3.3 70B | llama-3.3-70b-versatile | llama-3.3-70b | meta-llama/Meta-Llama-3.3-70B-Instruct | meta-llama/Llama-3.3-70B-Instruct-Turbo | meta/llama-3.3-70b-instruct | meta-llama/llama-3.3-70b-instruct |
| Qwen 3 32B | -- | qwen-3-32b | Qwen/Qwen3-32B | Qwen/Qwen3-32B | -- | qwen/qwen3-32b |

### Migration safety checklist

Before switching providers, verify:

1. **Auth env var**: Each provider uses its own env var (`GROQ_API_KEY`, `CEREBRAS_API_KEY`, `OPENROUTER_API_KEY`, `MOONSHOT_API_KEY`, `DASHSCOPE_API_KEY`, etc.)
2. **Model ID**: Model names differ between providers (see table above)
3. **Tool calling support**: Not all providers/models support tool calling
   - Cerebras: Only `gpt-oss-120b`, `qwen-3-32b`, `zai-glm-4.7`
   - Qwen: Cannot combine streaming + tools on older models
   - Kimi: `tool_choice="required"` not supported
4. **Temperature range**: Clamp to provider limits
   - Groq: 0-2 (standard)
   - Cerebras: 0-1.5
   - Kimi: 0-1 (values >1 rejected)
   - Qwen: 0-2 (standard)
5. **Rate limits**: Check provider tier limits before heavy usage
6. **Regional endpoints**: Kimi (.ai vs .cn) and Qwen (intl vs cn) use non-interchangeable keys
7. **Unsupported parameters**: Some OpenAI parameters are silently ignored or rejected
   - Cerebras: `frequency_penalty`, `presence_penalty`, `logit_bias` return 400
   - Groq: `n`, `logprobs`, `logit_bias` silently ignored
   - OpenRouter: Unsupported params may be silently ignored by upstream

### Provider-specific migration caveats

**Migrating TO Groq**:
- `temperature=0` is normalized to `1e-8` server-side
- `n` must be 1 (multiple completions not supported)
- Messages `.name` field is silently ignored

**Migrating TO Cerebras**:
- Non-standard rate limit headers (`x-ratelimit-*-day`, `x-ratelimit-*-minute`)
- Response includes `time_info` (WSE timing data) — extra field, safe to ignore
- `frequency_penalty` and `presence_penalty` cause HTTP 400

**Migrating TO OpenRouter**:
- Model IDs require `org/model` format (e.g., `openai/gpt-4o-mini`, not `gpt-4o-mini`)
- Actual serving model may differ from requested (check `response.model`)
- Mid-stream errors arrive as SSE payload with `finish_reason='error'` (HTTP 200)
- SSE comment frames (`: OPENROUTER PROCESSING`) must be ignored

**Migrating TO Kimi (moonshotai)**:
- Three Pi entries: `moonshotai` (global), `moonshotai-cn` (China), `kimi-for-coding` (Anthropic API)
- Keys are NOT interchangeable between `.ai` and `.cn` endpoints
- `kimi-for-coding` uses `anthropic-messages` API, not `openai-completions`
- Temperature must be 0-1 (not 0-2)
- `tool_choice="required"` not supported — use `"auto"` instead

**Migrating TO Qwen (alibaba)**:
- Tool calling CANNOT be combined with streaming on older models
- Two distinct 429 error types: `qps` (retryable) vs `quota` (non-retryable)
- `system_fingerprint` returns null in current OpenAI-compatible examples; do not rely on it
- `logprobs` always returns null

## Related Docs

- Configuration examples: `docs/provider-config-examples.json`
- Auth troubleshooting: `docs/provider-auth-troubleshooting.md`
- Longtail evidence: `docs/provider-longtail-evidence.md`
- Per-provider setup docs: `docs/provider-{groq,cerebras,openrouter,kimi,qwen}-setup.json`
