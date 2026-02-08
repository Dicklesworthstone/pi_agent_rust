# Extension Runtime Compatibility Matrix

This document describes the Node.js and Bun API surface available to Pi
extensions running in the PiJS (QuickJS-based) runtime. Extension authors can
use this matrix to determine whether their extension will run unmodified.

---

## Conformance Snapshot

| Surface | Result | Details |
|---|---:|---|
| Extension corpus (223 extensions) | 205/223 pass (91.9%) | 100% Tier 1, 95.4% Tier 2 |
| Scenario conformance | 24/25 pass (96.0%) | Registration, events, tools, session |
| Node API matrix | 13/13 pass (100%) | All critical Node builtins covered |
| Bun API matrix | 5/7 pass (71.4%) | `connect`/`listen` unsupported |

---

## 1. Compatibility Tiers

| Tier | Description | Corpus coverage | Policy |
|------|---------|---:|--------|
| T1 | Simple single-file extensions | 38/38 (100%) | Run silently |
| T2 | Multi-registration (tools + events + flags) | 83/87 (95.4%) | Run silently |
| T3 | Multi-file with local imports | 79/90 (87.8%) | Run + warn if relative imports fail |
| T4 | Extensions with npm dependencies | 1/3 (33.3%) | Blocked unless virtual stub exists |
| T5 | Extensions using exec/network APIs | 4/5 (80.0%) | Capability-gated |

---

## 2. Node.js Builtin Modules

### Fully Supported

These modules are shimmed in PiJS and behave like their Node.js counterparts
for the API subset that extensions actually use.

| Module | Key APIs | Test coverage | Notes |
|--------|----------|---:|-------|
| `node:path` | `join`, `resolve`, `dirname`, `basename`, `extname`, `sep`, `posix`, `win32` | Full | POSIX semantics |
| `node:fs` | `readFileSync`, `writeFileSync`, `statSync`, `mkdirSync`, `readdirSync`, `unlinkSync`, `rmSync`, `copyFileSync`, `renameSync`, `appendFileSync`, `accessSync`, `existsSync`, `realpathSync` | 33 tests | Rooted to extension dir by default |
| `node:fs/promises` | `readFile`, `writeFile`, `stat`, `mkdir`, `readdir`, `unlink`, `rm`, `access`, `copyFile`, `rename` | Included above | Async versions of fs shim |
| `node:crypto` | `createHash`, `createHmac`, `randomUUID`, `randomBytes`, `randomInt`, `timingSafeEqual`, `getHashes` | 29 tests | SHA-256, SHA-512, SHA-1, MD5 |
| `node:buffer` | `Buffer.from`, `Buffer.alloc`, `Buffer.concat`, `Buffer.isBuffer`, `Buffer.byteLength`, `.toString()`, `.slice()`, `.subarray()`, `.compare()`, `.equals()`, `.indexOf()`, `.copy()` | 41 tests | Full Buffer protocol |
| `node:child_process` | `spawnSync`, `execSync`, `execFileSync`, `spawn`, `exec`, `execFile` | 53 tests | Capability-gated (`exec`) |
| `node:http` | `request`, `get`, `createServer`, `STATUS_CODES`, `METHODS`, `Agent` | 40 tests | `createServer` throws (sandbox) |
| `node:https` | `request`, `get` | Shared with http | Same as `node:http` |
| `node:events` | `EventEmitter`, `on`, `emit`, `once`, `removeListener`, `removeAllListeners`, `listenerCount` | 26 tests | Full EventEmitter pattern |
| `node:os` | `platform`, `hostname`, `tmpdir`, `homedir`, `cpus`, `arch`, `type`, `release`, `userInfo`, `EOL` | Included | Returns host values |
| `node:url` | `URL`, `URLSearchParams`, `parse`, `format`, `resolve` | 6 tests | WHATWG URL standard |
| `node:process` | `env`, `argv`, `cwd`, `exit`, `platform`, `arch`, `version`, `pid`, `hrtime` | 24 tests | `exit` is sandboxed |
| `node:util` | `format`, `inspect`, `inherits`, `deprecate`, `debuglog`, `types`, `TextEncoder`, `TextDecoder`, `stripVTControlCharacters` | Included | Standard utility functions |
| `node:stream` | `Readable`, `Writable`, `Transform`, `Duplex`, `PassThrough`, `pipeline`, `finished` | Included | Stream constructors + helpers |
| `node:stream/promises` | `pipeline`, `finished` | Included | Promise-based stream helpers |
| `node:querystring` | `parse`, `stringify`, `encode`, `decode` | Included | Query string utilities |
| `node:assert` | `ok`, `strictEqual`, `deepStrictEqual`, `throws`, `rejects`, `fail` | Included | Test assertion helpers |
| `node:string_decoder` | `StringDecoder` | Included | UTF-8 string decoding |
| `node:module` | `createRequire` | Included | Module system compat |

### Partially Supported

These modules expose a subset of their Node.js API. Missing functions throw
with a clear error message identifying the unsupported call.

| Module | Supported | Unsupported | Notes |
|--------|-----------|-------------|-------|
| `node:net` | `createConnection` (stub), `Socket` (stub) | `createServer` | Network listen blocked by sandbox |
| `node:readline` | `createInterface` (stub) | Interactive mode | Non-interactive only |

### Blocked

These modules are blocked because they require capabilities outside the
extension sandbox.

| Module | Reason | Alternative |
|--------|--------|-------------|
| `vm` | Arbitrary code execution | Use extension API directly |
| `worker_threads` | Thread creation | Single-threaded runtime |
| `cluster` | Process forking | Single-process runtime |
| `dgram` | Raw UDP sockets | Use `pi.http()` for network |
| `tls` | Raw TLS sockets | Use `node:https` instead |

---

## 3. Bun APIs

Pi provides a targeted Bun compatibility surface via `globalThis.Bun` and
`import "bun"`.

| API | Status | Notes |
|-----|--------|-------|
| `Bun.argv` | Supported | Process arguments |
| `Bun.file(path)` | Supported | Returns object with `exists()`, `text()`, `arrayBuffer()`, `json()` |
| `Bun.write(path, data)` | Supported | Write files |
| `Bun.which(command)` | Supported | Locate executables on PATH |
| `Bun.spawn(...)` | Supported | Capability-gated (`exec`) |
| `Bun.connect(...)` | **Unsupported** | Use `pi.http()` or `node:http` instead |
| `Bun.listen(...)` | **Unsupported** | Server APIs are outside the sandbox |

---

## 4. Virtual npm Module Stubs

Extensions that import popular npm packages get virtual stubs that expose the
package's public API shape. These stubs allow extensions to load and register
without runtime errors, even when the real package is not installed.

### Pi Framework Modules

| Package | Key exports |
|---------|-------------|
| `@mariozechner/pi-coding-agent` | `ExtensionAPI`, `Tool`, `SlashCommand`, `EventHook` |
| `@mariozechner/pi-ai` | `AI`, `Message`, `StreamEvent` |
| `@mariozechner/pi-tui` | `TUI`, `Widget`, `Layout` |
| `@sinclair/typebox` | `Type`, `Static`, `TSchema` |

### Protocol and Framework Modules

| Package | Key exports |
|---------|-------------|
| `@modelcontextprotocol/sdk/*` | MCP client/server/transport types |
| `vscode-languageserver-protocol/*` | LSP types and protocol definitions |
| `jsonwebtoken` | `sign`, `verify`, `decode` |
| `uuid` | `v4`, `v5`, `v7`, `NIL` |
| `dotenv` | `config`, `parse` |
| `shell-quote` | `parse`, `quote` |
| `ms` | Duration parsing |
| `diff` | `diffChars`, `diffLines`, `createPatch` |
| `glob` | `glob`, `globSync` |

### Terminal and UI Modules

| Package | Key exports |
|---------|-------------|
| `node-pty` | `spawn` (returns PTY stub) |
| `chokidar` | `watch` (returns watcher stub) |
| `@xterm/headless` | `Terminal` |
| `@xterm/addon-serialize` | `SerializeAddon` |
| `turndown` | `TurndownService` |
| `turndown-plugin-gfm` | `gfm`, `tables`, `strikethrough` |
| `@mozilla/readability` | `Readability`, `isProbablyReaderable` |
| `beautiful-mermaid` | `render` |
| `jsdom` | `JSDOM` |

### Observability Modules

| Package | Key exports |
|---------|-------------|
| `@opentelemetry/api` | `trace`, `context`, `propagation`, `SpanStatusCode` |
| `@opentelemetry/sdk-trace-base` | `BasicTracerProvider`, `SimpleSpanProcessor` |
| `@opentelemetry/resources` | `Resource` |
| `@opentelemetry/exporter-trace-otlp-http` | `OTLPTraceExporter` |
| `@opentelemetry/semantic-conventions` | `SEMRESATTRS_*` constants |

---

## 5. Extension API Surface (Pi Protocol)

The core Pi extension API is fully supported. This is the primary API that
extensions use.

### Registration

```javascript
export default function activate(pi) {
  // Register tools
  pi.tool({ name: "my-tool", description: "...", schema: {}, run: async (input) => { ... } });

  // Register slash commands
  pi.slashCommand({ name: "/my-cmd", description: "...", run: async (args) => { ... } });

  // Register event hooks
  pi.on("onMessage", async (event) => { ... });
  pi.on("onToolResult", async (event) => { ... });

  // Register flags
  pi.flag({ name: "my-flag", description: "...", default: false });

  // Register shortcuts
  pi.shortcut({ name: "my-shortcut", key: "ctrl+k", run: async () => { ... } });

  // Register providers
  pi.registerProvider({ name: "my-provider", models: [...], streamSimple: async (model, context) => { ... } });
}
```

### Session and State APIs

| API | Description |
|-----|-------------|
| `pi.session.getState()` | Get current session state |
| `pi.session.getMessages()` | Get conversation messages |
| `pi.session.getName()` / `setName()` | Session name |
| `pi.session.getModel()` / `setModel()` | Active model |
| `pi.session.setLabel(key, value)` | Set status line labels |
| `pi.session.getThinkingLevel()` / `setThinkingLevel()` | Thinking mode |
| `pi.events(op, payload)` | Dispatch lifecycle events |

### Host Tools

| API | Description |
|-----|-------------|
| `pi.tool(name, input)` | Call built-in tools (read/write/edit/bash/grep/glob/ls) |
| `pi.exec(command, args, options)` | Run commands (capability-gated) |
| `pi.http(request)` | HTTP client (policy-controlled) |
| `pi.log({level, event, message})` | Structured logging |

---

## 6. Capability Policies

Extension access to sensitive APIs is governed by capability policies.

| Policy | `exec` | `http` | `fs` (outside root) | `env` |
|--------|--------|--------|---------------------|-------|
| `safe` | Denied | Denied | Denied | Denied |
| `balanced` (default) | Allowed | Allowed | Warned | Allowed |
| `permissive` | Allowed | Allowed | Allowed | Allowed |

Use `pi doctor <path> --policy <profile>` to check extension compatibility
under a specific policy.

---

## 7. Preflight Analysis

The `pi doctor` command performs static analysis on extensions before loading:

```bash
# Text output (default)
pi doctor /path/to/extension

# JSON for automation
pi doctor /path/to/extension --format json

# Markdown for documentation
pi doctor /path/to/extension --format markdown

# Check against specific policy
pi doctor /path/to/extension --policy safe
```

The report includes:
- **Verdict**: PASS / WARN / FAIL
- **Confidence score**: 0-100 numeric rating
- **Risk banner**: Human-readable summary
- **Findings**: Per-issue category, severity, message, and line number

---

## 8. Known Limitations

### Module Resolution

- **Bare package specifiers** (`import foo from "some-package"`) require a
  virtual stub entry. Extensions using unlisted npm packages will fail to load.
- **Relative imports** (`import ./utils`) work within bundled extensions but may
  fail for multi-file extensions that were not bundled.
- **Network imports** (`import "https://..."`) are rejected.

### Runtime Constraints

- **Single-threaded**: No `worker_threads`, no parallel execution.
- **No native addons**: C/C++ Node addons cannot be loaded. Use hostcalls or
  WASM instead.
- **Sandbox boundary**: `createServer`, `listen`, and other server-side APIs are
  blocked. Extensions are clients, not servers.
- **Filesystem scope**: By default, filesystem access is scoped to the extension
  directory. Reads outside require explicit capability grants.

### Remaining Failure Buckets (18/223)

| Category | Count | Root cause |
|----------|------:|------------|
| Multi-file relative specifiers | 4 | Unbundled multi-file extensions |
| Package module specifiers | 5 | npm packages without virtual stubs |
| Host-read policy denials | 4 | Read access outside extension root |
| Runtime shape/load errors | 4 | Extension structure mismatch |
| Test fixture artifacts | 1 | Not a real extension |

---

## 9. Verifying Compatibility

### For Extension Authors

```bash
# Check your extension
pi doctor /path/to/your-extension

# Check with strict policy
pi doctor /path/to/your-extension --policy safe

# View supported policy modes
pi --explain-extension-policy
```

### For CI Integration

```bash
# JSON output for automated checks
pi doctor /path/to/extension --format json | jq '.verdict'

# Exit code is always 0 (verdict is in output, not exit code)
# Parse the JSON verdict field for pass/fail decisions
```

---

## 10. Reporting Issues

If your extension fails to load and you believe it should be compatible:

1. Run `pi doctor /path/to/extension --format json` and capture the output.
2. Check the findings array for specific error messages.
3. If the failure is a missing module stub or shim gap, it may be eligible for
   a fix in the PiJS runtime.
