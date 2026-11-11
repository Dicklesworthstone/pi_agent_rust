# Extension Runtime Compatibility Matrix

This document describes the Node.js and Bun API surface available to Pi
extensions running in the PiJS (QuickJS-based) runtime. Extension authors can
use this matrix to determine whether their extension will run unmodified.

---

## Conformance Snapshot

These are the existing corpus results, not a new conformance run for the
context-mode compatibility changes described below. Adding a module or passing
a focused fixture does not establish compatibility with every npm dependency.

| Surface | Result | Details |
|---|---:|---|
| Extension corpus (223 extensions) | 205/223 pass (91.9%) | 100% Tier 1, 95.4% Tier 2 |
| Scenario conformance | 24/25 pass (96.0%) | Registration, events, tools, session |
| Node API matrix | 13/13 pass (100%) | All critical Node builtins covered |
| Bun API matrix | 7/7 pass (stubbed) | `connect`/`listen` stubbed (no network I/O) |

---

## 1. Compatibility Tiers

| Tier | Description | Corpus coverage | Policy |
|------|---------|---:|--------|
| T1 | Simple single-file extensions | 38/38 (100%) | Run silently |
| T2 | Multi-registration (tools + events + flags) | 83/87 (95.4%) | Run silently |
| T3 | Multi-file with local imports | 79/90 (87.8%) | Run + warn if relative imports fail |
| T4 | Extensions with npm dependencies | 1/3 (33.3%) | Resolution stays within allowed roots; compatibility depends on required APIs |
| T5 | Extensions using exec/network APIs | 4/5 (80.0%) | Capability-gated |

---

## 2. Node.js Builtin Modules

### Fully Supported

These modules are shimmed in PiJS and behave like their Node.js counterparts
for the API subset that extensions actually use.

| Module | Key APIs | Test coverage | Notes |
|--------|----------|---:|-------|
| `node:path` | `join`, `resolve`, `dirname`, `basename`, `extname`, `sep`, `posix`, `win32` | Full | Native Windows or POSIX behavior; both explicit namespaces available on every host |
| `node:fs` | `readFileSync`, `writeFileSync`, `statSync`, `mkdirSync`, `readdirSync`, `unlinkSync`, `rmSync`, `copyFileSync`, `renameSync`, `appendFileSync`, `accessSync`, `existsSync`, `realpathSync`, `readlinkSync`, `createReadStream`, `createWriteStream`, `chmodSync`, `chownSync` | 39 tests | Rooted to extension dir by default; permission changes are path-checking no-ops |
| `node:fs/promises` | `readFile`, `writeFile`, `stat`, `mkdir`, `readdir`, `unlink`, `rm`, `access`, `copyFile`, `rename`, `chmod`, `chown`, `utimes` | Included above | Async versions of fs shim; permission changes are path-checking no-ops |
| `node:crypto` | `createHash`, `createHmac`, `randomUUID`, `randomBytes`, `randomInt`, `timingSafeEqual`, `getHashes`, Ed25519 `sign`/`verify` | 56 tests | SHA-256/SHA-384/SHA-512, SHA-1, MD5, HMAC, KDFs; ciphers and RSA/ECDSA fail closed |
| `node:buffer` | `Buffer.from`, `Buffer.alloc`, `Buffer.concat`, `Buffer.isBuffer`, `Buffer.byteLength`, `.toString()`, `.slice()`, `.subarray()`, `.compare()`, `.equals()`, `.indexOf()`, `.copy()` | 41 tests | Full Buffer protocol |
| `node:child_process` | `spawnSync`, `execSync`, `execFileSync`, `spawn`, `exec`, `execFile` | Shim and persistent-session regressions | Capability-gated (`exec`); bounded piped sessions support interactive protocols |
| `node:http` | `request`, `get`, `createServer`, `STATUS_CODES`, `METHODS`, `Agent` | HTTP shim and virtual server regressions | Outbound requests use the HTTP capability; explicit loopback servers are private in-memory routes, never OS listeners |
| `node:https` | `request`, `get` | Shared with http | Same as `node:http` |
| `node:events` | `EventEmitter`, `on`, `emit`, `once`, `removeListener`, `removeAllListeners`, `listenerCount` | 26 tests | Full EventEmitter pattern |
| `node:os` | `platform`, `hostname`, `tmpdir`, `homedir`, `cpus`, `arch`, `type`, `release`, `userInfo`, `EOL` | Included | Returns host values |
| `node:url` | `URL`, `URLSearchParams`, `parse`, `format`, `resolve` | URL shim regressions | URL construction and base resolution use the native URL parser; the mutable JS surface remains bounded |
| `node:process` | `env`, `argv`, `cwd`, `exit`, `platform`, `arch`, `version`, `pid`, `hrtime` | 24 tests | `exit` is sandboxed |
| `node:util` | `format`, `inspect`, `inherits`, `deprecate`, `debuglog`, `types`, `TextEncoder`, `TextDecoder`, `stripVTControlCharacters` | Included | Standard utility functions |
| `node:stream` | `Readable`, `Writable`, `Transform`, `Duplex`, `PassThrough`, `pipeline`, `finished` | Included | Stream constructors + helpers |
| `node:stream/promises` | `pipeline`, `finished` | Included | Promise-based stream helpers |
| `node:querystring` | `parse`, `stringify`, `encode`, `decode` | Included | Query string utilities |
| `node:assert`, `node:assert/strict` | `ok`, `strictEqual`, `deepStrictEqual`, `throws`, `rejects`, `fail` | Included | Test assertion helpers |
| `node:string_decoder` | `StringDecoder` | Included | UTF-8 string decoding |
| `node:module` | `createRequire` | Included | Module system compat |

### Partially Supported

These modules expose a subset of their Node.js API. Missing functions throw
with a clear error message identifying the unsupported call.

| Module | Supported | Unsupported | Notes |
|--------|-----------|-------------|-------|
| `node:net` | `createConnection` (stub), `Socket` (stub) | `createServer` | Stubbed sockets (no network I/O); use `pi.http()` |
| `node:readline` | Stream-backed `createInterface`, line/close/error events, async iteration, `promises.createInterface` | Terminal editing and history | Reads UTF-8 protocol lines from supplied streams; console questions keep the `pi.ui('input')` fallback |
| `node:timers` and global timers | Timeout handles, shared cancellation, `ref`, `unref`, `hasRef`, `refresh`, `close`, numeric coercion | Control over the host process's lifetime | Deterministic host scheduler; handles keep their original owner and session provenance |

### Reading subprocess protocol lines

Extensions can consume a child's stdout using the ordinary nonterminal readline
pattern, including JSON-RPC and other newline-delimited protocols:

```javascript
import { createInterface } from "node:readline";

const lines = createInterface({
  input: child.stdout,
  terminal: false,
  crlfDelay: Infinity,
});
for await (const line of lines) {
  const message = JSON.parse(line);
  // Handle the protocol message.
}
```

The interface decodes UTF-8 across input chunks, recognizes LF, CR, and CRLF,
and emits a final unterminated line when input ends. `crlfDelay: Infinity`
keeps a CR/LF pair together even when separate chunks arrive far apart; the
default delay is 100 ms. Event-based consumers can use `on`, `once`, and
`removeListener`. Both `node:readline` and `node:readline/promises` support async
iteration. Ending input or calling `close()` emits `close` once and removes the
interface's input listeners. An input error also closes the interface and
rejects pending iterator reads. Leaving a `for await` loop closes the
interface; it does not destroy the supplied stream or terminate its child.
`pause()` and `resume()` forward to inputs that provide those methods.

A logical line is limited to 4 MiB of UTF-8 text. An active async iterator may
retain at most 1,024 lines totaling 4 MiB; inputs without pause support are
subject to the same bounds. Exceeding either bound reports
`ERR_PI_READLINE_LIMIT` and closes the interface. Event-only readers retain no
iterator backlog. These limits are separate from native subprocess output
limits and do not increase process or filesystem permissions.

Stream-backed questions write their prompt to the supplied output and consume
the next input line. Promise questions reject on abort or interface closure.
Questions without a readable input, including the host console facade, retain
the existing `pi.ui('input')` behavior and empty-string fallback when that UI
is unavailable. Terminal editing, completion, history, and raw-key handling
remain outside this interface.

### Timer handles and lifecycle

`setTimeout()` and `setInterval()` return opaque handles. Both cancellation
functions accept either kind of handle, including its numeric ID; interval IDs
cannot collide with unrelated timeout IDs. `refresh()` resets the deadline
without changing the handle's identity and can reactivate a fired timeout.
Explicit cancellation or runtime scrub prevents a handle from reactivating.
When available, `Symbol.dispose` cancels the handle as well.

`ref()`, `unref()`, and `hasRef()` track the handle's reference preference.
PiJS is embedded: the host actor controls its lifetime, so an unreferenced timer
still fires while that actor is running, and a referenced timer cannot keep a
shut-down actor alive. This permits context-mode's bounded shutdown fallback
without giving guest code control over the application lifecycle. Refreshes and
interval repeats retain the original Rust-captured session provenance, including
after a timeout has fired. Private state and scrub epochs prevent a retained
handle from rescheduling work after its runtime has been reset.

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
| `Bun.which(command)` | Supported | Metadata-only lookup in the runtime's configured PATH; requires `exec` permission |
| `Bun.spawn(...)` | Supported | Capability-gated (`exec`) |
| `bun:sqlite` | Supported subset | Real FrankenSQLite databases; ESM and `createRequire()` return the same `Database` constructor |
| `Bun.connect(...)` | Stubbed (no network) | In-memory socket emitter; use `pi.http()` or `node:http` for real network |
| `Bun.listen(...)` | Stubbed (no network) | In-memory server emitter; use `pi.http()` or `node:http` for real network |

### SQLite databases

`import { Database } from "bun:sqlite"` and
`createRequire(import.meta.url)("bun:sqlite").Database` use the repository's
existing FrankenSQLite engine. Supported operations include schema creation
and migration, parameterized statements, `run`, `get`, `all`, `values`,
`iterate`, nested synchronous transactions, WAL pragmas, and close/reopen.
`prepare()` parses schema and transaction statements using the engine's SQL
parser; their semantic checks happen when executed because the engine does not
compile those statements into prepared programs. Query and DML statements use
the engine's prepared programs.

Connections select the engine's standard SQLite single-writer mode before
accepting guest SQL. This applies to bare `BEGIN`, outermost `SAVEPOINT`, and
autocommit writes, including databases also opened by a context-mode MCP child.
The engine-specific concurrent-writer mode is not part of the Bun compatibility
contract: `BEGIN CONCURRENT` and pragmas that re-enable it are rejected during
admission. A separate-process stock SQLite regression covers shared-file
behavior when Node's SQLite module is available; selecting this mode alone
does not certify cross-engine interoperability.

`INSERT`, `UPDATE`, and `DELETE ... RETURNING` produce real result rows,
including the resume-state updates used by context-mode. A bridge row or
encoding-limit failure rolls back that statement's changes while preserving
earlier work in an outer transaction. Native constraint failures retain
SQLite's `ABORT`, `FAIL`, and `ROLLBACK` semantics.

Native SQLite errors retain their canonical `code`, extended `errno`, and
`primaryCode`. The message includes the canonical code so integrations such
as context-mode can recognize a busy database and retry. Host admission
failures remain separate; SQL text and filenames cannot impersonate native
error classifications. Nested transactions preserve the original error when
SQLite has already rolled back the entire transaction.

File-backed databases require an extension-owned persistent storage grant and
both read and write capabilities, including connections opened with
`readonly: true`. The native engine can create or repair coordination files
while opening an existing or copied database, before SQL execution begins.
The bridge checks these capabilities before entering the engine; `readonly`
still enforces SQL query-only behavior. `:memory:` databases require neither
filesystem capability. Database handles cannot be transferred to another
extension. SQL batches are admitted in full before their first statement runs;
SQL cannot acquire additional paths through `ATTACH`, `VACUUM INTO`, loadable
extensions, filesystem functions, arbitrary virtual-table modules, or writable
schema/directory pragmas. FTS5 is the only admitted virtual-table module.
Triggers and allocating scalar functions such as `randomblob`, `zeroblob`,
`printf`, and `format` are unsupported. Unsupported APIs and pragmas throw errors. In
particular, the optional `mmap_size` optimization used by context-mode is not
implemented.

Database replacement must also respect the engine's namespace identity records.
Recovery code that renames only the main database, WAL, and SHM files can leave
a nonempty FrankenSQLite identity record beside a missing main file. The engine
then refuses to recreate that database. The bridge does not automatically remove
these records: a safe replacement requires a guarded generation transition
before the old file is moved. This limits context-mode's corrupt-database
rename-and-retry recovery path.

The bridge bounds database handles, SQL/request size, returned rows and bytes,
page cache, and workers. Each synchronous request reserves 30 seconds for
processing and at most one additional 30-second native lock-wait allowance,
anchored to the original request start. The allowance uses the engine's actual
busy-timeout setting and its file-open/prepare bootstrap floor; batches and
retries cannot renew the resulting 60-second bound. This is a wall-time
reservation, not measured CPU time. Normal native busy errors remain retryable
on the same connection instead of colliding with the host deadline.

An expired request cancels and retires the worker, with a bounded 250 ms
acknowledgment grace. Retirement does not run an asynchronous close that could
flush retained writes. Cancellation remains cooperative, and a worker that
has not yet exited retains its process-wide permit. The embedding actor cannot
preempt this synchronous call with its own timer. These limits do not
provide an OS memory sandbox for every SQL intermediate allocation. The native
database-family size limit is checked before and after operations, so it is
not a hard per-write disk quota.

Database files and the engine's companion files are checked for symlinks,
reparse points, and hard links, with pinned parent identities rechecked around
operations. This includes the migration marker, pre-migration database/WAL/SHM
backups, their temporary files, the WAL/FEC lock, and Windows advisory locks for
the main database, WAL, rollback journal, and WAL certificates. Native startup or
maintenance can access these paths before guest SQL executes. They require the
same storage grant and contribute to the aggregate database-family size limit.
A link present before opening is rejected before the engine is entered; a
new link discovered on an existing handle retires that handle without flushing
retained writes. Guest filesystem APIs cannot create native links in a storage
grant. FrankenSQLite's native VFS still opens pathnames: on Unix, this does not
isolate the database from a separate native process with the same OS user
identity concurrently replacing directories. Applications requiring that
stronger boundary need an OS sandbox or a descriptor-based engine VFS.

### Persistent subprocess protocols

`spawn()` supports real piped stdin/stdout/stderr for protocols such as MCP,
including writes queued before admission, write callbacks, backpressure, EOF,
and process exit. Logical stdin writes up to the existing pending-byte budget
are segmented into bounded native chunks, with one callback after the entire
write completes. This supports large MCP requests without a partial write
when admission rejects an oversized request. Creation goes through the existing `exec` policy, command
mediation, and admission accounting. Native callbacks operate only on opaque,
extension-owned handles; they cannot launch another process or signal an
arbitrary PID. Input, output, session counts, environment size, and lifetime
are bounded. Shutdown retires the owned process group and retains admission
accounting until cleanup finishes.
Per-runtime process slots also remain reserved until the child and its I/O
workers actually finish, including after a handle is closed or startup fails.

The runtime services timers and subprocess callbacks while its command channel
is idle. Healthy extension realms receive turns in stable round-robin order;
each turn advances a scheduler step and at most one queued hostcall. A slow
hostcall retains its dispatch future across turns, preserving capability
checks, request ownership, and captured session provenance. A later command
continues polling that same work instead of launching it again. Lifecycle
commands take priority over idle work, and cancellation, replacement, and
shutdown drop pending dispatch state before acknowledging completion.

Application ownership is tracked separately from internal manager references.
Dropping the last public manager or runtime handle signals the actor even when
a pending hostcall or subprocess cleanup retains an internal manager. A public
runtime handle can outlive its manager. Shutdown uses a stop latch so a full
command queue cannot block the shutdown request before its cleanup budget.
Existing JavaScript instruction and microtask limits remain in force; idle
scheduling does not add a hard wall-time limit to synchronous guest code.

The runtime recognizes only the exact conventional executable probes
`command -v NAME` on POSIX and `where NAME` on Windows. It answers them using
file metadata and the configured PATH/PATHEXT, without running a shell.
General synchronous process execution retains its existing opt-in requirement.
Child environments are explicit and filtered for secrets and loader injection.
Spreading `process.env` copies only the owner-permitted environment snapshot,
in stable key order. Loader hooks (such as `NODE_OPTIONS` and `LD_PRELOAD`) and
invalid child bindings are omitted from enumeration, while individual reads
remain subject to environment disclosure policy. A secret disclosure allowlist
permits individual reads without enabling implicit secret forwarding; custom
secret rules also filter the inherited child snapshot. Explicitly supplying unsafe
child bindings is rejected. A global or per-extension environment denial
exposes no snapshot keys or values. Pi
advertises its configuration directory through `PI_CONFIG_DIR` metadata while
preserving an explicitly configured value. This lets child integrations
identify their host without receiving read access to a parent data directory.

`PWD` and `PI_WORKSPACE_DIR` are reserved workspace metadata. Both follow the
runtime's selected working directory, replacing stale values inherited from
the launcher. This keeps context-mode's extension and MCP child on the selected
project database when an SDK or resumed session uses a different directory.
Windows snapshots contain one canonical spelling of each key. An admitted
`PI_DETERMINISTIC_CWD` override follows the existing `process.cwd()` contract;
an override hidden by environment policy or SecretBroker is never copied into
these bindings. The metadata remains subject to environment disclosure policy
and grants no filesystem access. Unrelated explicit bindings are preserved.

For the provenance-validated `npm:context-mode` owner, the loader also prepares
the package's supported `CONTEXT_MODE_SESSION_SUFFIX` override before its
environment snapshot is created. It queries the real Git top-level directory
and worktree inventory, canonicalizes the roots, and supplies the same raw
eight-character hash that context-mode uses for a linked worktree. An empty
value is generated only when successful queries establish the main worktree.
The extension and its MCP child therefore use the same database filename even
though ordinary guest synchronous process execution remains disabled. An
explicit override, including an empty string, is preserved.

This preparation requires the owner's environment, read, and exec capabilities
to be allowed, plus normal SecretBroker disclosure of the suffix. It uses fixed
Git queries, existing command mediation, a trusted absolute Git executable,
and a clean noninteractive environment. The queries share a two-second budget
bounded by the original extension-load deadline; each output stream is limited
to 64 KiB, and native process ownership handles cancellation and cleanup.
Returned repository paths do not become filesystem grants. Unrelated packages
and ambiguous package owners do not trigger these queries.

Unavailable Git metadata leaves the override unset rather than asserting an
empty suffix. Automatic discovery accepts protected system Git installations;
custom installations can supply the explicit supported suffix override. This
is a session-start metadata snapshot, refreshed when the extension runtime is
reloaded, and does not enable any general synchronous subprocess API.

Context-mode's MCP server still needs a real Node or Bun executable on PATH.
Its process launch and environment snapshot require the existing `exec` and
`env` permissions; installing a package or granting storage does not enable
those capabilities. See the [extension security operator handbook](security/operator-handbook.md#dangerous-capability-opt-in)
for the existing opt-in and policy precedence rules.
An allowed external process runs with the host user's OS authority, as with
the existing `exec` capability; the extension's data-directory grants do not
constitute a sandbox for external executables.

---

## 4. Virtual npm Module Stubs

The CommonJS transform hoists only free `require()` calls. A locally bound
`createRequire()` or function parameter keeps its normal scope and lazy
execution, so an inactive optional native dependency does not prevent a Bun
SQLite extension from loading. CommonJS function and primitive exports retain
their actual values when consumed by another transformed module.

`createRequire()` and its `resolve()` method throw an error with
`code: "MODULE_NOT_FOUND"` for unsupported `node:*` modules and missing
denylisted system modules such as `dns`, `dgram`, and their subpaths. They use
the same sensitive-package denylist as the ESM resolver. Supported builtin
aliases still select their real PiJS implementations; the existing optional
utility-package stubs remain available. In particular, probing `node:sqlite`
now fails explicitly so an extension can choose the supported `bun:sqlite`
driver instead of receiving a callable no-op database.

File URL helpers preserve spaces, Unicode, and literal URL delimiters in
filenames, with explicit POSIX/Windows conversion and drive/UNC handling.
The resolver decodes file URLs before applying the existing import-root
policy; encoded separators and malformed encodings are rejected. Module
metadata and CommonJS filenames use the same conversion, including dynamic
imports built from `pathToFileURL()`.

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
| `jsonwebtoken` | `decode`, HS256/HS384/HS512 `sign`/`verify` |
| `uuid` | `v4`, `v5`, `v7`, `NIL` |
| `dotenv` | `config`, `parse` |
| `shell-quote` | `parse`, `quote` |
| `ms` | Duration parsing |
| `diff` | `diffChars`, `diffLines`, `createPatch` |
| `glob` | `glob`, `globSync` |
| `undici` | `fetch`, `Headers`, `Response`; explicit unsupported `Agent` constructor |

`jsonwebtoken` support is intentionally bounded to HMAC JWTs. RSA/ECDSA
algorithms and unsupported verification options fail closed with explicit
diagnostics instead of silently accepting tokens.

### pi-freeflow 1.39.0 and optional Undici dispatchers

The current pi-freeflow release imports `Agent` from `undici`, even though
connection pooling is optional. PiJS registers a narrow `undici` compatibility
module so this import resolves. Its fetch exports use the same capability-gated
transport as global `fetch`. PiJS does not advertise `process.versions.undici`:
Freeflow's version check therefore selects its built-in-fetch fallback.
Constructing an `Agent` or passing a custom fetch dispatcher fails explicitly;
there is no npm package loading, custom socket transport, or alternate TLS
authority behind this module.

Import resolution alone is insufficient for this extension. Freeflow also
starts a local proxy and registers an OpenAI-compatible provider against it.
PiJS implements `http.createServer(listener).listen(port, host)` as a private
in-memory route when `host` is explicitly `127.0.0.1`, `localhost`, or `::1`.
Routes belong to the extension that registered them. A provider registered by
that same extension can use its route through the existing native provider
request/response codecs; no real port is bound and other extensions cannot
intercept the route. Unsupported listen forms and non-loopback hosts fail.
Freeflow's detached daemon spawn options remain unsupported, allowing its
existing in-process fallback to run.

The provider keeps its originally granted extension/origin binding when that
extension closes and replaces the listener on the same port. Each new request
resolves the current owned listener; a provider that had no route when it was
registered cannot acquire one later without registration. URL construction
resolves root-relative health and control paths against the origin, allowing
Freeflow's health, client attachment, shutdown, and recovery operations to use
their intended endpoints.

Provider-level and model-level `compat` settings survive registration and merge
with model settings taking precedence. A model's top-level `thinkingLevelMap`
overrides the compatibility map. String values remap effort names; explicit
`null` values disable those tiers, while omitted keys retain existing defaults.
The native model picker and request serializers honor that distinction.
OpenAI-compatible providers may explicitly enable the plain `reasoning_effort`
field with `supportsReasoningEffort: true`; declared OpenRouter formats retain
their `reasoning` object. Undeclared providers keep their existing request shape.

Outbound fetch and Node HTTP calls still require the extension's HTTP
capability and the connector's TLS/host policy. Responses expose headers before
streaming body bytes; reader cancellation, request destruction, abort signals,
header deadlines, body idle timeouts, and runtime task deadlines retain owned
transport cleanup. `AbortSignal.any()` composes caller and timeout signals.
`Readable.fromWeb()` and `StringDecoder` preserve byte and split UTF-8 behavior.
JavaScript tasks awaiting deferred HTTP work yield to the native executor so
socket readiness, other tasks, and timers can progress within the root deadline.
Limits include 16 virtual servers and 64 active virtual requests per realm,
1 MiB of queued virtual response data, a 10 MiB virtual request body, and the
configured connector response limit. Custom TLS/socket overrides and streaming
fetch uploads are rejected rather than acquiring additional authority.

Regression coverage is in `tests/node_http_shim.rs`,
`tests/virtual_http_server.rs`, and the provider/runtime unit tests. The optional
`scripts/freeflow_compat_probe.mjs` reads the actual embedded JavaScript and
loads a pinned, clean upstream checkout at
`c4370df7fd4d186d00a33e4fc177ea340abf08d3` with controlled HTTP responses.
It covers the import, dispatcher fallback, activation, model metadata, relay
behavior, proxy streaming and recovery, health/control requests, and
Windows/POSIX path projections. The script documents setup and
usage. These V8-based diagnostics do not replace QuickJS/Rust execution or a
native Windows run; use `dsr quality --tool pi_agent_rust` for the authoritative
repository gate.

### Runtime API Compatibility Modules

| Package | Key exports |
|---------|-------------|
| `openai` | `OpenAI`, default `OpenAI`, `chat.completions.create` |
| `adm-zip` | default `AdmZip`, `getEntries`, `readAsText`, `extractAllTo`, `addFile`, `writeZip` |
| `linkedom` | `parseHTML` with document/window shape used by corpus extensions |
| `@sourcegraph/scip-typescript` | `scip.Index`, default `{ scip }` |
| `@sourcegraph/scip-typescript/dist/src/scip.js` | `scip.Index`, default `{ scip }` |
| `@sourcegraph/scip-typescript/dist/src/main.js` | `main`, `run`, default `main` |

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

### Deferred tool registration

The Agent publishes the runtime's validated tool catalog before each provider
schema snapshot. Tools registered after awaited initialization in
before_agent_start, as context-mode does after MCP tools/list, are therefore
available in that request and through the shared execution registry. Later
definition changes reach the next provider request; unchanged visible schemas
retain their cache version.

Registrations made by idle timers and subprocess callbacks update the same
validated snapshot used by discovery and execution routing. An invalid
registration quarantines its owner without blocking healthy extensions'
catalogs or event hooks. The last accepted names of a quarantined owner remain
reserved until unload, so another extension cannot take over its routes.
Quarantine cancels retained native requests and subprocesses and retires its
SQLite worker without an asynchronous close that could flush retained writes.
Resource permits remain held until the actual workers finish. Best-effort
provider-stream cleanup cannot publish new registrations from a failed owner.

Reconciliation retains inactive registrations for setActiveTools restoration
and removes only the exact wrappers it previously published. Initial active
extension overrides keep their existing behavior; automatic later refreshes
preserve host or MCP tools that have replaced an extension's name. An initially
inactive same-name extension does not displace a built-in. Internal catalog
wrappers do not retain their own actor's application lifetime through the
shared registry; explicitly retained public runtime handles keep their normal
ownership semantics.

### Startup readiness and event cancellation

`before_agent_start` can await external-service readiness and register tools
before the provider's tool schema is captured. The Agent gives each such
dispatch a default budget of **120,000 ms**, shared across all its handlers.
The runtime uses one deadline for queueing and the ordered handlers; moving
to another owner does not restart it. This applies on every turn that fires
the event, not only the first turn after opening a session.

Set `extensionStartupTimeoutMs` in an admitted settings source to shorten or
extend that budget:

```json
{
  "extensionStartupTimeoutMs": 120000
}
```

The setting accepts an unsigned integer in milliseconds and is clamped to
`1`–`300000`; the default is `120000`. Existing settings precedence and project
trust checks apply. It configures the Agent's `before_agent_start` dispatch,
not every extension event. Ordinary actionable hooks retain their five-second
default, informational hooks retain their shorter default, and compaction
keeps its dedicated budget. An explicit SDK or runtime dispatch timeout is
never enlarged by the startup default or this setting.

The same setting applies when extensions are prewarmed, and an embedding
manager's tighter budget still caps the wait. Cancellation in the Agent's
text, structured-content, and custom prompt paths returns before provider
invocation or admitting that prompt to conversation history.

This addresses a concrete context-mode startup requirement. Published
`context-mode@1.0.169` starts its MCP child lazily in `before_agent_start`,
awaits `initialize` and `tools/list`, and only then registers its tools. Its
handshake requests each allow 60 seconds, with bounded initialization retries.
A healthy six-second initialization therefore exceeded the former generic
five-second event budget. The dedicated startup budget permits that work while
remaining bounded. It does not promise to accommodate the package's complete
worst-case retry sequence, which can exceed 120 seconds.

Dropping an admitted event future or reaching its deadline signals the busy
actor directly. The currently executing extension owner is quarantined so a
late Promise continuation cannot publish registrations, and retained native
requests and subprocesses are retired. Other completed owners remain
available, and handlers which have not started are skipped for that failed
dispatch. A QuickJS interrupt fence also observes cancellation and deadlines
during guest CPU loops. Quarantine retires owned resources through the
existing cleanup paths and retains admission accounting until those resources
finish. Native synchronous host operations retain their own bounds; a
JavaScript interrupt cannot preempt Rust inside one of those operations.

Event-batch context creation shares the request's cancellation and deadline.
Batch cleanup skips already quarantined isolates and gives each remaining
owner's context deletion its own 100 ms execution budget. A failed or
over-budget cleanup quarantines that owner while cleanup continues for peers.

The supplementary `scripts/test_published_extension_rendering.mjs` diagnostic
now also pins and executes the complete published `extension.js` and
`mcp-bridge.js` modules. With a controlled MCP response and real Linux Node
timers, discovery and `session_start` spawned no child; the real startup hook
was still pending with no tools at five seconds, then registered the controlled
MCP tool after approximately six seconds. The package completed its actual
`initialize` → `notifications/initialized` → `tools/list` sequence and requested
`SIGTERM` on shutdown. All 24 checks pass with the current sources: 23 existing
rendering checks and this readiness case. With
`--startup-source /path/to/prior/extensions.rs` pointing to the prior
five-second default, the rendering checks pass and the readiness assertion
fails.

The timeout constant read from Rust is applied by a fixture observer in that
diagnostic. Imported dependencies, SQLite, the extension API and child I/O are
controlled fixtures; no native process is launched. These results establish
the published hook's readiness requirement, not native configuration loading,
timeout enforcement, cancellation, quarantine, catalog publication or Windows
behavior. Native event and settings regressions cover those request boundaries
and owner isolation, but remain unexecuted here pending the DSR native runner.

### Context message transforms

Context hooks receive the message array produced by earlier handlers, across
isolated extension realms and the direct/event-bus phases. Returning no value
preserves earlier changes, including in-place array mutations. Wrapped
`{ messages }` results and the existing bare-array form compose in order;
`{ messages: null }` explicitly clears the request context.

Context-mode appends ephemeral user messages without timestamps for routing,
active memory, and compaction resumes. At the Agent request boundary, an absent
user timestamp receives the original request's last message timestamp, or zero
for an empty request. This deterministic normalization preserves explicit
timestamps and does not repair malformed values or relax the persisted
message schema. Context transformations affect the provider request without
adding those injected messages to durable conversation history.

### Tool-result capture and transforms

`tool_result` events retain the existing flat `content`, `details`, and
`isError` fields and also expose `result: { content, details, isError }`.
Context-mode 1.0.169 reads this structured result to extract error text, facts,
and MCP responses for its persistent session history and compaction snapshot.
Without it, that adapter can record an error with empty text or miss facts in
otherwise successful tool output.

QuickJS handlers receive the accepted output from earlier handlers in the
existing direct-handler order, followed by event-bus order, including across
isolated realms. Each handler receives a fresh deep snapshot. Only explicitly
returned, validated flat `content` and `details` fields change the result;
in-place mutation, a void return, and null fields preserve prior accepted
values. A malformed patch is rejected as a whole without undoing earlier
transforms. The structured `result` view is rebuilt before the next handler,
so it cannot expose output that an earlier handler has replaced. The host
retains authority over `isError`; neither a returned error flag nor a returned
nested `result` object changes the execution outcome or the transform contract.

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

Persistent agent sessions reserve their eventual transcript filename before
extension factories, `startup`, and `session_start` run. The session state and
`ctx.sessionManager.getSessionFile()` expose that same filename before and after
the first save, so extensions that hash it for database keys retain their
history on cold resume. The reservation uses the host's effective working
directory and configured session backend and directory. It creates no session
directory or transcript; the normal validated save performs those writes.
Ephemeral (`--no-session`) sessions continue to expose `null`.

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

### Extension owner isolation

Before creating JavaScript realms, the loader validates the canonical source
of every entry, including entries expanded from package declarations. A shared
owner may combine entries explicitly declared by the same containing package.
An outer workspace declaration cannot merge entries across nested package
boundaries. A nested manifest containing only the module `type` setting does
not create a separate package. Undeclared loose entries retain their directory
boundary; those directly under an independent `extensions/` directory retain
individual source identities. Flat sibling files retain directory-based
filesystem access. Validated package roots keep the more specific entry
directories that protect one owner's private files from another owner.

If unrelated sources resolve to the same legacy extension ID, loading fails
before their factories run. The loader preserves existing IDs because
capability overrides, trust state, and resource limits use those exact keys.
A rejected replacement leaves the previously loaded extensions available.

### Persistent application state

Ordinary extension writes retain their existing virtual-filesystem behavior.
Persistent data directories are separate host-assigned capabilities and are
never added to the JavaScript module resolver's search roots.

For a verified installed npm extension, the loader can assign
`~/.pi/<package-name>` (or `~/.pi/@scope/<package-name>`). Verification requires
the installed `node_modules` path, manifest package name, and the declared
`pi.extensions` entrypoint to agree. Thus `npm:context-mode` can create and
reopen `~/.pi/context-mode/sessions` without receiving access to all of HOME or
all of `.pi`. Pi's own configuration/authentication directories and unsafe
namespace components are reserved. Shared legacy extension IDs across unrelated
sources are rejected before runtime construction. Ambiguous application
namespaces suppress automatic grants and emit diagnostics; unrelated extensions
retain their own grants. Invalid or linked automatic roots remain deny-only
boundaries.

Each owner's storage paths are resolved once using that owner's runtime
configuration. Every realm receives the same protected path boundaries before
extension factories run, including when environment permissions differ or a
configured home directory lies beneath the workspace.

The host embedding API `PiJsRuntime::add_persistent_storage_root` can assign an
explicit absolute directory to one extension. Guest code, package metadata,
and environment changes cannot add arbitrary paths. A custom
`CONTEXT_MODE_DATA_DIR` outside the assigned namespace therefore requires a
corresponding host grant.

Persistent reads and mutations check ownership and read/write policy. Mutations
use no-follow directory handles on Unix and pinned, reparse-checked directory
handles on Windows. Traversal, linked targets, Windows device paths/names,
alternate data streams, and overlapping grants to different owners are
rejected. Checks inspect original inputs and virtual symlink targets before
normalization can discard forbidden components. FIFOs, sockets, and device
targets are rejected before I/O. Unix final-file opens are nonblocking as well
as no-follow, so replacement with a FIFO after preflight cannot park the
runtime waiting for a pipe peer. Windows raw component validation runs before
canonicalization and includes reserved console names and superscript COM/LPT
device aliases. These directories support
durable mkdir/mkdtemp, file reads/writes/appends, copy, rename, unlink, and
empty-directory removal. Read-only descriptors belong to the extension that
opened them and can still be closed by that owner after a path is revoked.
`realpath` and `realpathSync.native` resolve authorized existing host paths,
including project aliases, with a fallback for genuine virtual files.
Writable descriptors,
symlink creation, recursive directory deletion, and cross-filesystem rename
are explicitly unsupported within a persistent grant.

### Read-only extension configuration

Configuration is a separate, exact-file capability in the host data registry.
The host API `PiJsRuntime::add_configuration_read_file` grants one owner a
read-only file and protects its parent namespace from other owners and ordinary
workspace fallback. It does not authorize directory listings, neighboring files,
JavaScript imports, or writes, even when the extension has a `write` capability.
Registration creates neither the configuration file nor its directories.

The npm loader currently declares this compatibility mapping:

| Admitted package identity | File relative to the owner's home |
|---|---|
| `@zhushanwen/pi-context-engineering` | `.pi/agent/extensions/context-engineering/config.json` |

The identity comes from the existing admitted package provenance: installed
`node_modules` layout, manifest name, and declared `pi.extensions` entry must
agree. Aliases are explicit host policy; arbitrary scopes and `pi-` prefixes
are never stripped to infer ownership. Guest package fields and changes to
`process.env` cannot add grants. Each path is resolved once in its owner's
runtime configuration, and all realms receive its protected boundary before
factories run. A collision with a different extension's code directory rejects
the proposed load before executing factories.

Configuration reads, stat, and realpath use the same pinned, no-follow file
handles and link checks as persistent data. Windows case aliases must identify
the actual granted file; case folding alone cannot authorize a distinct sibling
in a case-sensitive directory. Read-only configuration never satisfies SQLite's
write-authority requirement. Context-mode's existing writable
`~/.pi/context-mode` namespace remains separate.

An authorized absent configuration is reported with `error.code === "ENOENT"`.
Permission, unsafe-link, and I/O failures remain errors and cannot silently
become missing-file defaults. This matters for the published
`@zhushanwen/pi-context-engineering@0.2.0`, which reads configuration during
initialization and again on `session_start`, and checks `error.code` to select
its defaults.

#### Issue #279 verification scope (2026-11-04)

The supplementary diagnostic in
`scripts/test_published_extension_config.mjs` pins the published
`@zhushanwen/pi-context-engineering@0.2.0` source files and evaluates its actual
configuration loader and factory with the embedded PiJS JavaScript filesystem
facade. All 13 checks passed under Linux Node 24.22.1, both with POSIX paths and
with Windows path/platform projection. The same diagnostic against the prior
facade passed 6 checks and failed 7. Native host callbacks are fixtures in this
diagnostic: these results do not verify Rust authorization, QuickJS execution,
or a native Windows filesystem.

An independent Linux Node reference exercise of the unmodified
`context-mode@1.0.169` package completed initialization, session events,
compaction state, shutdown, and a cold-process SQLite reopen with the same
session identity. Its actual MCP server registered 11 tools and completed an
index/search round trip while the other package's configuration remained
readable and unchanged. This establishes the published package behavior to
exercise in PiJS; it is not evidence of a complete PiJS lifecycle.

Native regressions in `tests/extensions_fs_shim.rs` and
`tests/e2e_ts_extension_loading.rs` cover the exact reported configuration
layout, owner/provenance checks, missing files, policy denial, peer isolation,
read-only access, traversal, links, Windows aliases/junctions, and coexistence
with the existing context-mode SQLite/session reload fixture. The authoring
environment has no configured RCH worker or native Windows runner, so those
Rust tests have not been executed here. The unchanged DSR quality recipe
remains the required native validation path; published-package PiJS startup,
reload, MCP behavior, and Windows lifecycle remain unverified until that
runner evidence is available.

### Context usage for compression hooks

`ctx.getContextUsage()` is synchronous and returns a fresh
`{ tokens, contextWindow, percent }` snapshot when the host supplies usage.
The native agent supplies it to each `context` dispatch, before extensions
transform the request. It estimates the actual message list using the active
model's token counter, archive projection, and image policy. Hidden history,
sibling branches, and cumulative provider billing totals are not counted.
System-prompt and tool-schema overhead are not included in this estimate.

The snapshot belongs to that request; it never enters the cached session
context or a JavaScript global mirror. A later request gets a new estimate
even when the extension registry has not changed. Provider replacement clears
the old window until the owning session installs the new model metadata.
Standalone `Agent` embedders can supply that metadata through
`set_context_window_tokens`. A missing or zero window yields `undefined`.
Other events, commands, and minimal tool contexts currently return `undefined`
because they have no authoritative request snapshot. An explicitly supplied
snapshot with unknown tokens returns `tokens: null` and `percent: null`.

Only the three public numeric fields are exposed. The getter copies them and
calculates percent from tokens/window, so mutating one returned object cannot
change subsequent reads. Percent uses percentage points: 60,000 tokens in a
200,000-token window is `30`. Values over `100` remain visible as overflow.

This restores a critical path in the published
`@zhushanwen/pi-context-engineering@0.2.0`: its `context` handler calls
`getContextUsage()` unconditionally inside a catch block. The previously absent
method caused the handler to return `{}`, silently skipping all compression
after configuration loaded successfully. The published-package diagnostic also
exercises actual budget compression, recall, statistics, command toggles, and
session reset with the verbatim PiJS context builder.

Version `0.2.0` has a separate threshold-units defect: its emergency compression
compares `contextUsage.percent` directly with the default `emergencyThreshold`
of `0.9`. For a 90% threshold with the percentage API, configure that value as
`90`; the host does not change percentage units to compensate. This package
limitation is distinct from the missing-method failure.

The supplementary diagnostic runs under Node with controlled native callbacks;
all 20 checks pass with POSIX paths and Windows path/platform projection. The
same expanded diagnostic against the source before the usage fix passes the
original 13 checks and fails all seven new checks. It does not execute Rust,
QuickJS, or native Windows. Native request-path and
snapshot regressions cover fresh usage, window changes, unknown metadata,
post-compaction inputs, and isolation from later events. They require the DSR
native runner before their results can be claimed as executed verification.

### Tool progress and cancellation

JavaScript extension tools receive the standard argument order:

```typescript
execute(toolCallId, input, signal, onUpdate, ctx)
```

`signal` is a fresh `AbortSignal` for that invocation. `onUpdate`, when the
native caller supplies an observer, forwards `{ content, details }` snapshots
through the existing `ToolUpdate` and `ToolExecutionUpdate` path used by the
terminal interfaces and RPC. The final tool result stays separate. Native
descriptor tools keep their existing immediate-result behavior.

Progress belongs to a host-generated private invocation token, independently
of the public tool-call ID. Owner checks and secret-protected lifecycle controls
prevent guest code from selecting another invocation's destination. The host
validates each snapshot and caps its serialized UTF-8 size at 1,000,000 bytes.
One replaceable pending snapshot bounds the buffer: slow observers see the most
recent state rather than an unbounded backlog. Native observers run in the
awaiting caller, outside the JavaScript actor. Invalid progress is ignored;
updates after completion or cancellation are suppressed, and retained guest
callbacks release their native destination when the invocation closes.

Dropping an admitted tool future, including agent cancellation, sets an atomic
flag observed by the busy actor. It does not enqueue a cancellation request
behind the work being cancelled. Active tool waits poll a bounded collection
of retained native hostcalls, allowing a pending operation and a later operation
needed to release it to make progress. Existing capability checks, native
resource guards, session provenance, and per-extension routing remain in force.
When the runtime's safety envelope vetoes interleaving, new admission falls to
one retained hostcall. Previously admitted work continues to be polled without
being dropped, and further requests wait for the conservative limit.

Cancellation and the existing tool deadline close progress before dispatching
abort listeners. The runtime gives cooperative JavaScript cleanup a 100 ms
budget. A tool that remains unresolved is quarantined with its own isolate's
resources; other extension isolates remain available. A QuickJS interrupt
fence also checks cancellation and the deadline during guest CPU loops. An
interrupted isolate is quarantined even if an async function converts the
interruption into a rejected promise. Native synchronous host operations retain
their own timeout behavior; the JavaScript interrupt hook cannot preempt Rust
while it is inside one of those operations.

Cancellation is cooperative for resources: pass the signal to `fetch`, or use
an abort listener to stop a child process. The host does not infer ownership of
all background work from its timing and does not cancel every resource owned
by an extension that completes its abort handling. Hostcalls have extension and
session provenance, but not automatic task-local attribution across every
Promise continuation. A tool that settles while abandoning unrelated-looking
background work therefore does not cause that work to be cancelled implicitly.

#### FTUI live previews

FTUI displays an extension tool's text progress in its existing pending card.
Each accepted snapshot replaces the previous preview, including a shorter or
empty snapshot, and changes the card's render revision when its visible detail
changes. Progress leaves the card pending until the tool finishes.

The FTUI forwarder owns an operation scope tied to the displayed session.
Each tracked invocation gets a private presentation identity and a replaceable
mailbox. Public tool-call IDs remain opaque lookup keys; only displayed names,
summaries, and output are terminal-sanitized. The UI checks both operation
identity and session ownership before accepting a card or its progress.
Completion, operation retirement, and conversation reset close the relevant
mailboxes, so retained callbacks and queued messages cannot revive their
progress in a later operation.

Progress tracking admits at most **128 active tools** and remembers at most
**1,024 distinct call IDs per operation**. Call IDs and tool names used by that
tracker are limited to **4,096 bytes** each. Reused IDs, oversized keys, and
capacity exhaustion suppress live progress for the affected invocation while
preserving the existing lifecycle and terminal-result path.

Each mailbox keeps only its latest bounded text preview: up to **200 lines**,
with up to **300 Unicode characters per line**, plus truncation markers. The
preview is built directly from borrowed text blocks. Images and arbitrary
`details` objects are not copied into this presentation channel. Progress
updates enqueue no notifications or result payloads; ordered lifecycle messages
establish and finish the card. FTUI polls active mailboxes on its existing
**120 ms spinner heartbeat**, so updates between polls coalesce. This is a poll
cadence, not a guarantee that every intermediate snapshot appears on screen.
If terminal suspension or an external editor consumes and parks a tick,
returning terminal ownership rearms that heartbeat without adding another
chain when a timer is already pending.

The terminal result closes progress before its final preview is delivered and
replaces any partial text. A final result without text clears the partial
preview. Manual shell cards keep their existing behavior: a `BashResult` can
fold output into the card before a `ToolEnd` without a replacement preview.
The classic terminal and RPC retain their existing update consumers. Interim
`out://` replies become assistant prose only after successful completion;
provisional updates do not publish those replies.

These are generic text previews. Full JavaScript `renderCall` and
`renderResult` callbacks, custom tool components, and rich `details` rendering
remain unsupported in the FTUI tool-card path. Presentation buffering does
not change the tool result supplied to the model or persisted in the session.

#### Verification

The supplementary `scripts/pijs_tool_lifecycle_probe.mjs` diagnostic pins the
unchanged repository subagent example and runs the actual PiJS JavaScript
registration, dispatch, signal, and lifetime code in Node. Its **16 checks
pass**; the pre-lifecycle bridge at `152c13d` fails all 16. The real example
emits a shorter replacement snapshot in single mode, detached aggregate
snapshots in parallel mode, and a parallel terminal summary distinct from its
last progress message. It also invokes its child `SIGTERM` handler on abort.
Child processes, native sinks, and other host facilities are controlled
fixtures: this verifies JavaScript behavior at the bridge sink, not native
child termination, actor cancellation, FTUI delivery, or Windows. The bridge
at `c15db1a`, before the FTUI delivery change, also passes these 16 checks;
the source diagnostic does not establish the native UI fix.

Native regressions cover wrapper progress delivery, caller abandonment,
deadlines, late callbacks, mailbox bounds, hostcall scheduling, and isolation
after unresponsive tools. FTUI regressions cover bounded preview transport,
pending-card updates, replacement and final-output handling, and operation
and session isolation. They require the DSR native runner before their results
can be claimed as executed verification; native FTUI and Windows behavior
remain unexecuted here. The published
`context-mode@1.0.169` tool callback ignores Pi's signal and progress callback;
these host features alone do not establish cancellation or streamed progress
for that package.
The existing owner-scoped configuration, persistent-storage, SQLite, session
identity, and grapheme mechanisms are independent of invocation lifetime.

### Grapheme segmentation for extension rendering

PiJS supports locale-independent extended grapheme segmentation through
`new Intl.Segmenter(undefined, { granularity: "grapheme" })`; the default
granularity is also `grapheme`. The pure native helper uses the already-locked
`unicode-segmentation` 1.13.3 Unicode tables and returns UTF-16 boundaries in
one batch. It does not acquire filesystem, environment, process, or network
capabilities. JavaScript retains the original input, creates records lazily,
and gives each iterator its own cursor. `Segments.containing(index)` searches
the same boundaries, including when the index falls within a surrogate pair.
Unpaired surrogate code units remain intact in returned text.

This fixes a concrete failure in the published `context-mode@1.0.169` tool
renderer: the previous iterator recursively invoked itself and overflowed the
stack on nonempty output. Removing that recursion alone still split flags,
emoji modifiers, ZWJ families, Hangul Jamo, and Indic clusters during clipping.
The new boundaries keep those clusters together and report UTF-16 indices
rather than code-point counts.

This is a focused grapheme implementation, not complete locale-aware Intl
support. Word and sentence granularities are explicitly rejected with
`RangeError` instead of silently returning graphemes. Locale negotiation,
`supportedLocalesOf`, and `resolvedOptions` are not implemented for Segmenter.

The supplementary diagnostic
`scripts/test_published_extension_rendering.mjs` pins the unmodified published
renderer and runs it with both Node's native Intl and the actual PiJS JavaScript
facade in the same invocation. All 23 rendering checks pass with the updated
facade; the prior facade fails all 23. Cases include ANSI preservation,
clipping at grapheme boundaries, multiline rendering, independent iterators, UTF-16
indices, input coercion, and lone surrogates. Node supplies the boundary
callback in this diagnostic, so this result verifies the JavaScript facade
and published renderer, not the Rust segmentation implementation or QuickJS.

Native boundary, QuickJS callback, and full PiJS facade regressions are in
`src/extensions_js/intl.rs`. They remain unexecuted in this environment because
the required DSR remote Rust runner is unavailable. Native Windows rendering
and the complete published-package PiJS lifecycle remain unverified.

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

- **Bare package specifiers** (`import foo from "some-package"`) must resolve
  to an installed package within the allowed extension roots or a registered
  virtual module. Resolution does not supply native addons or complete Node/Bun
  API compatibility.
- **Relative imports** (`import ./utils`) work within bundled extensions but may
  fail for multi-file extensions that were not bundled.
- **Network imports** (`import "https://..."`) are rejected.

### Runtime Constraints

- **JavaScript execution**: No guest `worker_threads`; bounded native workers
  handle SQLite and subprocess I/O.
- **No native addons**: C/C++ Node addons cannot be loaded. Use hostcalls or
  WASM instead.
- **Sandbox boundary**: `createServer`, `listen`, and other server-side APIs are
  blocked. Extensions are clients, not servers.
- **Filesystem scope**: Extension/workspace access retains its existing policy.
  Persistent application namespaces are separate, owner-scoped data grants;
  they do not authorize arbitrary HOME access or native addon loading.
- **Compaction cancellation**: `compact()` / `ctx.compact()` called with a
  signal that is already aborted rejects with an `AbortError` before any host
  work starts. An abort that fires
  while the compaction is running is not propagated: no JavaScript abort
  handler runs until the compact hostcall returns. The dedicated
  `session_before_compact` budget (300 seconds) and the provider request
  timeout bound the call instead.

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
