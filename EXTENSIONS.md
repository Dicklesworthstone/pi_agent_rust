# Extension System (Big‑Guns Plan)

This document defines the extension architecture for **pi_agent_rust** with the
goal of **maximum compatibility**, **formal safety guarantees**, and **measurable
performance**. The system is **best‑effort** by default, but designed to
converge to full parity with legacy Pi extensions.

---

## 0. Design Goals

1. **Compatibility**: run legacy Pi extensions with best‑effort fidelity.
2. **Performance**: <2ms p95 overhead per tool call (excluding tool work).
3. **Safety**: explicit, auditable capability grants with optional strict mode.
4. **Stability**: versioned protocol + conformance fixtures.
5. **Portability**: same artifact runs on Linux/macOS/Windows.

Non‑goals:
- Custom TUI rendering from extensions (core owns the UI).
- Node‑native addons (must use hostcalls or WASM).

---

## 1. Runtime Tiers (Hybrid, Best‑of‑All Worlds)

**Tier A — WASM Component (default):**
- Fast, sandboxed, portable.
- Typed hostcalls via WIT.

**Tier B — JS Compatibility (compiled):**
- Legacy TS/JS compiled to a single bundle.
- Pre‑compiled to **QuickJS bytecode** or **JS→WASM**.
- No Node runtime required.

**Tier C — MCP (process IPC):**
- For heavy integrations: IDEs, databases, cloud services.

> WASM is the default. JS compatibility is a **compile step**, not a runtime.

---

## 1A. Node/Bun‑Free Runtime: Connectors + Event Loop

Mario’s critique is correct in the narrow sense: **QuickJS is just a JS engine**.
It intentionally does **not** ship a Node/Bun‑style OS API surface or a full
general-purpose event loop.

Our answer is: **great** — we don’t want the Node/Bun surface area.

Instead, Pi provides a tiny, capability‑gated **connector layer** and an explicit
event loop that is *tailored to Pi’s extension needs* (not the entire web/Node
ecosystem).

### 1A.1 The “Connector” Model (Minimal OS Surface)

Extensions do not get raw OS access (no `fs`, no `child_process`, no arbitrary
sockets). They get a **small set of hostcalls** that map to Pi’s already-audited
operations (tools + session/ui actions).

Core examples (names illustrative):
- `pi.tool(name, input)` → delegates to the built-in tool registry (read/write/edit/bash/grep/find/ls)
- `pi.exec(command, args, options)` → a constrained process runner (timeout + process-tree cleanup)
- `pi.fs.*` → a *capability filesystem* rooted at project/cwd (no path escape)
- `pi.http(request)` → a constrained HTTP client (policy-controlled)
- `pi.session.*`, `pi.ui.*`, `pi.events.*` → Pi-internal APIs (no OS exposure)

This is strictly smaller than Node/Bun, and it is auditable: every connector call
is an explicit, logged capability check.

### 1A.2 The Event Loop Bridge (Promises Without Node)

QuickJS supports Promises/microtasks; it just needs a host to **drive** them.

We provide a tiny “Pi event loop”:
- Drain the QuickJS job queue (microtasks)
- Poll outstanding host operations (Rust futures via tokio/asupersync)
- Resolve/reject the corresponding JS Promises
- Repeat until idle (or until a deadline/timer fires)

In other words: Node’s event loop is a *product*; ours is a *proof obligation*:
it only implements what Pi needs, with deterministic testing hooks.

### 1A.3 Why This Is Better (Security + Performance)

**Security:** Node/Bun expose an enormous ambient-authority surface by default.
Our connector layer is capability-based and narrow by construction.

**Performance:** Node/Bun pay startup/memory costs for massive compatibility.
We precompile JS to bytecode (or WASM) and the runtime only contains:
1) a JS engine + 2) a small dispatcher + 3) our connectors.

**Determinism:** With asupersync (LabRuntime) we can test extension async + time
deterministically (no “real time” flakiness).

### 1A.4 PiJS Runtime Contract (Normative)

This section defines the **authoritative PiJS runtime contract** for running
JS/TS extensions **without Node/Bun**, with a **deterministic, testable event
loop** and an explicit, capability-gated hostcall surface.

This contract is the reference for the JS runtime, scheduler, hostcall bridge,
and test harness workstreams.

#### 1A.4.1 Assumptions / Constraints

- **Assume QuickJS has no WebAssembly**: any JS bundle expecting
  `globalThis.WebAssembly` must use the PiWasm bridge (or Tier A WASM
  components).
- No ambient OS APIs: all side effects must flow through the connector
  dispatcher (capability checks + structured audit logs).

#### 1A.4.2 Definitions (Terms)

- **Microtasks**: the QuickJS job queue (Promise reactions, `queueMicrotask`).
- **Macrotasks**: host-driven tasks (timers, inbound extension events, hostcall
  completions).
- **Tick**: one deterministic scheduling step that runs **at most one**
  macrotask plus a full microtask drain.
- **Hostcall**: a side-effecting request from JS to the host, represented in
  protocol terms as `host_call` / `host_result` (see §3.2).

#### 1A.4.3 Module / Artifact Loader Contract

##### Artifact inputs

PiJS executes extension artifacts produced by `extc` (the compiler pipeline) from
**pinned sources** (see `docs/extension-sample.json`).

The compiled output MUST be:
- deterministic (byte-for-byte stable under identical inputs)
- ESM-resolvable inside PiJS
- sourcemap-correct (runtime errors map to original TS/JS)

##### Allowed specifiers and resolution

The PiJS module resolver MUST:
- resolve relative specifiers (`./` and `../`) within the artifact
- resolve internal Pi-provided modules under a reserved namespace (recommended:
  `pi:*`)
- forbid network imports (`http:` / `https:`) and other ambient loaders

Recommended canonicalization performed by `extc`: rewrite Node builtins to
`pi:node/*` and inject any required polyfills deterministically.

##### Initialization contract

- The host loads the artifact entry module.
- The entry module MUST export a **default function** that receives a host-
  provided `pi` object (the Extension API surface).
- Any thrown error during load/initialization MUST be mapped to an extension
  error with sourcemapped location and emitted as structured log events.

#### 1A.4.4 The `pi` API Contract (JS-facing)

The `pi` object provided to extensions is the single ambient authority. It MUST
be capability-gated internally.

##### Registration surface (protocol-facing)

At minimum (shape may follow the legacy API):
- `pi.registerTool(spec)`
- `pi.registerSlashCommand(spec)`
- `pi.on(event_name, handler)` for lifecycle/tool-call hooks

Semantics:
- Registration MUST be idempotent per `(extension_id, name)`.
- Invalid specs MUST fail fast with actionable errors.
- Registration controls what the host advertises/dispatches for that extension.

##### Connector surface (hostcall-facing)

At minimum:
- `pi.tool(name, input) -> Promise<ToolOutput>`
- `pi.exec(cmd, args, options) -> Promise<{ stdout, stderr, exitCode }>`
- `pi.http(request) -> Promise<response>`
- `pi.session.*` accessors/mutations as defined by the protocol
- `pi.ui.*` primitives (select/input/confirm/editor) that can be denied in
  non-interactive mode
- `pi.log(level, event, data)` for extension-authored logs

Rules:
- Every connector method maps to a `host_call` with a `call_id`, capability,
  method, params, and timeout/cancel metadata (§3.2).
- Every connector method MUST emit structured audit logs (see §3.1 / §3.4).
- Errors MUST map onto the hostcall error taxonomy (§3.2): Denied/Timeout/IO/
  InvalidRequest/Internal.

##### Cancellation + timeouts

- Any async connector call MAY accept an `AbortSignal`; cancellation MUST map to
  hostcall cancel-token semantics.
- Timeouts MUST be enforced in the dispatcher; JS receives a deterministic
  Timeout error.

#### 1A.4.5 PiJS Event Loop: Formal State Machine

##### State

Define the runtime state as:

- `seq: u64` monotone counter (total-order tie-breaker)
- `Q_micro`: the QuickJS job queue (internal to engine; host can drain)
- `Q_macro`: FIFO queue of macrotasks, each tagged with an enqueue `seq`
- `Q_timer`: min-heap of timers keyed by `(deadline_ms, seq)`
- `clock`: a monotonic time source (injectable for tests)

Each macrotask is one of:
- `TimerFired(timer_id)`
- `HostcallComplete(call_id, outcome)`
- `InboundEvent(event_id, payload)` (tool_call, slash_command, lifecycle hook,
  UI response, etc.)

##### The `tick()` algorithm (normative)

`tick(state)` MUST be deterministic given the current state and the set of newly
arrived host completions.

Algorithm:
1) **Ingest host completions**: any completed hostcalls since the last tick are
   enqueued into `Q_macro` with a deterministic order key.
   - Recommended: assign each completion an enqueue `seq` in arrival order using
     the monotone counter.
2) **Move due timers**: while `Q_timer.min.deadline_ms <= clock.now_ms`, pop
   timers and enqueue `TimerFired` into `Q_macro` (preserving `(deadline_ms,
   seq)` order).
3) **Run one macrotask**:
   - If `Q_macro` is non-empty: pop the lowest `seq` macrotask and execute it.
   - Else: idle (no-op).
4) **Drain microtasks to fixpoint**: repeatedly drain the QuickJS job queue until
   it is empty.
5) Return updated state.

##### Invariants (must hold)

- **I1 (single macrotask):** at most one macrotask executes per tick.
- **I2 (microtask fixpoint):** after any macrotask, microtasks are drained until
  empty.
- **I3 (stable timers):** timers with equal deadlines fire in increasing `seq`
  order.
- **I4 (no reentrancy):** hostcall completions do not synchronously re-enter JS;
  they enqueue macrotasks.
- **I5 (total order):** all externally observable scheduling is ordered by `seq`
  (deterministic tie-break).

##### Timers contract

- `setTimeout(fn, ms)` enqueues a timer with
  `(deadline_ms = clock.now_ms + ms, seq = next_seq())`.
- `clearTimeout(id)` removes it if pending.
- `setInterval` is optional unless required by the pinned sample; if implemented,
  it MUST be specified in terms of repeated `setTimeout` with stable ordering.

##### Hostcall completion contract

- Each hostcall has a stable `call_id` and (recommended) an issuance `seq`.
- Completion enqueuing MUST be deterministic:
  - In production: order by completion arrival, then stabilize with the monotone
    `seq`.
  - In tests: completion order can be controlled by recorded fixtures /
    deterministic runtime.

#### 1A.4.6 Determinism Contract

##### What we promise

Given:
- identical artifact bytes + shim versions
- identical initial state
- identical sequence of inbound events (tool calls, lifecycle events, UI
  responses)
- identical sequence of hostcall results (including their enqueue order)
- identical clock behavior (or a deterministic clock)

Then:
- the sequence of executed macrotasks and the resulting observable outputs (tool
  results, logs, UI prompts) are identical.

##### Proof sketch (why)

- The scheduler is a pure function of `(state, arrivals)` with a total-order
  tie-breaker `seq`.
- Timer ordering is deterministic via `(deadline_ms, seq)`.
- Hostcall completion ordering is deterministic by construction (completion
  enqueue `seq`).
- Microtask draining to a fixpoint ensures no hidden interleavings.
- Therefore, by induction over ticks, the entire execution trace is deterministic
  under fixed inputs.

#### 1A.4.7 Observability / Trace Contract

- Every tick and every enqueue/dequeue event MAY be logged (debug-level) under
  `pi.ext.log.v1` with `trace_id` / `span_id` and correlation ids.
- Deterministic test runs MUST be able to compare traces for equality after the
  normalization rules in §3.1.

---

## 1B. Extension Taxonomy + Compatibility Matrix (Normative)

This section defines the **canonical extension shapes** we support and maps
each shape to its **entrypoint/config** and **required host capabilities**.
It is the reference for selection, conformance, and documentation work.

### 1B.1 Extension Shapes (Canonical)

**Runtime extensions (executable):**
- **PiJS (JS/TS)** — legacy extensions compiled to JS (Tier B).
- **WASM component** — WIT-based components (Tier A).

**External servers (out-of-process):**
- **MCP server** — stdio/http/sse tool server (Tier C).

**Resource packs (non-executable):**
- **Skill pack** — `SKILL.md` bundles for agent behavior.
- **Prompt template** — `.md` prompt files.
- **Theme** — `.json` theme definitions for UI.

**Bundles/packages (distribution):**
- **Package source** — a bundle that may include any of the above (extensions,
  skills, prompts, themes). Resolved by `src/package_manager.rs`.

### 1B.2 Shape Matrix (entrypoint/config → runtime → I/O)

| Shape | Entrypoint / Config | Runtime | Primary I/O Surface | Notes |
|---|---|---|---|---|
| **PiJS extension** | `extension.json` (`pi.ext.manifest.v1`) or package manifest listing `extensions`; entry `.ts`/`.js` | QuickJS + Pi event loop | `register` + `host_call`/`host_result` | Legacy TS/JS compiled and shimmed; no Node/Bun. |
| **WASM component** | `extension.json` with `runtime="wasm"`; entry `.wasm` component | Wasmtime (component model) | WIT hostcalls → `host_call`/`host_result` | Typed hostcalls via WIT. |
| **MCP server** | MCP config (`*.json`) or CLI args | External process / remote server | MCP protocol (stdio/http/sse) | Not the extension protocol; policy-gated by connector. |
| **Skill pack** | `SKILL.md` + optional assets | None (resource) | File load only | Injected into prompt context; no hostcalls. |
| **Prompt template** | `.md` prompt files | None (resource) | File load only | Used by `/template` invocation. |
| **Theme** | `.json` theme file | None (resource) | File load only | Used by TUI renderer. |
| **Package source** | `package.json` / package manifest with resources | Mixed | Depends on contained resources | May include extensions + skills + prompts + themes. |

### 1B.3 Capability Matrix (registration type → required capabilities)

**Capabilities are always derived from hostcalls** (never trusted from the
extension), but registration types imply typical capability usage:

| Registration type | Protocol surface | Typical hostcalls | Derived capabilities | Notes |
|---|---|---|---|---|
| **Tool** (`registerTool`) | `register` → `tool_call`/`tool_result` | `pi.tool(...)` / `pi.exec(...)` | `read` / `write` / `exec` / `tool` | `read/write/exec` derived by tool name; unknown tools map to `tool`. |
| **Slash command** (`registerCommand`) | `register` → `slash_command`/`slash_result` | `pi.ui.*`, `pi.session.*`, optional `pi.exec` | `ui` / `session` / `exec` | Commands are UI-driven; exec is optional. |
| **Event hook** (`event_hook`) | `register` → `event_hook` | `pi.session.*`, `pi.ui.*`, `pi.exec`, `pi.http` | `session` / `ui` / `exec` / `http` | Capabilities depend on event handler behavior. |
| **Provider** (`registerProvider`) | `register` + streaming hooks | `pi.http(...)` | `http` (+ `read` if local files) | Providers require network; record API key access as `env` if used. |
| **Flag** (`registerFlag`) | `register` only | none until used | none (at register) | Flags are config; capabilities are driven by later behavior. |
| **Shortcut** (`registerShortcut`) | `register` only | `pi.ui.*` on activation | `ui` | Shortcuts are UI-level triggers. |

**Non-executable resource packs (skills/prompts/themes)** do not invoke hostcalls
and therefore have **no runtime capability requirements** beyond file loading.

---

## 1C. Ecosystem Research & Candidate Pool (Informational)

This section is the **research foundation** for extension compatibility work.
It documents **where we discover candidates**, **how we validate them**, and the
**canonical metadata** we track so downstream beads can rank/select without
re‑doing discovery.

### 1C.1 Source Tiers (where candidates come from)

We classify candidates by **source tier** (not by runtime tier):

- `official-pi-mono` — the canonical upstream corpus (the “official 60” plus any
  additional pinned upstream examples).
- `community` — small community repos/gists; often single‑file extensions.
- `third-party-github` — larger third‑party repos (may be multi‑file).
- `npm-registry` — published packages that contain Pi extensions.
- `agents-mikeastock` — special-case curated corpus (kept as its own tier so we
  can reason about provenance).
- `non-conformance` — interesting but explicitly out-of-scope for parity (kept
  for research/triage only).

The tier labels above match the static-scan and master-catalog artifacts in
`docs/` (see §1C.4).

### 1C.2 Discovery Workflow (repeatable + evidence-based)

**Authoritative discovery sources (v1, ordered):**

1. **Upstream pi-mono** (`badlogic/pi-mono`) extension examples corpus (canonical
   “official” reference set).
2. **Curated corpora snapshots** checked into this repo (e.g. under
   `legacy_pi_mono_code/`), used for deterministic scanning and reproducible
   conformance runs.
3. **GitHub discovery sweep** (keyword + topic search) → candidate repos and
   raw files (tracked by research beads).
4. **npm registry sweep** (keyword search + reverse-dep trails) → candidate
   packages and tarballs (tracked by research beads).
5. **Marketplaces/registries** (when applicable) such as OpenClaw/ClawHub
   inventories (tracked by research beads).

We treat discovery as a **pipeline**, not a one-off search:

1. **Enumerate corpus roots** per tier (local repo snapshots, git checkouts,
   npm package tarballs).
2. **Static scan**:
   - Find candidate entrypoints (default export / `register(...)` patterns).
   - Record “capability signals” (imports/calls that imply hostcalls).
   - Emit a machine-readable inventory for dedup + triage.
3. **Dynamic validation** (ground truth):
   - Load each candidate in the **pi-mono TS runtime** (Bun-based harness).
   - Record load success/failure, error class, and registration output.
   - Note: action methods may intentionally throw during load; we only require
     *registration* to succeed.
4. **Consolidate + deduplicate** into a master candidate pool.
5. **Enrich + rank** (only after the pool is stable):
   - Size, file-count, dependency shape, IO patterns, popularity signals.
   - Produce tiered execution plans (conformance ordering, complexity buckets).

### 1C.3 Candidate Identity & Dedup Strategy (deterministic)

The same logical extension can show up via multiple paths (forks, mirrors,
vendored copies, npm repacks). We deduplicate using **canonical source keys**
and content checksums:

- **Canonical source key** (stable identity):
  - Git: `git:<repo_url>#<path>`
  - npm: `npm:<package_name>@<version>#<path>` (or omit `@<version>` if unknown)
  - local snapshot: `local:<absolute_path>`
- **Content checksum** (stable content): `sha256(file_bytes)` (single-file) or
  `sha256(concat(sorted(file_checksums)))` (multi-file directory).

Rules:
- Prefer upstream canonical URLs when known (avoid per-fork “new identities”).
- When two candidates share a checksum, treat them as duplicates unless the
  runtime behavior differs under dynamic validation.
- Human-readable `id` should be stable when possible (manifest id or filename),
  but the source key + checksum are the real identity.

### 1C.4 Canonical Artifacts (source of truth)

We keep the research outputs in `docs/` so they can be reviewed, diffed, and
used by CI/harnesses:

- `docs/extension-entry-scan.json` — static scan inventory (entrypoints +
  submodules + confidence + per-tier stats).
- `docs/extension-master-catalog.json` — **deduplicated master pool** for
  conformance (all tiers, minimal fields + checksums).
- `docs/extension-catalog.json` — enriched metadata for the **full validated
  corpus** (223 extensions across all source tiers, with conformance status,
  capabilities, IO patterns, complexity buckets, checksums, and perf budgets).
- `docs/extension-catalog.schema.json` — JSON Schema for `docs/extension-catalog.json`
  (`pi.ext.catalog.v1`).
- `docs/extension-priority.json` — ranking/order plan for the official corpus
  (testability-first execution strategy).

Downstream beads should treat these as inputs and avoid re-scraping/re-scanning
unless they are explicitly rebuilding the pipeline.

#### Catalog Schema: `pi.ext.catalog.v1`

`docs/extension-catalog.json` is the enriched metadata layer for the **official**
extension corpus. It is defined by:
- Version tag: `schema: "pi.ext.catalog.v1"` (embedded in the JSON)
- Validation: `docs/extension-catalog.schema.json`

**Top-level fields**
- `schema` *(string, const)*: schema identifier (`pi.ext.catalog.v1`)
- `generated_at` *(RFC3339 string)*: artifact generation timestamp
- `total_extensions` *(int)*: number of catalog entries
- `items` *(array)*: catalog entries (see below)
- `tier_summary` / `runtime_summary` *(object)*: aggregate counts

**Catalog entry fields (required in v1)**
- `id` *(string)*: stable extension identifier
- `name` *(string)*: entrypoint filename (informational)
- `source_tier` *(enum)*: provenance tier (official/community/npm/etc)
- `source` *(union)*: pinned source reference (`git`/`npm`/`url`)
- `runtime_tier` *(enum)*: packaging shape bucket (`legacy-js`/`multi-file`/`pkg-with-deps`)
- `interaction_tags` *(enum[])*: tool/command/event/UI/provider surface tags
- `capabilities` *(enum[])*: required capability set (read/write/http/exec/session/ui/etc)
- `io_pattern` *(enum[])*: coarse IO behavior buckets
- `complexity` *(enum)*: `small|medium|large`
- `file_count` / `total_bytes` *(int)*: size metadata for the artifact
- `checksum.sha256` *(hex string)*: stable content checksum

**Reserved fields (optional; populated by downstream beads)**
- `version`: extension version (when applicable; e.g. npm)
- `license`: license identifier (`docs/extension-artifact-provenance.json`)
- `category_tags`: workflow tags (git/tests/devops/etc)
- `compatibility_notes`: known constraints / warning reasons (see `docs/ext-compat.md`)
- `perf_budgets`: perf expectations + observed baselines (bench artifacts)

**Mapping / source-of-truth inputs**
- Checksums + file metadata: `docs/extension-master-catalog.json`
- License + pinned provenance: `docs/extension-artifact-provenance.json`
- Node API + hostcall usage: `docs/extension-api-matrix.json`
- Testability notes + execution order: `docs/extension-priority.json`

### 1C.5 Coverage Targets and Achieved Results

The purpose of coverage targets is to prevent a "high-score shortlist" from
missing whole classes of real-world behavior. Targets are used by selection
beads to produce a Tier-1 "must-pass" corpus that is large, stratified, and
defensible.

**Tier size targets (selection constraint):**

- **Tier-0 baseline:** the upstream official example set (must-pass baseline).
- **Tier-1 MUST PASS:** **≥ 200** unmodified extensions, stratified across
  source tiers and behavior buckets.
- **Tier-2 stretch:** additional long-tail extensions chosen primarily for
  unique API surface / coverage (not popularity).

**Achieved coverage (as of 2026-02-07):**

All 223 validated extensions are tested. 187 pass (83.9%).

| Source tier | Target | Actual | Pass | Rate |
|---|---:|---:|---:|---:|
| `official-pi-mono` | 60 | 61 | 60 | 98.4% |
| `npm-registry` | 50 | 75 | 48 | 64.0% |
| `community` | 50 | 58 | 52 | 89.7% |
| `third-party-github` | 20 | 23 | 16 | 69.6% |
| `agents-mikeastock` | all | 1 | 0 | 0% |
| **Total** | **≥ 200** | **223** | **187** | **83.9%** |

By conformance tier (complexity bucket):

| Tier | Description | Total | Pass | Rate |
|---|---|---:|---:|---:|
| T1 | Simple single-file | 38 | 38 | 100% |
| T2 | Multi-registration | 87 | 85 | 97.7% |
| T3 | Multi-file / complex | 90 | 60 | 66.7% |
| T4 | npm dependencies | 3 | 2 | 66.7% |
| T5 | exec/network | 5 | 2 | 40.0% |

**36 failures break down as:**
- Manifest registration mismatch (22) — fixable by auditing manifests
- Missing npm package stub (5) — fixable by adding virtual modules
- Multi-file dependency (4) — partially fixable (needs bundling)
- Runtime error (4) — needs investigation
- Test fixture (1) — not a real extension

See `tests/ext_conformance/reports/COMPATIBILITY_SUMMARY.md` for the full
combined conformance + performance report.

**Tier-1 behavior / capability quotas (minimum coverage buckets):**

Registration / surface:
- **Tools:** include all extensions that register tools (or meet a minimum of
  60, whichever is larger as the corpus grows).
- **Event hooks:** include all event-hook extensions (or ≥ 80).
- **Slash commands:** include ≥ 25 command extensions.
- **Provider registration / streaming:** include **all** provider-registered
  extensions (rare/high-risk surface).
- **UI surfaces:** include ≥ 15 overlay-heavy and ≥ 40 UI-integrated (header/
  footer/status/message-renderer) extensions.

Hostcall / capability risk:
- **Exec-heavy (`exec_api`)**: include all (capability is high risk).
- **Network-heavy (`http`)**: include ≥ 25.
- **FS-heavy (`read/write/edit`)**: include ≥ 50.
- **Session/UI heavy (`session_api` / `ui_*`)**: include ≥ 50 combined.

**Category coverage (user workflow buckets):**

Maintain at least a small quorum in each high-value workflow category:
- `git` / repo hygiene / checkpoints
- `tests` / lint / format / CI
- `devops` / infra / cloud tooling
- `research` / search / summarization
- `codegen` / refactor / scaffolding
- `ui` / interaction / TUI enhancements
- `security` / policy / guardrails

Notes:
- These targets intentionally mix **hard minima** and "include-all-rare" rules.
  For rare-but-critical surfaces (provider registration, exec-heavy), selection
  should bias toward full coverage rather than sampling.
- The `docs/extension-*.json` artifacts are the measurement source for counts
  and bucket classification.

## 2. Artifact Pipeline (Legacy → Optimized)

**Inputs**
- `extension.json` (manifest)
- Source files (TS/JS or Rust/WASM)

**Pipeline**
1. **SWC build**: TS/JS → bundle (tree‑shaken/minified).
2. **Compatibility scan**: static analysis for forbidden APIs.
3. **Protocol shim**: rewrite legacy extension imports to hostcalls.
4. **Artifact build**:
   - **QuickJS bytecode** (fast startup), or
   - **WASM component** (portable + sandboxed).
5. **Cache** by hash:
   ```
   hash = sha256(manifest + bundle + engine_version)
   ```

**Output**
- `extension.artifact` + `artifact.json` (metadata, engine, hash, caps)

---

## 2A. Extc Compatibility Contract (Normative)

This section defines the **extc compiler contract** that maps legacy Node/Bun imports
to PiJS shims so **all 16 extensions in `docs/extension-sample.json` run unmodified**
(no manual source edits required).

### 2A.1 Genericity Constraint (Non-Negotiable)

- **No per-extension exceptions** in extc rewrites.
- Rewrites MUST be defined solely in terms of import specifiers and generic,
  semantics-preserving code patterns.
- If the sample reveals a gap, fix it by adding a **general rule + tests**, not by
  branching on extension id.

### 2A.2 Canonical Import Rewrite Rules

Extc MUST ensure every import specifier is resolvable inside PiJS without Node/Bun.

#### A) Node Builtins (`node:*`)

Rewrite `node:*` builtins to an internal namespace that PiJS provides (so bundlers
don't externalize them):

| Source Specifier        | Target Specifier           |
|-------------------------|----------------------------|
| `node:fs`               | `pi:node/fs`               |
| `node:fs/promises`      | `pi:node/fs_promises`      |
| `node:path`             | `pi:node/path`             |
| `node:os`               | `pi:node/os`               |
| `node:url`              | `pi:node/url`              |
| `node:crypto`           | `pi:node/crypto`           |
| `node:child_process`    | `pi:node/child_process`    |
| `node:module`           | `pi:node/module`           |

#### B) Bare Builtins (No Prefix)

Many real-world deps import builtins without the `node:` prefix. Treat these
identically:

| Source Specifier        | Target Specifier           |
|-------------------------|----------------------------|
| `fs`                    | `pi:node/fs`               |
| `fs/promises`           | `pi:node/fs_promises`      |
| `path`                  | `pi:node/path`             |
| `os`                    | `pi:node/os`               |
| `url`                   | `pi:node/url`              |
| `crypto`                | `pi:node/crypto`           |
| `child_process`         | `pi:node/child_process`    |
| `module`                | `pi:node/module`           |

### 2A.3 Global Polyfill Injection

Extc MAY inject an idempotent prelude import at the bundle entrypoint:

```javascript
import 'pi:polyfills/node_globals'  // installs process, Buffer, __dirname, __filename
import 'pi:polyfills/fetch'         // if needed: fetch, Headers, Request, Response
import 'pi:polyfills/webassembly'   // PiWasm bridge (QuickJS has no native wasm)
```

**Injection rules:**
- **Deterministic**: stable ordering, always at the top of the entry module.
- **Sourcemap-correct**: injected imports MUST NOT corrupt sourcemap line mappings.
- **Versioned**: `shim_version` MUST be included in the artifact hash.
- **Idempotent**: multiple injections produce identical output.

**Node globals provided by `pi:polyfills/node_globals`:**
- `process` (with `process.env`, `process.cwd()`, `process.platform`, etc.)
- `Buffer`
- `__dirname` / `__filename` (computed from module URL)
- `global` (alias for `globalThis`)
- `setImmediate` / `clearImmediate`

### 2A.4 Forbidden and Flagged APIs

The compatibility scanner MUST classify APIs into:

#### Forbidden (Hard Error)

APIs that bypass capability policy or escape the sandbox. Extc MUST reject bundles
using these:

| API / Pattern                        | Reason                                   |
|--------------------------------------|------------------------------------------|
| `require('vm')` / `node:vm`          | Arbitrary code execution                 |
| `require('worker_threads')`          | Unsupported concurrency model            |
| `require('cluster')`                 | Unsupported concurrency model            |
| `require('dgram')`                   | Raw UDP sockets                          |
| `require('net')` (raw sockets)       | Bypasses HTTP policy                     |
| `require('tls')` (raw sockets)       | Bypasses HTTP policy                     |
| `require('inspector')`               | Debugger access                          |
| `require('perf_hooks')`              | Performance timing oracle                |
| `require('v8')`                      | Engine internals                         |
| `require('repl')`                    | Interactive eval                         |
| `process.binding()`                  | Native module access                     |
| `process.dlopen()`                   | Native addon loading                     |
| Direct `eval()` with dynamic string  | Arbitrary code execution (see note)      |

**Note on `new Function(...)`:** The pinned sample includes `new Function(...)` for
loading a bundled script. This is **flagged but allowed** with evidence logging,
not forbidden outright.

#### Flagged (Warning + Evidence)

Risky constructs that require evidence logging but don't block compilation:

| API / Pattern                        | Evidence Required                        |
|--------------------------------------|------------------------------------------|
| `new Function(...)`                  | Log function body hash + call site       |
| `eval(variable)`                     | Log if variable is not a literal         |
| `setTimeout(string, ...)`            | Log string body hash                     |
| `setInterval(string, ...)`           | Log string body hash                     |
| `Proxy` / `Reflect` (reflection)     | Log usage pattern                        |
| `Object.defineProperty` on builtins  | Log target + property                    |

### 2A.5 Extc Input/Output Contract

#### Input

- Extension manifest (`extension.json` or `package.json`)
- Source files (TypeScript or JavaScript)
- Optional: `tsconfig.json` for type resolution

#### Output

- **ESM bundle**: single entry module, tree-shaken, minified
- **Sourcemap**: accurate line/column mapping to original source
- **Artifact metadata** (`artifact.json`):
  ```json
  {
    "schema": "pi.ext.artifact.v1",
    "extension_id": "...",
    "entry_module": "index.js",
    "hash": "sha256:...",
    "shim_version": "1.0.0",
    "rewrite_log": [
      { "from": "node:fs", "to": "pi:node/fs", "locations": [...] }
    ],
    "injected_polyfills": ["pi:polyfills/node_globals"],
    "flagged_apis": [
      { "api": "new Function", "locations": [...], "evidence_hash": "..." }
    ],
    "forbidden_apis": [],
    "capabilities_required": ["read", "exec"]
  }
  ```

- `capabilities_required` MUST be computed per §2B.3 (declared ∪ inferred) with
  deterministic ordering.

#### Side-Effect Policy

- Extc MUST NOT execute extension code during compilation.
- Static analysis only; no `require()` resolution that triggers side effects.
- If a dependency cannot be statically analyzed, emit a warning and include it
  verbatim (the runtime will handle capability checks).

### 2A.6 Compatibility Matrix

The following Node APIs are supported via shims. Each maps to a PiJS connector
with explicit capability requirements:

| Node API                 | Shim Module             | Sync/Async | Capability   | Notes                          |
|--------------------------|-------------------------|------------|--------------|--------------------------------|
| `fs.readFileSync`        | `pi:node/fs`            | sync       | `read`       | Blocks event loop              |
| `fs.writeFileSync`       | `pi:node/fs`            | sync       | `write`      | Blocks event loop              |
| `fs.promises.readFile`   | `pi:node/fs_promises`   | async      | `read`       | Preferred                      |
| `fs.promises.writeFile`  | `pi:node/fs_promises`   | async      | `write`      | Preferred                      |
| `fs.existsSync`          | `pi:node/fs`            | sync       | `read`       |                                |
| `fs.readdirSync`         | `pi:node/fs`            | sync       | `read`       |                                |
| `fs.statSync`            | `pi:node/fs`            | sync       | `read`       |                                |
| `fs.mkdirSync`           | `pi:node/fs`            | sync       | `write`      |                                |
| `path.join`              | `pi:node/path`          | sync       | (none)       | Pure computation               |
| `path.resolve`           | `pi:node/path`          | sync       | (none)       | Uses `process.cwd()`           |
| `path.dirname`           | `pi:node/path`          | sync       | (none)       | Pure computation               |
| `path.basename`          | `pi:node/path`          | sync       | (none)       | Pure computation               |
| `path.extname`           | `pi:node/path`          | sync       | (none)       | Pure computation               |
| `os.platform`            | `pi:node/os`            | sync       | `env`        | Returns host platform          |
| `os.homedir`             | `pi:node/os`            | sync       | `env`        | Returns home directory         |
| `os.tmpdir`              | `pi:node/os`            | sync       | `env`        | Returns temp directory         |
| `child_process.spawn`    | `pi:node/child_process` | async      | `exec`       | Streams stdout/stderr          |
| `child_process.exec`     | `pi:node/child_process` | async      | `exec`       | Buffers output                 |
| `child_process.execSync` | `pi:node/child_process` | sync       | `exec`       | Blocks; prefer async           |
| `crypto.randomBytes`     | `pi:node/crypto`        | sync       | (none)       | CSPRNG                         |
| `crypto.createHash`      | `pi:node/crypto`        | sync       | (none)       | Pure computation               |
| `url.parse`              | `pi:node/url`           | sync       | (none)       | Pure computation               |
| `url.URL`                | `pi:node/url`           | sync       | (none)       | WHATWG URL                     |
| `process.env`            | `pi:polyfills/...`      | sync       | `env`        | Filtered by policy             |
| `process.cwd()`          | `pi:polyfills/...`      | sync       | (none)       | Project root                   |
| `process.exit()`         | `pi:polyfills/...`      | sync       | (none)       | Throws; extension cannot exit  |
| `Buffer.from`            | `pi:polyfills/...`      | sync       | (none)       | Binary data handling           |
| `Buffer.alloc`           | `pi:polyfills/...`      | sync       | (none)       | Binary data handling           |
| `fetch`                  | `pi:polyfills/fetch`    | async      | `http`       | WHATWG Fetch                   |

**Error mapping:** All shim errors MUST map to the hostcall error taxonomy (§3.2):
`denied`, `timeout`, `io`, `invalid_request`, `internal`.

### 2A.7 Sourcemap Contract

Extc MUST produce sourcemaps that:

1. **Map accurately**: every generated line/column MUST map to the correct original
   source location.
2. **Preserve through rewrites**: import rewrites MUST NOT corrupt mappings.
3. **Include sources**: sourcemap SHOULD include `sourcesContent` for offline
   debugging.
4. **Inline or external**: support both inline (`//# sourceMappingURL=data:...`)
   and external (`.map` file) formats.

**Runtime usage:**
- When an error occurs, the runtime MUST use the sourcemap to produce a stack trace
  with original file/line/column.
- Structured logs (§3.1) MUST include sourcemapped locations in `source.location`.

### 2A.8 Test Requirements

- **Unit transform fixtures**: common imports + injection patterns with expected
  output.
- **Negative tests**: forbidden APIs MUST produce exact error messages.
- **E2E harness**: verify rewritten bundles run 16/16 sample extensions with
  actionable failure diagnostics.

---

## 2B. Extension Manifest + Capability Inference (Normative)

This section defines:
- the **on‑disk extension manifest** (`extension.json`), and
- how tooling derives **required capabilities** deterministically from a bundle
  (capability inference) and merges them with the manifest.

This is the contract used by:
- **extc** (compiler + compatibility scanner) during artifact build (§2A), and
- **runtime + harness** when deciding prompt/deny and validating conformance.

### 2B.1 Extension Manifest (`extension.json`, `pi.ext.manifest.v1`)

**Location:** `<extension_root>/extension.json`

**Fallback:** if `extension.json` is missing, extc MAY read the same schema from
`package.json#pi`. In that case, `name` / `version` default to top‑level
`package.json` fields unless overridden inside `pi`. If both exist,
`extension.json` wins.

**Canonicalization (v1):**
- Manifest hashing MUST use **canonical JSON** (UTF‑8, no whitespace, object keys
  sorted lexicographically, arrays preserve order).
- The pipeline hash (§2) is computed over canonical manifest bytes.

**Machine schema:** `docs/schema/extension_manifest.json`

**Schema (v1) — human‑readable form:**
```json
{
  "schema": "pi.ext.manifest.v1",
  "extension_id": "ext.todo",
  "name": "Todo",
  "version": "0.1.0",
  "api_version": "1.0",
  "runtime": "js",
  "entrypoint": "src/index.ts",

  "capabilities": ["read"],
  "capability_manifest": {
    "schema": "pi.ext.cap.v1",
    "capabilities": [
      { "capability": "read", "methods": ["tool"], "scope": { "paths": ["src/**"] } }
    ]
  }
}
```

Fields:
- `schema` (required): must be `pi.ext.manifest.v1`.
- `extension_id` (required): stable identifier used in logs (`ext.log.v1`) and
  harness fixtures.
- `name` / `version` / `api_version` (required): must match the protocol
  `register` payload (§3).
- `runtime` (required): `js` or `wasm`.
- `entrypoint` (required): path relative to extension root:
  - JS: source entrypoint (pre‑bundle), e.g. `src/index.ts`.
  - WASM: component artifact path, e.g. `dist/extension.wasm`.
- `capabilities` (optional, legacy): flat list used as a coarse capability set
  until all extensions emit a scoped manifest.
- `capability_manifest` (optional, recommended): scoped requirements using the
  schema in §3.3 (`pi.ext.cap.v1`).

### 2B.2 Capability Inference (`pi.ext.infer.v1`)

**Goal:** deterministically derive the minimum known set of capabilities that an
artifact *appears* to require, with auditable evidence.

**Output:** an inferred `pi.ext.cap.v1`‑shaped requirement set plus evidence
records. The inferred set is written into `artifact.json` as:
- `capabilities_required`: a stable, sorted list of capability keys (`read`,
  `write`, `exec`, `http`, ...), and optionally
- `capability_scope_inferred`: a scoped manifest when inference can extract
  stable scopes (paths/hosts).

**Evidence sources (v1, ordered):**
0) **Config files (when source is unavailable):**
   - `package.json#pi.capabilities` may be treated as coarse evidence.
   - Dependency signatures MAY be used for coarse inference (e.g., `node-fetch`,
     `axios`, `undici` → `http`) with `kind=config_hint`.
1) **Import specifiers** (post‑rewrite):
   - `pi:node/fs` / `pi:node/fs_promises` → infer `read` and/or `write` based on
     used APIs (see rules below).
   - `pi:node/child_process` → `exec`.
   - `pi:polyfills/fetch` or `fetch(` usage → `http`.
2) **PiJS primitives**:
   - `pi.tool("read"|"grep"|"find"|"ls", ...)` → `read`
   - `pi.tool("write"|"edit", ...)` → `write`
   - `pi.tool("bash", ...)` or `pi.exec(...)` → `exec`
   - `pi.http(...)` → `http`
3) **Literal scope hints** (best‑effort):
   - `read`/`write` paths: string literals that look like relative paths.
   - `http` hosts: string literals parsed as URLs; host extracted.

**Inference rules (v1):**
- Determinism: inference MUST be stable across platforms; ordering is:
  `capability` ascending, then `method` ascending, then scopes sorted.
- Soundness target: inference MUST be **conservative** (over‑approx is allowed),
  but MUST NOT invent scopes from non‑literal sources. Dynamic values produce
  an **unspecified scope** (forces prompt/deny depending on policy).
- JS vs WASM: the capability names and scope semantics are identical (§3.2A).
  WASM inference MAY be based on:
  - static analysis of the component (if available), or
  - observed `host_call` traces in capture mode (preferred for correctness).

### 2B.3 Merge Policy (Declared ∪ Inferred + User Overrides)

Define:
- `declared`: from `extension.json.capability_manifest` if present, otherwise
  from legacy `extension.json.capabilities` (coarse).
- `inferred`: from the inference engine (§2B.2).
- `overrides`: user policy overrides (allow/deny/narrow scope) from config.

**Effective requirements (v1):**
1) Start with `declared ∪ inferred` (union by capability key).
2) Apply user **deny** overrides:
   - Removing a capability is allowed; runtime hostcalls will return `denied`.
   - Narrowing scope is allowed; apply scope intersection.
   - If a **declared** capability is denied:
     - `strict` → registration fails
     - `prompt` → user decision required
     - `permissive` → allow but log
3) Apply user **allow** overrides (add capability / widen scope).
4) Emit a `capability.resolve` log (see §2B.5) with the full breakdown.

### 2B.4 Validation (Hard Errors)

The runtime/harness MUST reject an extension manifest when:
- `schema` is unknown.
- `name`/`version`/`api_version` are empty.
- A declared capability key is unknown to the taxonomy (§3.2A).
- A declared scope contains invalid shapes (non‑string items) or non‑normalized
patterns (implementation-defined, but MUST be deterministic).

### 2B.5 Capability Resolution Logs (ext.log.v1)

At extension load (artifact or dev), the host MUST emit one log entry:
- `event`: `capability.resolve`
- `data`: declared/inferred/overrides/effective, plus evidence hashes.

Example:
```json
{
  "schema": "pi.ext.log.v1",
  "ts": "2026-02-03T00:00:00Z",
  "level": "info",
  "event": "capability.resolve",
  "message": "Resolved effective capabilities",
  "correlation": { "extension_id": "ext.todo", "scenario_id": "scn-local" },
  "data": {
    "declared": ["read"],
    "inferred": ["read", "http"],
    "effective": ["read", "http"],
    "evidence": [
      { "capability": "http", "kind": "literal_url", "value_hash": "sha256:..." }
    ]
  }
}
```

Notes:
- Evidence values SHOULD be hashed (and optionally include a redacted preview)
  to avoid leaking secrets; follow the redaction rules in §3.1.

### 2B.6 Test Requirements

- Unit fixtures for inference with deterministic ordering (same input → same
  inferred output).
- Negative fixtures for invalid manifests (unknown capability, invalid scope).
- Harness fixtures asserting `capability.resolve` logs are stable after
  normalization.

---

## 3. Extension Protocol (v1)

All communication uses a **versioned, JSON‑encoded protocol**:
`docs/schema/extension_protocol.json`.

Core message types:
- `register`
- `tool_call` / `tool_result`
- `slash_command` / `slash_result`
- `event_hook`
- `host_call` / `host_result` (extension → core connector calls)
- `log` / `error`

WASM components use the **WIT interface** in `docs/wit/extension.wit`.

---

### 3.1 Structured Logging (ext.log.v1)

All extension-related logs across **capture**, **harness**, and **runtime** must
use the same JSONL schema. The protocol `log` message payload matches this
schema exactly. One log entry per line.

**Log entry schema (required fields marked \*):**
```json
{
  "schema": "pi.ext.log.v1",          // *
  "ts": "2026-02-03T03:01:02.123Z",   // * RFC3339
  "level": "info",                    // * debug|info|warn|error
  "event": "tool_call.start",         // * stable event name
  "message": "tool call dispatched",  // * human summary
  "correlation": {                    // * IDs for joining logs
    "extension_id": "ext.my_ext",     // *
    "scenario_id": "scn-001",         // *
    "session_id": "sess-abc123",
    "run_id": "run-20260203-0001",
    "artifact_id": "sha256:...",
    "tool_call_id": "tool-42",
    "slash_command_id": "slash-7",
    "event_id": "evt-9",
    "host_call_id": "host-13",
    "rpc_id": "rpc-55",
    "trace_id": "trace-...",
    "span_id": "span-..."
  },
  "source": {                         // optional emitter info
    "component": "runtime",           // capture|harness|runtime|extension
    "host": "host.name",
    "pid": 4242
  },
  "data": { "duration_ms": 12 }
}
```

**Event naming (examples):**
- `extension.register`, `extension.ready`
- `tool_call.start`, `tool_call.end`
- `slash_command.start`, `slash_command.end`
- `event_hook.start`, `event_hook.end`
- `host_call.start`, `host_call.end`
- `policy.decision`, `compat.warning`

**Correlation rules:**
- `extension_id` + `scenario_id` are **required** for all extension logs.
- Populate the most specific ID available (`tool_call_id`, `slash_command_id`,
  `event_id`, `host_call_id`, `rpc_id`).
- `trace_id`/`span_id` are optional but recommended for long chains.

**Redaction rules (mandatory):**
- Replace secrets/credentials with `"[REDACTED]"`.
- Always redact keys matching (case-insensitive):
  `api_key`, `token`, `authorization`, `cookie`, `password`, `secret`,
  `private_key`, `credential`, `bearer`.
- For PII (email/phone/address), either redact or hash.
- Never log full file contents; log only sizes/paths/summary.

**Normalization for fixtures (deterministic diffs):**
- Replace `ts`, `pid`, `host`, `run_id`, `session_id`, `artifact_id`,
  `trace_id`, `span_id` with placeholders.
- Normalize absolute paths to `<cwd>/...`.
- Stable IDs (like `scenario_id`) must be deterministic and **not** randomized.

**Log sinks (documented contract):**
- **Runtime:** `~/.pi/agent/logs/extensions/<session_id>.jsonl`
  (override with `PI_EXTENSION_LOG_DIR`).
- **Capture:** `tests/ext_conformance/capture/<ext>/<scenario>/extension.log.jsonl`
- **Harness:** `target/ext_conformance/logs/<scenario_id>.jsonl`

**CI consumption:**
- CI should archive `target/ext_conformance/logs/**` as artifacts.
- Harness compares normalized logs to fixtures; diffs are triaged by `event`
  and `correlation` IDs.

---

### 3.2 Hostcall ABI (`host_call` / `host_result`)

`host_call` is the **only** way an extension requests privileged I/O from core.
Every call is explicit, capability-gated, and logged.

**`host_call.payload` fields (v1):**
- `call_id` (string, required): correlates request ↔ response(s).
- `capability` (string, required): the capability key evaluated by policy. **MUST**
  match the capability core derives from `method` + `params` (prevents spoofing).
- `method` (string, required): connector method name (e.g. `tool`, `exec`, `http`,
  `session`, `ui`, `log`).
- `params` (object, required): method-specific parameters.
- `timeout_ms` (int, optional): wall-clock timeout for the host operation.
- `cancel_token` (string, optional): idempotent cancellation handle (future).
- `context` (object, optional): free-form metadata (never used for policy decisions).

Example (`tool` call):
```json
{
  "call_id": "host-1",
  "capability": "read",
  "method": "tool",
  "params": { "name": "grep", "input": { "pattern": "TODO", "path": "src/" } },
  "timeout_ms": 2500
}
```

**Capability derivation (core-defined, v1):**
- For `method="tool"`, required capability is derived from `params.name`:
  - `read|grep|find|ls` → `read`
  - `write|edit` → `write`
  - `bash` → `exec`
  - unknown tool → `tool` (forces prompt/deny depending on policy)
- For other methods, required capability is the method itself (`http`, `exec`, etc).

**`host_result.payload` fields (v1):**
- `call_id` (string, required)
- `output` (object, required): method-specific result object (may be empty on error)
- `is_error` (bool, required)
- `error` (object, optional): required when `is_error=true`, forbidden otherwise
- `chunk` (object, optional): streaming metadata (when results are chunked)

Error example:
```json
{
  "call_id": "host-1",
  "output": {},
  "is_error": true,
  "error": {
    "code": "denied",
    "message": "capability denied by policy",
    "retryable": false,
    "details": { "capability": "exec" }
  }
}
```

**Error taxonomy (v1):**
- `timeout`: deadline reached.
- `denied`: capability not granted or out of scope.
- `io`: connector I/O failure (fs/network/process).
- `invalid_request`: malformed method/params/capability mismatch.
- `internal`: bug or invariant violation in the host.

**Streaming contract (v1):**
- Core may emit multiple `host_result` messages with the same `call_id`.
- When streaming, each message includes `chunk.index` starting at 0 and increasing
  by 1, and `chunk.is_last=true` marks the final chunk.
- `chunk.backpressure` is reserved for future flow-control hints.

---

### 3.2A Unified JS + WASM Capability Model (Normative)

This section defines a **single, coherent capability model** that applies
equally to **PiJS (JS)** and **WASM** extensions. Policy evaluation, logging,
and tooling **must not diverge** by runtime.

#### Capability taxonomy (v1)

| Capability | JS surface (PiJS) | WASM hostcall | Scope | Notes |
|---|---|---|---|---|
| `read` | `pi.tool(read/grep/find/ls)`; `pi.fs.read/list/stat` | `host_call(method=tool, name in {read,grep,find,ls})`; `host_call(method=fs, op in {read,list,stat})` | `paths` | Path scope enforced by connector. |
| `write` | `pi.tool(write/edit)`; `pi.fs.write/mkdir/delete` | `host_call(method=tool, name in {write,edit})`; `host_call(method=fs, op in {write,mkdir,delete})` | `paths` | Includes mutation; default-deny in strict mode. |
| `exec` | `pi.exec(...)`; `pi.tool(bash)` | `host_call(method=exec)`; `host_call(method=tool, name=bash)` | none | Process execution; high-risk. |
| `http` | `pi.http(request)` | `host_call(method=http)` | `hosts` | Host allow-list enforced. |
| `session` | `pi.session.*` | `host_call(method=session)` | none | Session metadata access. |
| `ui` | `pi.ui.*` | `host_call(method=ui)` | none | May be denied in non-interactive mode. |
| `log` | `pi.log(...)` | `host_call(method=log)` | none | Structured logging only. |
| `tool` | `pi.tool(<non-core>)` | `host_call(method=tool, name=<non-core>)` | none | Used for unknown/custom tools; forces prompt/deny in strict/prompt modes. |

Notes:
- The `fs` hostcall method is optional until the FS connector lands, but **when
  present** it MUST map to `read`/`write` exactly as shown above.
- The `tool` capability is a **catch-all** for non-core tools; the host should
  prefer explicit `read`/`write`/`exec` mapping for built-ins.

#### Mapping rules (required)

1) **Core derives capability** from `method` + `params` (never trust extension
   provided capability for authorization).
2) **JS and WASM map to the same capability names**. A policy decision made for
   JS must be identical for the equivalent WASM call.
3) **Mismatch is an error**: if `host_call.payload.capability` disagrees with
   the derived capability, respond with `invalid_request`.

#### Policy + logging alignment

- The **same policy evaluator** applies to both runtimes.
- Audit logs **must include** `capability`, `method`, and the derived decision.
- Recommended: include a `runtime` field in `log.data` (`js` or `wasm`) to make
  cross-runtime comparisons trivial.

---

### 3.3 Capability Manifest (`pi.ext.cap.v1`)

`register.payload.capability_manifest` optionally declares the extension’s
required capabilities up front so policy can prompt/deny deterministically and
the harness can validate conformance.

Schema (v1):
```json
{
  "schema": "pi.ext.cap.v1",
  "capabilities": [
    { "capability": "read", "methods": ["tool"], "scope": { "paths": ["src/**"] } },
    { "capability": "http", "methods": ["http"], "scope": { "hosts": ["api.github.com"] } }
  ]
}
```

Fields:
- `capabilities[].capability`: capability key (the same string used by policy and
  `host_call.payload.capability`).
- `capabilities[].methods` (optional): restrict to a set of connector methods
  that may be used with this capability (defense-in-depth).
- `capabilities[].scope` (optional):
  - `paths`: glob-like patterns relative to the project root/cwd.
  - `hosts`: allow-list of hostnames/domains for network calls.
  - `env`: allow-list of env var names (future connector).

Notes:
- `register.payload.capabilities` remains the legacy, flat list; it will be
  treated as a coarse capability set until all extensions emit a manifest.
- The manifest applies **equally to JS and WASM** runtimes; capability names and
  scope semantics are identical across both.
- Extensions SHOULD mirror the resolved set (declared ∪ inferred, §2B.3) in
  `capability_manifest`; hosts MUST log any drift.

---

### 3.4 Hostcall Evidence Ledger (per-call log contract)

For every hostcall the runtime emits an append-only evidence ledger using
`pi.ext.log.v1`:
- `host_call.start`: emitted immediately before dispatch
- `host_call.end`: emitted once on completion (success, error, or timeout)

**Required ledger fields (in `log.data`):**
- `capability` / `method`
- `params_hash` (sha256 hex)
- `timeout_ms` (if present)
- `duration_ms` (end event)
- `is_error` + `error.code` (end event, if error)

**`params_hash` canonicalization (v1):**
- Hash the canonical JSON serialization of:
  `{ "method": <method>, "params": <params> }`
- Canonical JSON rules: UTF-8, no whitespace, object keys sorted
  lexicographically, arrays preserve order.
- Never write raw `params` to logs (hash-only) unless explicitly allowed by a
  fixture or debug mode.

---

## 4. Capability Policy (Configurable Modes)

`extensions.policy.mode` supports:
- `strict`: deny by default, explicit grants required.
- `prompt`: ask once per capability.
- `permissive`: allow most; warn and log.

Suggested config (document‑only for now):
```json
{
  "extensions": {
    "policy": {
      "mode": "prompt",
      "max_memory_mb": 256,
      "default_caps": ["read", "write", "http"],
      "deny_caps": ["exec", "env"]
    }
  }
}
```

Capabilities are enforced per‑hostcall and logged in an **audit ledger**.

---

## 5. Alien‑Artifact Safety (Formal Decisioning)

We apply a **loss‑aware, evidence‑driven** model to decide capability grants.

**Evidence Ledger** (example):
```
E = { uses_fs: 0.8, uses_exec: 0.1, unsigned: 0.6, size_mb: 0.2 }
```

**Loss matrix** (risk‑averse):
```
           | grant | deny |
-----------+-------+------+
benign     |   0   |   2  |
malicious  | 100   |   1  |
```

Decision rule: grant if expected loss is lower. This supports **strict** and
**prompt** modes with mathematically traceable decisions.

> This is intentionally conservative: false‑deny is cheap; false‑grant is costly.

---

## 6. Conformance Harness

The conformance harness validates that extensions load and register correctly
in the Rust QuickJS runtime by comparing against expected registrations from
a validated manifest (`VALIDATED_MANIFEST.json`).

### 6.1 Test Infrastructure

- **`tests/ext_conformance_generated.rs`** — auto-generated `conformance_test!`
  macro invocations for all 223 extensions in the corpus.
- **`tests/ext_conformance/mod.rs`** — harness core: loads an extension in
  QuickJS, captures registrations (tools, commands, flags, providers, hooks,
  shortcuts), compares against the validated manifest.
- **`tests/ext_conformance/fixtures/*.json`** — golden fixtures for 16
  representative extensions (used by differential oracle tests).
- **`VALIDATED_MANIFEST.json`** — ground truth from the pi-mono TS runtime
  (generated by loading each extension in Bun and capturing its registrations).

### 6.2 Differential Oracle (TS vs Rust)

The conformance harness uses a **differential oracle** approach:
1. Load each extension in the **pi-mono TS runtime** (Bun-based) → record
   registered tools, commands, hooks, flags, providers, shortcuts.
2. Load the same extension in the **Rust QuickJS runtime** → record the same.
3. Compare the two outputs. Any difference is a conformance failure.

This ensures the Rust runtime produces identical behavior to the reference
implementation without coupling tests to implementation details.

### 6.3 Running Conformance Tests

```bash
# Run all 223 conformance tests
cargo test --test ext_conformance_generated --features ext-conformance -- --nocapture

# Generate full conformance report (JSONL + JSON + MD)
cargo test --test ext_conformance_generated conformance_full_report \
  --features ext-conformance -- --nocapture
```

### 6.4 Current Results (2026-02-07)

- **187 of 223 extensions pass** (83.9%)
- **100% pass rate** for Tier 1 (simple single-file) extensions
- **98.4% pass rate** for official pi-mono extensions (60/61; 1 test fixture)
- **30 negative tests pass** (malformed/hostile extensions correctly rejected)

Reports:
- `tests/ext_conformance/reports/conformance_baseline.json` — machine-readable baseline
- `tests/ext_conformance/reports/conformance_summary.json` — summary with failure categories
- `tests/ext_conformance/reports/CONFORMANCE_REPORT.md` — detailed per-extension results
- `tests/ext_conformance/reports/COMPATIBILITY_SUMMARY.md` — combined conformance + perf

---

## 7. Performance Harness

The performance harness measures extension load times and event dispatch
latency across the corpus, enforces budgets, and detects regressions.

### 7.1 Benchmark Infrastructure

- **`tests/ext_bench_harness.rs`** — benchmark runner with 3 scenarios:
  cold load (fresh runtime), warm load (cached runtime), event dispatch.
- **`tests/perf_budgets.rs`** — CI-enforced budget checks that read baseline
  data and fail if thresholds are exceeded.
- **`BENCHMARKS.md`** — workflow documentation (modes, env vars, interpretation).

### 7.2 Running Benchmarks

```bash
# Quick PR check (10 diverse extensions, 3 iterations)
PI_BENCH_MODE=pr cargo test --test ext_bench_harness --features ext-conformance -- --nocapture

# Nightly full corpus (103 safe extensions, 10 iterations)
PI_BENCH_MODE=nightly PI_BENCH_MAX=103 PI_BENCH_ITERATIONS=10 \
  cargo test --test ext_bench_harness --features ext-conformance -- --nocapture
```

### 7.3 Performance Budgets

| Budget | Threshold | Actual (debug) | Status |
|--------|-----------|----------------|--------|
| Cold load P95 (across extensions) | < 200ms | 106ms | PASS |
| Cold load per-extension P99 | < 100ms | 134ms | FAIL* |
| Warm load P95 | < 100ms | 734us | PASS |
| Warm load per-extension P99 | < 100ms | 926us | PASS |
| Event dispatch P99 (PR mode) | < 5ms | 616us | PASS |

*Debug build only; release builds are 5-10x faster (~5-10ms cold load).

### 7.4 Performance Highlights (2026-02-07)

| Metric | Value |
|--------|-------|
| Median cold load (P50) | 77ms |
| Fastest cold load | 67ms (trigger-compact) |
| Slowest cold load | 126ms (hjanuschka-plan-mode) |
| Median warm load (P50) | 333us |
| Slowest warm load | 836us (jyaunches-pi-canvas) |
| Extensions benchmarked | 100 of 103 |

Reports:
- `tests/perf/reports/ext_bench_baseline.json` — machine-readable baseline
- `tests/perf/reports/BASELINE_REPORT.md` — per-extension breakdown
- `tests/perf/reports/budget_summary.json` — budget pass/fail summary

---

## 8. Best-Effort Compatibility Rules

Compatibility scanner outputs:
- **compatible** (safe)
- **warning** (works but constrained)
- **blocked** (unsafe / unsupported)

The system always **tries to run** with warnings unless `strict` is set.

### 8.1 Known Limitations

Extensions that rely on the following will not work in the Rust QuickJS runtime:

| Limitation | Impact | Workaround |
|------------|--------|------------|
| **npm packages not stubbed** | 5 failures (`openai`, `adm-zip`, `linkedom`, `@sourcegraph/scip-typescript`) | Add virtual module stubs |
| **Multi-file imports across directories** | 4 failures (`../../shared`, `./dist/extension.js`, etc.) | Bundle into single file before loading |
| **Native Node addons** | Blocked | Use hostcalls or WASM |
| **Worker threads / cluster** | Blocked | Unsupported concurrency model |
| **Raw sockets (`net`/`tls`/`dgram`)** | Blocked | Use `pi.http()` connector |
| **Manifest registration mismatches** | 22 failures | Audit manifests against actual registrations |

### 8.2 Supported Node API Shims

The QuickJS runtime provides shims for common Node APIs. See §2A.6 for the
full compatibility matrix. Key supported modules:

- `node:fs` — `readFileSync`, `writeFileSync`, `existsSync`, `readdirSync`,
  `statSync`, `mkdirSync`, `realpathSync`, promises API
- `node:path` — `join`, `resolve`, `dirname`, `basename`, `extname`, `sep`
- `node:os` — `platform`, `homedir`, `tmpdir`, `hostname`, `type`, `arch`
- `node:crypto` — `randomBytes`, `createHash`, `randomUUID`
- `node:url` — `URL`, `parse`, `fileURLToPath`
- `node:child_process` — `spawn`, `exec`, `execSync` (via `exec` capability)
- `node:readline` — basic interface for interactive prompts
- `node:module` — `createRequire` stub

16+ npm package stubs are provided for common third-party dependencies
(`node-pty`, `chokidar`, `jsdom`, `turndown`, `@opentelemetry/*`, etc.).

---

## 9. Adding New Extensions

To add a new extension to the validated corpus:

1. **Place the extension source** under the appropriate corpus directory
   (e.g., `legacy_pi_mono_code/corpus/community/`).

2. **Validate in the TS oracle** — run the extension through the Bun-based
   harness to capture its expected registrations:
   ```bash
   cd tests/ext_conformance/ts_oracle
   bun run validate.ts /path/to/extension.ts
   ```

3. **Add to `VALIDATED_MANIFEST.json`** — merge the oracle output into the
   manifest so the Rust conformance test has a ground-truth comparison.

4. **Regenerate the conformance test** — the `conformance_test!` macro entries
   in `tests/ext_conformance_generated.rs` are generated from the manifest.

5. **Run conformance** — verify the extension passes:
   ```bash
   cargo test --test ext_conformance_generated test_<extension_id> \
     --features ext-conformance -- --nocapture
   ```

6. **Update the catalog** — add an entry to `docs/extension-catalog.json`
   following the `pi.ext.catalog.v1` schema (§1C.4).

If the extension fails conformance, classify the failure (see §1C.5 failure
breakdown) and determine whether a new Node shim, npm stub, or manifest
correction is needed.

## 10. Future Work

- **WASM component runtime** (Tier A) — wasmtime integration with WIT hostcalls.
- **`extc` compiler pipeline** — SWC-based TS→JS bundling + QuickJS bytecode
  precompilation for faster cold loads.
- **Remaining npm stubs** — `openai`, `adm-zip`, `linkedom`,
  `@sourcegraph/scip-typescript`.
- **Multi-file bundling** — resolve cross-directory imports for complex
  extensions.
- **Release build benchmarks** — establish release-mode baselines (expected
  5-10x faster than debug).
