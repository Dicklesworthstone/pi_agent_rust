# f-0007: Provider Reliability Layer

> Epic: [Rust Investment Roadmap](../epics/rust-investment-roadmap.md) — Item 7
> Priority: Medium | Effort: 1-2 weeks

---

## Problem

The Rust port has 11 independently implemented provider backends. Each
handles its own HTTP, SSE parsing, error mapping, and auth. There is no
shared reliability layer — if a provider returns 5xx or rate-limits, the
user sees a raw error.

## Current Providers

| Provider | File | LOC |
|----------|------|-----|
| Anthropic | `src/providers/anthropic.rs` | 2,998 |
| OpenAI (completions) | `src/providers/openai.rs` | 2,414 |
| OpenAI (responses) | `src/providers/openai_responses.rs` | 2,900 |
| Google Gemini | `src/providers/gemini.rs` | 2,219 |
| Azure OpenAI | `src/providers/azure.rs` | 1,733 |
| Cohere | `src/providers/cohere.rs` | 1,952 |
| AWS Bedrock | `src/providers/bedrock.rs` | 1,302 |
| Google Vertex | `src/providers/vertex.rs` | 1,085 |
| GitHub Copilot | `src/providers/copilot.rs` | 565 |
| GitLab | `src/providers/gitlab.rs` | 488 |
| Shared/mod | `src/providers/mod.rs` | 3,001 |

## Features

### 7a. Provider Health Monitoring

Track per-provider reliability metrics in memory.

**Metrics per provider:**
- Request count (total, last 5min)
- Error count by category (auth, rate_limit, server_error, timeout)
- Latency percentiles (P50, P95, P99) — rolling window
- Token throughput (tokens/sec for streaming)
- Last successful request timestamp

**Implementation:**
- Add `ProviderHealthTracker` to `src/providers/mod.rs`
- Each provider calls `tracker.record_request(duration, result)` after
  each API call
- Expose via `pi doctor` and status bar footer

### 7b. Automatic Failover

When a provider fails, try the next one.

**Configuration** (`~/.pi/config.json`):
```json
{
  "provider_failover": {
    "enabled": true,
    "chain": ["anthropic", "openai", "google"],
    "trigger_on": ["server_error", "rate_limit", "timeout"],
    "max_retries": 2,
    "cooldown_seconds": 60
  }
}
```

**Implementation:**
- Wrap provider dispatch in `src/agent.rs` with failover logic
- On triggering error: mark provider as degraded, try next in chain
- Cooldown: don't retry a failed provider for N seconds
- Notify user in TUI when failover occurs

### 7c. Rate Limit Handling

**Per-provider tracking:**
- Parse `Retry-After` header (Anthropic, OpenAI)
- Parse `X-RateLimit-Remaining` headers
- Exponential backoff with jitter: 1s, 2s, 4s, 8s (cap at 30s)

**Implementation:**
- Add `RateLimitState` to each provider's state
- Before request: check if in cooldown period
- After 429 response: parse headers, set cooldown timer
- Show countdown in TUI: "Rate limited, retrying in 8s..."

### 7d. Cost Tracking

**Per-session tracking:**
- Input tokens, output tokens, cached tokens per request
- Provider-specific pricing (configurable, with defaults)
- Running total displayed in session footer

**Implementation:**
- Add `CostTracker` to session state
- Parse token counts from provider responses (all providers return these)
- Pricing table in `src/providers/mod.rs` with override in config
- Display in TUI footer: `$0.42 | 12.3k tokens`

### 7e. VCR Golden Cassettes

`src/vcr.rs` exists for HTTP recording/replay. Create golden cassettes
for each provider's streaming format.

**Cassettes needed:**
- Normal streaming response (each provider)
- Rate limit response (429 + headers)
- Server error (500/503)
- Auth error (401/403)
- Partial stream (connection drop mid-response)

**Implementation:**
- Record cassettes against real provider APIs (one-time, manual)
- Store in `tests/fixtures/vcr/providers/`
- Run in CI with `VCR_MODE=playback`
- Validates SSE parsing, error handling, token counting

## Acceptance Criteria

- `pi doctor` shows per-provider health metrics
- Failover triggers automatically on 5xx with user notification
- Rate limit retries with visible countdown in TUI
- Session footer shows token count and estimated cost
- VCR cassettes exist for all 11 providers with at least success + error cases
