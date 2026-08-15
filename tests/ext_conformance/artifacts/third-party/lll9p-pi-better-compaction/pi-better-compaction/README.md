# pi-better-compaction

A Pi extension that upgrades compaction with two coordinated strategies:

1. **OpenAI Responses APIs** (`openai-responses`, `openai-codex-responses`) use the provider's
   native `/responses/compact` endpoint, then replay the opaque compacted window on later
   requests without patching Pi core.
2. **Every other API** (Anthropic, Gemini, etc.) runs Pi's own native compaction method,
   optionally driven by a **dedicated compaction model** so you can summarize with a cheaper/faster
   model than the one you are chatting with.

Everything fails open: if any step cannot proceed, the extension returns control to Pi's default
compaction so a compaction never breaks because of this extension.

## Requirements

- **Minimum Pi version:** `@earendil-works/pi-coding-agent >= 0.80.0`

This extension relies on `modelRegistry.getApiKeyAndHeaders(model)` and the exported native
`compact()` function.

## Behavior

The `session_before_compact` decision tree:

```
session_before_compact
│
├─ config.enabled == false ───────────────────────► Pi default compaction
│
├─ current model API is a Responses API
│   │  (openai-responses / openai-codex-responses, narrowable via config)
│   ├─ POST /responses/compact
│   │   ├─ success ─────────────────────────────────► store opaque window + real summary
│   │   ├─ user aborted ─────────────────────────────► cancel
│   │   └─ failure (404 / network / malformed) ──────► fall through ▼
│   └─ (missing base URL / API key) ─────────────────► fall through ▼
│
├─ config.compactionModel is set and resolvable and ≠ current model
│   └─ run Pi's native compact() with that model ────► use its result
│
└─ otherwise ─────────────────────────────────────► Pi default compaction
      (no model configured, or it equals the current model — Pi runs the
       same native method itself, keeping its streaming progress UI)
```

On the next supported Responses request after a native `/responses/compact`, the
`before_provider_request` hook rewrites Pi's summary-oriented replay into:

- fresh current prompt envelope
- stored opaque compacted window
- live post-compaction tail

Requests produced by the native-method fallback carry Pi's standard `{readFiles, modifiedFiles}`
details, so they replay through Pi's default path — no rewrite, no special handling.

By default, a session whose latest compaction is not a stored native opaque window does not attempt
`/responses/compact`, preserving strict replay continuity. Set `allowCompactionContinuityBreak` to
`true` to restart native compaction from Pi's current effective context instead. Any information
already omitted by Pi's text summary cannot be recovered, but the successful compact response
becomes the new opaque replay checkpoint for later turns.

### Selection is by API, not provider

Any provider speaking a Responses API gets a native compact attempt, including OpenAI-compatible
proxies with a custom `baseUrl`. If such an endpoint does not implement `/responses/compact`, the
request 404s and the extension fails through to the configured fallback model (or Pi default). To
avoid the probe entirely for one API, narrow `responsesCompactApis`.

## Install

From npm (recommended):

```bash
pi install npm:@lll9p/pi-better-compaction
```

Try it for a single run without installing:

```bash
pi -e npm:@lll9p/pi-better-compaction
```

From a checkout (development):

```bash
git clone https://github.com/lll9p/pi-better-compaction.git
cd pi-better-compaction
pi install .
```

After installation, run `/reload`.

## Configuration

Single source, merged over built-in defaults:

```
~/.pi/agent/extensions/pi-better-compaction/config.json
```

A missing file silently uses the defaults below. The extension never writes this file for you.

```json
{
  "enabled": true,
  "allowCompactionContinuityBreak": false,

  "compactionModel": "openai/gpt-5.1-mini",
  "compactionThinkingLevel": "off",

  "responsesCompactApis": ["openai-responses", "openai-codex-responses"],

  "notifyOnLoad": false,
  "debug": false,
  "logProviderPayloads": false,
  "logCompactResponses": false,
  "redactSensitiveData": true,
  "artifactRoot": "~/.pi/agent/artifacts/pi-better-compaction"
}
```

| Key | Default | Description |
|-----|---------|-------------|
| `enabled` | `true` | Master switch. `false` → Pi default compaction everywhere. |
| `allowCompactionContinuityBreak` | `false` | Opt in to native compact recovery when the latest session compaction was created by Pi or another non-native path. The extension serializes `buildSessionContext().messages`, including Pi's text summary, into a new `/responses/compact` request. This sacrifices strict opaque-window continuity at that boundary; a successful response establishes a new native replay chain. |
| `compactionModel` | *(unset)* | `"provider/model-id"` used for native-method fallback (non-Responses APIs, or when the compact endpoint fails). `null`/unset → use the current model via Pi's default path. The provider is split on the first `/`, so model ids may contain slashes (e.g. `"openrouter/deepseek/deepseek-chat"`). |
| `compactionThinkingLevel` | `"off"` | Thinking level passed to the native `compact()` fallback. One of `off, minimal, low, medium, high, xhigh, max`. |
| `responsesCompactApis` | both Responses APIs | Which Responses APIs use the compact endpoint. May only narrow the built-in set; unknown entries are ignored with a warning. |
| `notifyOnLoad` | `false` | Show a load notification in the TUI. |
| `debug` | `false` | Write lifecycle + compaction-event artifacts. |
| `logProviderPayloads` | `false` | Write `before_provider_request` payload artifacts. |
| `logCompactResponses` | `false` | Write compact endpoint request/response artifacts. |
| `redactSensitiveData` | `true` | Redact secrets in artifacts. Keep on. |
| `artifactRoot` | `~/.pi/agent/artifacts/pi-better-compaction` | Debug artifact root. `~/` and relative paths (resolved against the config dir) are supported. |

### Codex-aligned compact request

For Responses compaction, the extension mirrors the latest codex_rs `CompactionInput` fields
(`tools`, `parallel_tool_calls`, `reasoning`, `service_tier`, `prompt_cache_key`, `text`) by
capturing them from the most recent live provider request for the same model/session and attaching
them to the compact request body. When no such request has been seen yet, the compact request falls
back to the minimal `model` / `input` / `instructions` body.

## Debug artifacts

Written per session under:

```
<artifactRoot>/sessions/<session-id>/
├── provider-requests/
├── compact-responses/
├── compaction-events/
└── lifecycle/
```

Troubleshooting flow:

1. set `debug: true` and `logCompactResponses: true` (keep `redactSensitiveData: true`)
2. `/reload`
3. run `/compact`, then send a follow-up message
4. inspect the newest artifact in the session directory

## Package structure

```
package-root/
├── index.ts                     # entrypoint declared in package.json
├── src/
│   ├── extension-runtime.ts     # hook registration + compaction decision tree
│   ├── config.ts                # config.json loader (single source + defaults)
│   ├── runtime.ts               # API-based environment resolution + auth
│   ├── compact-client.ts        # /responses/compact client + summary extraction
│   ├── native-fallback.ts       # configured-model native compact() driver
│   ├── request-context-cache.ts # captures codex-aligned fields from live requests
│   ├── serializer.ts            # Responses input serialization
│   ├── payload-rewrite.ts       # native opaque-window replay rewrite
│   ├── details-store.ts         # latest-valid native compaction lookup
│   ├── debug.ts                 # artifact writing + redaction
│   ├── supported-environment.ts # re-exports
│   └── types.ts                 # config + persisted native-compaction types
└── test/
```

## Tests

```bash
bun test
bun test --coverage --coverage-reporter=text --coverage-reporter=lcov
bun test ./test/pi-smoke.test.ts
```
