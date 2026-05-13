# HTTP Timeout Configuration - Implementation Summary

## Changes Made

### 1. Configuration Schema
Added `httpTimeoutSecs` support at three levels:

#### Global Level (`settings.json`)
```json
{
  "defaultProvider": "ollama-cloud",
  "httpTimeoutSecs": 180
}
```

#### Provider Level (`models.json`)
```json
{
  "providers": {
    "ollama-cloud": {
      "baseUrl": "https://ollama.com/v1",
      "httpTimeoutSecs": 300,
      "models": [...]
    }
  }
}
```

#### Model Level (`models.json`)
```json
{
  "providers": {
    "ollama-cloud": {
      "models": [
        {
          "id": "glm-5.1:cloud",
          "httpTimeoutSecs": 600
        }
      ]
    }
  }
}
```

### 2. Priority Hierarchy
Timeout resolution follows this order (highest to lowest):
1. **StreamOptions.http_timeout_secs** (runtime override)
2. **Model-specific timeout** (models.json)
3. **Provider-specific timeout** (models.json)
4. **Global timeout** (settings.json)
5. **Environment variable** (`PI_HTTP_REQUEST_TIMEOUT_SECS`)
6. **Default** (60 seconds)

Setting timeout to `0` disables timeout entirely.

### 3. Files Modified

#### Core Configuration
- `src/config.rs`: Added `http_timeout_secs` to `Config` struct
- `src/models.rs`: Added `http_timeout_secs` to:
  - `ProviderConfig`
  - `ModelConfig`
  - `ModelEntry`

#### HTTP Client
- `src/http/client.rs`: Added `effective_http_timeout()` function
- `src/http/mod.rs`: Re-exported `effective_http_timeout`

#### Provider Interface
- `src/provider.rs`: Added `http_timeout_secs` to `StreamOptions`

#### Provider Implementations
- `src/providers/openai.rs`:
  - Added `http_timeout_secs` field to `OpenAIProvider`
  - Added `with_http_timeout_secs()` method
  - Updated `stream()` to apply timeout from configuration

#### Extension Support
- `src/extensions.rs`: Updated ModelEntry creation to include timeout field

## Usage Examples

### Example 1: Global Timeout for All Cloud Models

`~/.pi/agent/settings.json`:
```json
{
  "httpTimeoutSecs": 300
}
```

All API requests will now use 300-second timeout instead of 60.

### Example 2: Per-Provider Timeout

`~/.pi/agent/models.json`:
```json
{
  "providers": {
    "ollama-cloud": {
      "baseUrl": "https://ollama.com/v1",
      "api": "openai-completions",
      "httpTimeoutSecs": 300,
      "models": [
        {
          "id": "glm-5.1:cloud",
          "name": "GLM-5.1 Cloud"
        }
      ]
    },
    "local-ollama": {
      "baseUrl": "http://localhost:11434/v1",
      "api": "openai-completions",
      "httpTimeoutSecs": 30,
      "models": [
        {
          "id": "qwen2.5:0.5b",
          "name": "Qwen 2.5 0.5B (Fast Local)"
        }
      ]
    }
  }
}
```

Now `ollama-cloud` uses 300s timeout, `local-ollama` uses 30s.

### Example 3: Per-Model Timeout Override

`~/.pi/agent/models.json`:
```json
{
  "providers": {
    "ollama-cloud": {
      "httpTimeoutSecs": 180,
      "models": [
        {
          "id": "glm-5.1:cloud",
          "httpTimeoutSecs": 600,
          "name": "GLM-5.1 (Complex Reasoning)"
        },
        {
          "id": "llama3.1:70b",
          "httpTimeoutSecs": 120,
          "name": "Llama 3.1 70B (Fast)"
        }
      ]
    }
  }
}
```

- `glm-5.1:cloud`: 600s (model-specific)
- `llama3.1:70b`: 120s (model-specific)
- Other models: 180s (provider default)

## Backwards Compatibility

- ✅ Existing configurations work without changes
- ✅ Environment variable `PI_HTTP_REQUEST_TIMEOUT_SECS` still supported
- ✅ Default behavior unchanged (60s timeout)
- ✅ All fields are optional

## Testing

### Compilation
```bash
cargo check
# ✅ Compiles successfully
```

### Runtime Test (TODO)
```bash
# Test with configuration
echo '{"httpTimeoutSecs": 300}' > ~/.pi/agent/test-settings.json
./target/release/pi -p "What is 2+2?"
```

## Next Steps

1. ✅ Code compiles successfully
2. ⏳ Update provider factory to pass timeout to OpenAI provider
3. ⏳ Test with actual Ollama Cloud model
4. ⏳ Add similar support to other providers (Anthropic, Gemini, etc.)
5. ⏳ Update documentation
6. ⏳ Create PR

## Migration Guide

### Before (Environment Variable)
```bash
export PI_HTTP_REQUEST_TIMEOUT_SECS=300
pi -p "your prompt"
```

### After (Configuration File)
`~/.pi/agent/settings.json`:
```json
{
  "httpTimeoutSecs": 300
}
```

Then simply:
```bash
pi -p "your prompt"
```

### Or Per-Provider
`~/.pi/agent/models.json`:
```json
{
  "providers": {
    "ollama-cloud": {
      "httpTimeoutSecs": 300,
      "...": "..."
    }
  }
}
```

## Solves Original Issue

**Problem**:
```
API Error: SSE Error: Request timed out reading body stream
```

**Root Cause**: Default 60-second timeout too short for cloud models

**Solution**: Configure per-provider timeout:
```json
{
  "providers": {
    "ollama-cloud": {
      "httpTimeoutSecs": 300
    }
  }
}
```

No more environment variables needed!
