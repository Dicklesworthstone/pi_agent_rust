# Legacy Pi Extension Compatibility (Best‑Effort)

This document maps the **legacy Pi extension API** to the new protocol and
defines compatibility tiers. The goal is **best‑effort parity** with clear,
actionable warnings where behavior diverges.

---

## 0. Reality Check: Legacy Extensions Do Not Need Node APIs

Legacy Pi extensions are *written in TS/JS*, but they mostly target the **Pi
ExtensionAPI** (`@mariozechner/pi-coding-agent`), not Node’s `fs`/`net`/etc.

In practice, the high-value extension behaviors are:
- register tools/commands/flags
- listen to Pi lifecycle events (turn/session/tool)
- read session state and emit custom entries/messages
- execute constrained commands (`pi.exec`) and use Pi tools (`read/write/edit/...`)

This means we do **not** need to embed Node/Bun to reach compatibility. We need a
Pi-shaped API surface with a small, capability-gated connector layer.

---

## 1. Compatibility Tiers

| Tier | Meaning | Action |
|------|---------|--------|
| Compatible | Matches expected behavior | Run silently |
| Warning | Works with constraints | Run + warn |
| Blocked | Unsafe/unsupported | Deny |

Default policy is **best‑effort** (Compatible/Warning run). `strict` mode blocks
Warnings unless explicitly approved.

---

## 2. Legacy API → Protocol Mapping

### Registration
Legacy:
- `registerExtension()` → New `register` message

Protocol payload:
```json
{
  "name": "my-extension",
  "version": "1.2.3",
  "api_version": "1.0",
  "capabilities": ["read", "http"],
  "tools": [...],
  "slash_commands": [...],
  "event_hooks": ["onMessage", "onToolResult"]
}
```

### Tools
Legacy:
- `tools: [{ name, description, schema, handler }]`

New:
- `tool_call` → `tool_result`

### Slash Commands
Legacy:
- `slashCommands: { "/foo": handler }`

New:
- `slash_command` → `slash_result`

### Event Hooks
Legacy:
- `onMessage`, `onToolResult`, `onSessionStart`, `onSessionEnd`

New:
- `event_hook` with `event` type + JSON payload

---

## 3. Unsupported / Degraded Features

| Feature | Status | Notes |
|---------|--------|-------|
| Custom TUI rendering | Blocked | Core UI remains Rust‑only |
| Node native addons | Blocked | Use hostcalls or WASM |
| Unbounded filesystem access | Warning/Blocked | Requires explicit capability |

---

## 4. Compatibility Scanner (Static + Dynamic)

**Static pass (SWC AST):**
- Detect forbidden imports (fs, child_process, net).
- Detect top‑level `eval`, dynamic `require`.
- Classify **compatibility tier**.

**Dynamic pass (runtime):**
- Hostcall access violations → downgrade to Warning/Blocked.
- Record in audit ledger for future decisions.

---

## 5. Hostcall Surface (Legacy Shim)

The compatibility layer provides a **Pi connector surface** (names illustrative):
- `pi.tool(name, input)` → call a built-in tool (`read/write/edit/bash/grep/find/ls`)
- `pi.exec(command, args, options)` → constrained process runner (timeout + cleanup)
- `pi.http(request)` → constrained HTTP client (policy-controlled)
- `pi.session.*` → session manager actions (append entry, set label/name, etc.)
- `pi.ui.*` → UI requests (notifications/widgets via core rendering)
- `pi.log({level, event, message, correlation, data?})` → structured logging (`ext.log.v1`)

Each hostcall is capability‑gated.

---

## 6. Best‑Effort Philosophy

If a legacy extension uses unsupported APIs:
1. Warn with **precise reasons**.
2. Attempt a fallback when safe.
3. Provide a migration hint.

> Best‑effort means “try hard,” not “pretend it worked.”
