# Changelog

All notable changes to **pi_agent_rust** are documented here.

Versions marked **Release** have published binaries on
[GitHub Releases](https://github.com/Dicklesworthstone/pi_agent_rust/releases).
Versions marked **Tag-only** exist as git tags without a corresponding GitHub
Release (no downloadable binaries). Versions marked **Draft** have an
unpublished draft release on GitHub.

Repository: <https://github.com/Dicklesworthstone/pi_agent_rust>

---

## [Unreleased]

## [v0.2.0] — 2026-08-06 — Release

### Breaking Changes

- **The Rust SDK now boxes in-process prompt results** —
  `SessionPromptResult::InProcess` carries `Box<AssistantMessage>`. SDK callers
  that construct or pattern-match this variant must account for the box.
- **Assistant stop metadata is richer** — `AssistantMessage` adds optional
  structured `stop_details`, `pi::sdk` now exports `StopDetails`, and
  `StopReason` adds `PauseTurn` and `Refusal`. Downstream `AssistantMessage`
  struct literals must initialize the new field (normally with `None`), and
  exhaustive `StopReason` matches must handle the new variants.
- **The minimum supported Rust version is now 1.95** — published crate metadata
  declares Rust 1.95, while repository release builds remain reproducibly
  pinned to `nightly-2026-07-05`.
- **Static model-catalog lookup is now fallible** —
  `pi::providers::static_registry_models` returns `Result<Vec<String>>` so
  malformed, unsafe, or resource-exceeding local catalog data cannot be
  mistaken for a valid empty fallback.
- **Fetched-catalog persistence is provenance-bearing** — the public
  `persist_provider_model_catalog` API now accepts a `ProviderModelCatalog`
  rather than arbitrary provider/model rows, and that catalog's fields are
  private with read-only accessors. The generated on-disk schema advances from
  `pi.models.fetched.v1` to `pi.models.fetched.v2`. Pi preserves v1 bytes;
  move `models.fetched.json` aside first, then run a verified live
  `--fetch-models <provider> --refresh-models --persist-models` refresh to
  create the v2 catalog.
- **Runtime credentials now outrank `models.json` credentials** — explicit,
  ambient, stored, or external-provider credentials are used before a
  provider route's `apiKey`; the route value is now a fallback. Live catalog
  discovery and inference therefore use the same credential precedence.

### Features

- **Native, opt-in subagent orchestration** — the ninth built-in tool can run
  one named child agent, bounded parallel tasks, or a sequential chain. Child
  processes inherit the parent environment (including auth/router variables),
  apply each agent definition's model, reasoning, skill, and configured-or-default
  tool list, stream structured progress, and are cancelled and reaped with their
  parent. Addresses [#132](https://github.com/Dicklesworthstone/pi_agent_rust/issues/132),
  [#144](https://github.com/Dicklesworthstone/pi_agent_rust/issues/144), and
  [#145](https://github.com/Dicklesworthstone/pi_agent_rust/issues/145).
- **Provider and model parity improvements** — Anthropic `pause_turn` responses
  resume with a bounded continuation budget, refusal details remain structured,
  extension-registered providers can stream through the provider factory, and
  the built-in catalog includes GPT-5.6 Sol, Terra, and Luna seeds.
- **Live provider catalogs are usable outside the TUI** — `--fetch-models`
  prints model IDs and exits, `--refresh-models` requires a genuine live
  response, and `--persist-models` atomically records only verified live or
  same-process-cache results in `models.fetched.json`. Persisted provider/model
  membership carries a fetch timestamp and non-secret endpoint/transport
  fingerprint that excludes credential, URL-query, and header values; rows are
  ignored with an actionable error if the current route shape no longer
  matches, while credential rotation remains valid. Separate CLI invocations
  start with an empty in-memory cache. The generated catalog feeds future
  `--list-models` and interactive model pickers without rewriting or overriding
  hand-authored `models.json`. Fixes
  [#150](https://github.com/Dicklesworthstone/pi_agent_rust/issues/150).
- **Retired GitHub Models routing removed** — the `github-models` preset and
  dead upstream autocomplete rows are no longer exposed after GitHub retired
  the service on 2026-07-30. The distinct `github-copilot` native provider
  remains supported.
- **Injectable model credential resolution** — harnesses and SDK embedders can
  load the model registry without ambient process credentials affecting model
  availability, while production continues to use `AuthStorage`.

### Reliability and Release Integrity

- **Truncated tool calls fail explicitly** — a provider token limit reached
  while a tool call is incomplete now ends the turn as an error instead of
  silently treating an unusable partial call as a normal length stop. Fixes
  [#148](https://github.com/Dicklesworthstone/pi_agent_rust/issues/148).
- **File-lock ownership is identity-aware** — long-held locks refresh a
  heartbeat, stale reclaim checks ownership, and displaced owners cannot remove
  a replacement lock directory.
- **Model-catalog filesystem policy is deterministic under root and ordinary
  users** — reads enforce the effective Unix owner/group/other class on both
  files and traversed directories. Generated-catalog persistence preflights
  target and directory read/write/search access before creating directories,
  locks, or temporary files, and rejects final symlinks without mutation.
- **Concurrency-sensitive tests are deterministic** — RPC crash-recovery
  checkpoints, process-wide current-directory mutation, remote-worker fixtures,
  and extension memory/scanner cases now isolate their shared state.
- **Extension corpus scans are read-only by default** — ordinary test runs now
  validate the committed entry-point scan in place and keep auxiliary scan
  results in memory, so an ignored generated file cannot contaminate the
  fail-closed must-pass source snapshot. Maintainers can regenerate scan
  artifacts explicitly with `PI_GENERATE_EXT_ENTRY_SCAN=1`.
- **Release builds no longer compile duplicate process-inspection stacks** —
  the direct `sysinfo` dependency now matches the version already used by the
  runtime. This preserves process-tree behavior while reducing binary-size
  pressure.
- **Embedded release resources are losslessly size-optimized** — deterministic
  gzip and compile-time LZSS encoding preserve the exact source bytes while
  avoiding duplicate large text literals in the executable. The release-only
  LLVM machine outliner further reduces repeated instruction sequences without
  changing development/test profiles; round-trip, CLI-parity, and executable
  hardening checks guard the shipping path.
- **Extension filesystem fallback is more robust** — a denied host `readdir`
  no longer aborts the filesystem shim before later fallback paths are checked.
- **Extension VFS isolation is enforced end to end** — registered roots under
  `/tmp` remain host-backed while unrelated scratch paths stay private per
  extension; cached data, every hop of virtual symlink resolution, and shared
  file-descriptor operations are re-authorized for the active extension. The
  raw shared VFS state is no longer exposed to extension code.
- **Extension reloads use fresh JavaScript realms** — reset-time registry and
  intrinsic checks are hygiene evidence before dropping a realm, not authority
  to reuse arbitrary mutable module/global state. Versioned transpile-cache
  artifacts remain reusable across cold owner-isolated realms.
- **Context-evidence suppression is deterministic** — duplicate stale/unsafe
  suppression records are collapsed per source and reason while the complete
  excluded-item audit trail is retained.
- **Release evidence fails closed** — must-pass extension evidence is bound to
  the exact authoritative inclusion-list identities, corresponding validated
  manifest metadata, source-tree/inclusion-list/manifest digests, run lineage,
  non-skipped per-extension events, and exact declared runtime registrations.
  Stale, smaller, or identity-expanded historical evidence cannot certify a
  release. PiJS tool-call regression inputs now require exactly 2,000
  executable-path-verified perf-profile QuickJS iterations through the
  production extension manager; the 1-call lane gates arithmetic mean latency
  and the 10-call lane gates
  aggregate throughput. Debug, unverified-profile, preview, native-comparison,
  explicitly ineligible, malformed, stale, and uncontrolled host-page-cache
  load records cannot satisfy their respective gates. These checks gate
  performance-claim admission. v0.2.0 remains explicitly
  performance-claims-NOT-authorized because its checked-in budget summary is a
  valid blocked/NO_DATA artifact; publication may proceed only without
  quantitative or global performance claims. The producer and validator share
  the canonical G01–G12 contract. The v2 claim contract also pins the complete
  19-budget inventory and requires every declared budget to have data and pass
  before its global performance-authorization boolean can become true; CI-only
  counters remain additional diagnostics rather than an authorization loophole.
  Per-target build manifests now identify selected sibling-project crates by
  their locked registry version, source, and checksum instead of attributing
  binaries to unrelated sibling repository HEADs.
- **Cargo-audit vulnerability findings are remediated** — the lockfile
  advances `anyhow`, `crossbeam-epoch`, `event-listener`, `memmap2`,
  `plist`/`quick-xml`, and the optional Wasmtime component host to their
  patched release lines. Warning-only upstream maintenance inventory remains
  tracked separately. Wasmtime 47 compatibility also uses a movable owned
  runtime guard and gains real component load, call, trap, and malformed-input
  coverage.

### Internal

- Decomposed the extension monolith behind the stable `src/extensions.rs`
  façade: manager orchestration, protocol/reactor, filesystem, exec mediation,
  permission drift, event coalescing, native/Wasm runtime, and
  characterization-test domains now live in focused `src/extensions/` modules
  while public type definitions and import paths remain in the façade. Addresses
  [#130](https://github.com/Dicklesworthstone/pi_agent_rust/issues/130).
- Extracted the extension compatibility scanner into its own module while
  preserving the public compatibility contract and expanded the conformance,
  governance, provider, SDK, RPC, and swarm evidence suites.

## [v0.1.23] — 2026-07-28 — Release

### Features

- **First-class `max` thinking level above `xhigh`** — `--thinking max`,
  `/thinking max`, ACP `session/set_config_option`, and RPC
  `available_thinking_levels` all accept the new 7th level. Anthropic
  adaptive-thinking models send `output_config.effort: "max"` (Opus 4.6 /
  Sonnet 4.6 accept `max` without `xhigh`, per the documented effort matrix);
  DeepSeek reasoning models send `reasoning_effort: "max"` (with `xhigh`
  keeping its historical `"max"` mapping so existing configs are unchanged);
  models without a `max` wire value clamp to `xhigh`/`high`. Legacy
  budget-based thinking gains a `thinking_budgets.max` slot (default `65536`).
  Fixes [#139](https://github.com/Dicklesworthstone/pi_agent_rust/issues/139).
- **Atlas Cloud provider preset** — `atlascloud` (aliases `atlas-cloud`,
  `atlas`) routes through the OpenAI-compatible chat-completions adapter at
  `https://api.atlascloud.ai/v1` with `ATLASCLOUD_API_KEY` /
  `ATLAS_CLOUD_API_KEY`. From
  [#141](https://github.com/Dicklesworthstone/pi_agent_rust/pull/141),
  implemented independently after verifying the live service.

- **Newer z.ai (GLM) and MiniMax models in the registry** — the model catalog
  now includes z.ai **GLM-5.1** (`glm-5.1`, 200K context) and **GLM-5.2**
  (`glm-5.2`, 1M context) under the `zai` provider, and **MiniMax-M3**
  (`MiniMax-M3`, 1M context) under both `minimax` and `minimax-cn`. The
  upstream provider-id snapshot is updated so the mainland `zhipuai`/BigModel
  endpoint and the `zai-coding-plan`/`zhipuai-coding-plan`/`minimax-coding-plan`
  routing presets also surface these ids. Context windows, max-output ceilings,
  pricing, and reasoning support are taken from the published
  [models.dev](https://models.dev) catalog and verified against the official
  [z.ai](https://docs.z.ai) and [MiniMax](https://platform.minimax.io) API docs.
  Select them with e.g. `pi --provider zai --model glm-5.2` or
  `pi --provider minimax --model MiniMax-M3`. Fixes
  [#115](https://github.com/Dicklesworthstone/pi_agent_rust/issues/115).
- **`/mcp` slash command** — reports MCP (Model Context Protocol) server
  status in the interactive TUI instead of returning "Unknown command". Lists
  any MCP servers an installed extension has registered and clarifies that Pi
  does not read standalone MCP config files (`.agents/mcp.json`,
  `.pi/mcp.json`, `~/.pi/agent/mcp.json`). Fixes
  [#112](https://github.com/Dicklesworthstone/pi_agent_rust/issues/112).

### Bug Fixes

- **`--mode rpc` no longer drops or hangs the turn when stdin closes**
  — piping a single command (`printf '{"type":"prompt",...}' | pi --mode
  rpc`) tore the session down while the prompt task was still starting or
  streaming: current builds silently lost every event after the ack, older
  builds hung after `turn_start`. The RPC loop now drains in-flight work at
  EOF (streaming turns with retries and queued steering/follow-ups,
  extension commands, auto-compaction, background bash), cancels extension
  UI requests that can no longer be answered, guards the activity flags
  against panic/cancellation leaks, and flushes the stdout writer behind a
  shutdown sentinel so the full stream through `agent_end` reaches the
  client. Fixes
  [#137](https://github.com/Dicklesworthstone/pi_agent_rust/issues/137).
- **Startup no longer probes the npm registry for installed packages** —
  installed exact and conventional-range npm specs are now satisfied
  locally (ranges like `npm:pkg@^1.2.0` previously re-ran a full
  `npm install` on every startup, and dist-tag specs could fail startup
  lock verification), `npm root -g` runs at most once per resolution pass,
  and `pi update` refreshes ranges/dist-tags while exact pins stay pinned,
  matching upstream TypeScript pi semantics. Lock entries whose stored
  `pinned` classification predates this change rotate cleanly instead of
  hard-failing verification. No new dependencies. From the report in
  [#140](https://github.com/Dicklesworthstone/pi_agent_rust/pull/140).
- **CI gates repaired on main** — `tests/e2e_transient_retry_resume.rs` is
  registered in the suite-classification and traceability registries
  (caught via
  [#135](https://github.com/Dicklesworthstone/pi_agent_rust/pull/135));
  the provider-metadata snapshot suite reflects reality again (95 canonical
  providers / 51 aliases, including previously unregistered `cursor`,
  `llamacpp`, and `mistralrs`); and `clippy --all-targets -- -D warnings`
  is clean again (six `future_not_send` guard-across-await sites converted
  to `OwnedMutexGuard`).
- **Windows `WSAENOTCONN` retry now also fires for TLS errors surfaced through
  non-`Io` variants** — `is_retryable_not_connected_tls` walks the `TlsError`
  source chain in addition to matching the direct `Io` variant, so a "socket
  not connected" (os error 10057) reported via the TLS library is still
  detected and retried with a fresh connection. Hardens
  [#111](https://github.com/Dicklesworthstone/pi_agent_rust/issues/111) /
  [#106](https://github.com/Dicklesworthstone/pi_agent_rust/issues/106).
- **Streamed tool-call partials carry id/name from the first delta**
  ([#129](https://github.com/Dicklesworthstone/pi_agent_rust/issues/129)),
  **fsync refusals on virtiofs/FUSE filesystems no longer fail writes**
  ([#136](https://github.com/Dicklesworthstone/pi_agent_rust/issues/136)),
  and the extension compatibility layer supports current Pi package APIs
  (pi-subagents 0.34.0 import contract; groundwork for
  [#132](https://github.com/Dicklesworthstone/pi_agent_rust/issues/132)).

### Internal

- Async runtime migrated to **asupersync 0.3.9** with `OwnedMutexGuard` at
  every spawned lock site; the fuzz workspace dropped its stale asupersync
  git-rev pin and re-locked on the published crate.

## [v0.1.22] — 2026-07-10

First version published to crates.io since `0.1.18` (the `v0.1.19`–`v0.1.21`
tags never reached crates.io because the Publish workflow was missing the
`CARGO_REGISTRY_TOKEN` secret; the secret is now configured — see
[#127](https://github.com/Dicklesworthstone/pi_agent_rust/issues/127)).

### Bug Fixes

- **Streaming tool-call arguments now reach RPC/ACP clients** — the #124 fix
  grew the tool-call `arguments` on the provider's internal partial, but the
  partial message that `--mode rpc` / ACP clients actually receive is rebuilt
  in the agent loop, which left `arguments` as `null` on every
  `toolcall_delta` until the terminal event. The agent loop now accumulates
  the raw argument deltas per content index and applies the same best-effort
  partial-JSON completion, for every provider, so snapshot-based IDE
  frontends render large tool calls streaming in live. Verified by an
  end-to-end test that serializes agent events exactly as the RPC surface
  does. Fixes
  [#126](https://github.com/Dicklesworthstone/pi_agent_rust/issues/126).
- **Transient provider failures resume the turn instead of replaying it from
  the user message** — avoids re-executing tool calls that already ran.
  Fixes [#125](https://github.com/Dicklesworthstone/pi_agent_rust/issues/125).

### Maintenance

- Migrated to published **asupersync 0.3.6** (crates.io) and bumped the
  pinned toolchain to `nightly-2026-07-05` (required by `sysinfo 0.39`'s
  `cfg_select!`), with a mechanical clippy sweep for the newer nightly's
  strengthened lints.

## [v0.1.20] — 2026-06-13 — Tag-only

### Bug Fixes

- **Windows connect retries now survive `WSAENOTCONN` ("Socket is not
  connected")** — the HTTP client retries the TCP connect when Winsock reports
  `WSAENOTCONN`, and the error-classification logic walks the full
  `io::Error::get_ref` source chain so the code is detected even when it is
  wrapped several layers deep. A Winsock remediation hint is surfaced when the
  condition is hit. Fixes [#106](https://github.com/Dicklesworthstone/pi_agent_rust/issues/106).

### Internal

- Synced `Cargo.lock` to the released `0.1.19` dependency set.

## [v0.1.19] — 2026-06-11 — Tag-only

### Features

- **ACP dynamic configuration** — ACP mode supports `session/set_model` and
  `session/set_config_option`, so clients can switch the active model and tune
  config options mid-session.
  Fixes [#105](https://github.com/Dicklesworthstone/pi_agent_rust/issues/105).
- **ACP sessions can persist to disk** — `--session-dir` is honored in ACP mode,
  letting ACP-driven sessions be stored and resumed like interactive ones.
  Fixes [#102](https://github.com/Dicklesworthstone/pi_agent_rust/issues/102).

### Bug Fixes

- **System prompt emits date only (no clock time)** — the system prompt now
  carries a date-only stamp instead of a full timestamp, so the leading prompt
  prefix stays byte-stable within a day and provider KV caches are preserved
  instead of being invalidated on every turn.
  Fixes [#103](https://github.com/Dicklesworthstone/pi_agent_rust/issues/103).
- **`llamacpp` and `mistral.rs` are treated as keyless local providers** — these
  local OpenAI-compatible backends no longer demand an API key to be selectable.
  Fixes [#104](https://github.com/Dicklesworthstone/pi_agent_rust/issues/104).
- **Bundled TLS roots + cached connector** — the HTTP client uses bundled webpki
  roots and caches the TLS connector, avoiding slow per-request system trust-store
  parsing (notably the multi-second `SecTrustSettings` stall on macOS arm64).
  Fixes [#101](https://github.com/Dicklesworthstone/pi_agent_rust/issues/101).
- **Snapshot/override entries honored for native-adapter legacy providers** —
  `models.json` snapshot and `models-override.json` entries are no longer
  silently dropped for native-adapter providers (openai-codex, github-copilot,
  google-gemini-cli, google-antigravity).
  Fixes [#100](https://github.com/Dicklesworthstone/pi_agent_rust/issues/100).

### Internal

- Upgraded `asupersync` to `0.3.4`, dropping the `=0.3.2` pin (`0.3.3` was
  yanked).
- Pinned `rust-toolchain` to `nightly-2026-02-19` to stop clippy lint drift, and
  the publish workflow now emits the missing-token error on stdout.
- Docs: model examples use the `openai-completions` API id rather than `openai`.

## [v0.1.18] — 2026-06-05 — Release

### Features

- **TUI front-end is feature-gated behind a default-on `tui` feature** — SDK and
  library consumers can now build without compiling the terminal stack by
  disabling the `tui` feature, while default installs are unchanged.
  Fixes [#98](https://github.com/Dicklesworthstone/pi_agent_rust/issues/98).

### Internal

- The publish workflow fails loudly when `CARGO_REGISTRY_TOKEN` is missing, so
  crates.io publishes can no longer be silently skipped.
  Fixes [#99](https://github.com/Dicklesworthstone/pi_agent_rust/issues/99).
- Formatting and beads export housekeeping.

## [v0.1.17] — 2026-06-04 — Release

### Features

- **Configurable provider-aware HTTP request timeout** — request timeouts are now
  configurable and provider-aware, addressing spurious "Request timed out"
  failures on slow providers.
  Fixes [#90](https://github.com/Dicklesworthstone/pi_agent_rust/issues/90).
- **Dynamic model fetch with caching** — providers can fetch their live model
  list with a 5-minute TTL cache and a static-registry fallback; the static
  fallback is no longer cached so a transient fetch failure does not pin a stale
  list.

### Bug Fixes

- **GitHub Copilot sign-in works out of the box** — a default Copilot
  `client_id` is shipped and the Copilot device flow is wired into `/login`
  with an SSH/headless fallback, so login no longer requires manual client
  configuration.
  Fixes [#97](https://github.com/Dicklesworthstone/pi_agent_rust/issues/97).
- **Idle CPU churn from cursor blink removed** — the interactive front-end stops
  the cursor-blink repaint loop while idle so the async runtime can actually
  park.
- **Kimi-for-coding fallbacks modernized** — prefer K2.6/K2.5 over the legacy
  K2-thinking model as concrete fallbacks.

### Internal

- Release binary size budget raised from 20 MB to 25 MB; no-mock CI now gates
  only new violations via a `.no-mock-allowlist` file; fuzz target forced to
  `x86_64-unknown-linux-gnu` so ASan links against dynamic libc; trimmed 70 more
  stale `ext_conformance` artifact fixtures.

## [v0.1.16] — 2026-05-22 — Release

### Features

- **Configurable tool-iteration cap** — added `--max-tool-iterations <N>` CLI
  flag and `PI_MAX_TOOL_ITERATIONS` env var. Both override the historical
  hardcoded default of 50. Clamped to `[1, 1000]`; invalid or zero values
  fall back to 50 with a warning. Without this, long multi-step agentic
  tasks (multi-file refactors, multi-phase spec implementations) were
  forcibly stopped with no graceful handoff.
- **Soft handoff warning at 80% of the iteration cap** — when an agent
  crosses `(max * 4) / 5` tool iterations, the runtime injects a one-shot
  steering message ("Tool-iteration budget at ≥80%; begin graceful
  handoff per spec…") so the agent can self-pace into an
  `incomplete-handoff` envelope rather than being silently killed at the
  cap. Skipped for caps below 5 to avoid noise on tiny ceilings. Pairs
  with `--max-tool-iterations` to make iteration-aware-handoff protocols
  self-enforceable.

### Bug Fixes

- **Coding-plan providers are now selectable by `--provider` alone** —
  selecting a provider that has no models in the registry (the routing-only
  presets such as `zai-coding-plan`, `minimax-coding-plan`, and
  `kimi-for-coding`) previously failed with `No models available for
  provider …`. Such providers now synthesize an ad-hoc model entry from a
  per-provider default, honoring `config.default_model` only when it is
  paired with the same provider via `config.default_provider`.
- **Ad-hoc model entries resolve credentials from stored auth / env** —
  synthesized entries started with no API key, so `model_entry_is_ready`
  reported them as unconfigured even when valid credentials existed. Keys
  are now resolved when the entry is created (the SAP path keeps its own
  resolver), so readiness reflects reality.
- **Provider default model ids modernized** — `zai`/`zai-coding-plan` default
  to `glm-4.7` (was `glm-4.6`), `minimax`/`minimax-cn`/`minimax-coding-plan`
  default to `MiniMax-M2.7` (was `MiniMax-M2.5`), and the Kimi for Coding plan
  uses its stable virtual model id `kimi-for-coding`. The single defaults
  table now also feeds ad-hoc synthesis, eliminating duplication.
- **Active model auto-switches when it loses credentials** — when the selected
  model's credentials disappear mid-session, the interactive front-end switches
  to a still-ready model instead of failing the next turn.
  Fixes [#81](https://github.com/Dicklesworthstone/pi_agent_rust/issues/81).
- **Actionable hints for host/network-unreachable connect failures** — connect
  errors (host unreachable / network unreachable, e.g. the macOS arm64
  `EHOSTUNREACH` case) now surface a remediation hint instead of a bare OS error.
  Fixes [#88](https://github.com/Dicklesworthstone/pi_agent_rust/issues/88).

### Internal

- **`asupersync` upgraded to `0.3.2`**, fixing a post-session persistence worker
  that spun at ~500% CPU after a session completed.
  Fixes [#83](https://github.com/Dicklesworthstone/pi_agent_rust/issues/83).
- **Large QuickJS Node-shim conformance push** — extensive hardening of the
  embedded-runtime Node API shims, especially the global `Buffer` surface
  (encoding/length coercion, integer read/write bounds and validation, base64/hex
  decoding, `ArrayBuffer` sharing/offsets, byte-swap parity, single-byte
  encodings) plus `fs`/`crypto` shim fixes, tighter extension-VFS path scoping and
  hermetic fixtures, and fail-closed handling of non-primary entrypoint load
  errors. Adds the `@mariozechner/pi-ai` completion + model-registry host bridge.
- **Swarm operations + autopilot tooling** — deterministic swarm-replay engine
  and ingestor, `pi doctor` swarm checks (rch warm-target affinity planner,
  conflict predictor, reservation recommendations, affinity-proof gate), an
  autopilot dry-run next-action planner with budget-drift watcher and failure
  action catalog, a completion-audit generator, and a swarm operations runbook.
- Regenerate `rquickjs` bindings at build time on Android/Termux; track
  `scripts/skill-smoke.sh` and the `pi-agent-rust` skill so the installer
  regression harness passes in clean CI checkouts.

---

## [v0.1.15] — 2026-05-02 — Release

### Bug Fixes

- **Default OpenAI Codex model now prefers GPT-5.5 with xhigh reasoning** —
  startup, setup, and model-candidate resolution prefer `openai-codex/gpt-5.5`
  over older GPT-5.4 defaults, with fuzzy aliases covering GPT-5.5 model names.
- **RPC startup paths no longer accidentally behave like interactive TUI
  sessions** — the `--rpc` shortcut is parsed as an explicit RPC mode, avoids
  incompatible `--print` combinations, and skips interactive-only session-index
  maintenance.
- **Non-interactive exits are deterministic** — the CLI process exits after the
  selected mode returns, avoiding runtime-drop hangs after terminal shutdown.
- **Streaming and tool-call edge cases are fail-closed** — SSE EOF flushing is
  terminal across repeated polls, SIGPIPE trampoline exec failures propagate as
  tool errors, and Bun/Node extension helpers avoid invalid native argument
  shapes.
- **Filesystem and extension tests are confinement-aware** — grep output remains
  relative from symlinked working directories, Bun file-write coverage avoids
  escaping the extension root, and validation tolerates unvendored candidate-pool
  hits without weakening hard gates.
- **Release evidence writers honor off-repo target directories** — performance,
  lifecycle, provider-registry, and scenario harnesses now write artifacts under
  `CARGO_TARGET_DIR` when set, preventing release gates from failing on unwritable
  or missing repository `target/` paths.

### Internal

- Stabilize HTTP, model-registry, model-selector, provider backward-lock,
  non-mock compliance, and perf regression fixtures so the release compile,
  clippy, format, workspace test, and release-build gates run cleanly in the
  high-capacity release workspace.

## [v0.1.14] — 2026-04-28 — Release

### Bug Fixes

- **Slash dropdown Enter accepts highlighted entry** — When the autocomplete
  dropdown is open with a highlighted item (the user pressed Down to navigate
  to a specific entry), Enter now accepts the highlight and runs the selected
  command, matching the dropdown's own footer hint "Enter/Tab accept" and the
  convention used by fzf, vim completion, Slack/IRC slash menus, etc. The
  prior behavior — Enter submits the raw editor contents regardless of the
  highlight — is preserved when no item is highlighted, so users who never
  navigated keep the existing escape-hatch behavior.
  Fixes [#61](https://github.com/Dicklesworthstone/pi_agent_rust/issues/61).
  Regression tests in
  `src/interactive/tests.rs::enter_accepts_highlighted_autocomplete_item` and
  `enter_submits_when_no_autocomplete_item_highlighted`.

- **File mode bits preserved on session rewrite and tool write** — earlier
  release notes (51b7776d) for write-time mode preservation. Bug fix preserves
  permissions across session rewrites that previously stomped them.

### Features

- **User-overridable model list** — Drop a JSON file at
  `<config_dir>/pi/models-override.json` (or set `PI_MODELS_OVERRIDE` to point
  pi at a path elsewhere) to extend the bundled model snapshot at runtime. The
  override file uses the same shape as the bundled snapshot:

  ```json
  {
    "anthropic": ["claude-opus-4-7"],
    "openrouter": ["anthropic/claude-opus-4-7"]
  }
  ```

  Override entries union with the bundled snapshot (set semantics, deduped).
  Missing/blank/malformed files log a warning and are treated as empty so a
  typo never breaks startup. The model catalog cache fingerprint folds in a
  CRC of the override file so memoized consumers refresh correctly when the
  override changes. Documented in `docs/models.md`. Fixes
  [#60](https://github.com/Dicklesworthstone/pi_agent_rust/issues/60). This
  obsoletes the recurring one-line PRs that just add new model IDs to the
  bundled snapshot — drop them in your config instead.

- **claude-opus-4-7 in anthropic snapshot** — Anthropic shipped Opus 4.7 on
  2026-04-28; surfaced in `/model` autocomplete. Mirrors PR
  [#59](https://github.com/Dicklesworthstone/pi_agent_rust/pull/59) (closed in
  favor of independent implementation per project policy).

### Internal

- Refactor the snapshot ↔ override merge into a `merge_provider_model_ids`
  helper for testability.
- Address three nightly clippy lints in `package_manager.rs`,
  `extension_preflight.rs`, and `extensions.rs` that were blocking the
  `cargo clippy --all-targets -- -D warnings` CI gate.
- (Concurrent agent work also merged this cycle — see git log between
  v0.1.13 and v0.1.14 for the full set, including RPC lifecycle event
  refactors and permission recovery test probes.)

### Known Issues

- Seven pre-existing test failures in `src/app.rs::tests::select_model_*`,
  `src/rpc.rs::tests::auto_compaction_*`, and
  `src/session.rs::tests::test_continue_recent_*` predate this release. Tracked
  in beads `bd-d8v93` for follow-up.

---

## [v0.1.12] — 2026-04-23 — Release

### Bug Fixes

- **Actually fix Windows build.** v0.1.11 attempted to work around the published
  `sqlmodel-sqlite` 0.2.1 dynamic-link issue by adding `libsqlite3-sys` with
  `bundled-windows` as a top-level direct dep. That attempt failed because
  `sqlmodel-sqlite` 0.2.1 still declared `#[link(name = "sqlite3")]` (dynamic)
  without importing any item from `libsqlite3-sys`, so rustc elided the
  `libsqlite3-sys` rlib and dropped its build-script static-link directives
  before link time — leaving only the dynamic-link directive, which failed with
  `__imp_sqlite3_*: unresolved external symbol` on MSVC. Upgrading to
  `sqlmodel-sqlite` 0.2.2 picks up
  [`50e1aca`](https://github.com/Dicklesworthstone/sqlmodel_rust/commit/50e1aca)
  (always bundle via `libsqlite3-sys` feature `bundled`) and
  [`c8bb7d7`](https://github.com/Dicklesworthstone/sqlmodel_rust/commit/c8bb7d7)
  (pin the rlib with an explicit `#[link(name = "sqlite3", kind = "static")]`),
  which make the static-link directives actually survive into the link step.
  Fixes [#55](https://github.com/Dicklesworthstone/pi_agent_rust/issues/55).

### Dependencies

- Bump `sqlmodel-sqlite` and `sqlmodel-core` from 0.2.1 to 0.2.2.
- Drop the now-redundant `[target.'cfg(windows)'.dependencies] libsqlite3-sys`
  block and the FreeBSD `libsqlite3-sys` mirror of the same workaround. Both
  are subsumed by `sqlmodel-sqlite` 0.2.2 bundling sqlite on every target.

---

## [v0.1.11] — 2026-04-15 — Release

### Bug Fixes

- **Fix Windows build**: Add `libsqlite3-sys` with `bundled-windows` as a direct
  dependency. The published `sqlmodel-sqlite` crate was missing the
  `[target.'cfg(windows)'.dependencies]` section, causing
  `LINK : fatal error LNK1181: cannot open input file 'sqlite3.lib'` on MSVC.
  Fixes [#48](https://github.com/Dicklesworthstone/pi_agent_rust/issues/48).
- **Fix Windows compiler warnings**: Suppress platform-conditional unused
  variable/mut warnings in `rpc.rs`, `tools.rs`, `doctor.rs`, and
  `session_store_v2.rs` using targeted `#[cfg_attr(not(unix), ...)]` attributes.

### Dependencies

- Bump `sqlmodel-sqlite` and `sqlmodel-core` from 0.2.0 to 0.2.1.

---

## [Unreleased] (after v0.1.9)

Commits since v0.1.9 tag (2026-03-12) through 2026-03-21.

### New Model Definitions

- Add built-in entries for **GPT-5.2 Codex**, **Gemini 2.5 Pro CLI**, and **Gemini 3 Flash** ([`43ddc6f`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/43ddc6f0305fcc22dc574d90dc725572e00a9d29)).

### Auth & Configuration

- Support `$ENV:` prefix in `auth.json` so API keys can reference environment variables instead of storing secrets in plain text ([`266be4c`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/266be4c4774949bc707c2095e6d57ee36d6940dc)).
- Async Kimi OAuth flow, theme picker caching, session index offloading, and config error surfacing ([`943085f`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/943085f6fe3e1c6e5d2c89e5c8bc2e254c8a5bab)).
- Random-port OAuth callback server and viewport scroll clipping fix ([`bda35a4`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/bda35a46ed328c58128fe8da4ed2b892e729822b)).
- Fix OAuth callback `redirect_uri` per RFC 6749 Section 4.1.3 ([`d264bb8`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/d264bb8e31d5e04a89f20b606b7e9ca8ee2865d6), [`c44dfbd`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/c44dfbd2f8e6e6e81f40acf44c98fbc0eeace0a4)).
- Fix config override package toggle persistence ([`8268a8c`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/8268a8c35825c85de24a2d4f1c6618a87efc9f19)).
- Deterministic scope update ordering in package toggle tests ([`73eb0b6`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/73eb0b61123186cbf051f1fa4fd72de3dadf15dc)).

### Security & Reliability (fail-closed hardening)

- Fail closed on invalid extension manifests ([`028be33`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/028be3308e81ede7a793dbc98d1db26c2560b3e8), [`9bb1f8c`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/9bb1f8c61df080d7dc59ca64d2f8474d79b286b4)).
- Fail closed on malformed package manifests ([`ebffa82`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/ebffa82ff9378c7fac14d95c0c5f4e1f8d03d6da)).
- Fail closed on invalid hostcall reactor configs ([`7b0a0b6`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/7b0a0b67e67067061948f03b3d382f3fe199f3d6), [`3621449`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/3621449f0d5e1a35d0e64e5c53b7d9a2da2f6e2a)).
- Cap `randomBytes` native hostcall to prevent OOM DoS; use `sync_channel` for RPC stdout backpressure ([`c5afccf`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/c5afccf553d4bc2ce1da6f93cb6100ecc58cb1d0)).
- Cap WASM `Vec` pre-allocation in `extract_bytes` to prevent OOM on large arrays ([`95d4128`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/95d4128d7a5887683e488d5a91a18d043095224c)).
- Share WASM virtual files via `Arc` and fix UTF-8 read truncation ([`61b400b`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/61b400b9f86decc6b66a069dc8ecded87f712dba)).
- Prevent DoS vectors via unbounded thread creation and indefinite stream blocking ([`a7ecaa3`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/a7ecaa306caeeb7e4aff76a653c9ec7ee3ffbd59)).
- Remove implicit panics from lazy regex initialization and serialization unwraps ([`4c32edc`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/4c32edc6ebbe26ae4a8a5c1e1bead50fc9aa268b)).
- Reject non-regular-file paths in extension `fs_op_read` hostcall ([`a09f2a3`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/a09f2a31c81fa37e4e28f9f2b346674aa34f237e)).
- Normalize repository shorthands and suppress unsafe extension walks ([`53f80eb`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/53f80eb30bc56c93d02f83ebfb8c8ba2b3157473)).

### Session Management

- Prune stale session index rows when project directories disappear ([`d8bc4f4`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/d8bc4f44337ea00d8ec1bafef96472d43ca0704c)).
- Prune stale resume index rows and corrupt recents entries ([`1bd3887`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/1bd38874596b4fb9bdba94425ce3f4b3f554a3d7), [`ddd006a`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/ddd006a0e5a24b0e5c8ac3eb4a5babe03e56c91c)).
- Defer session picker prune on permission errors ([`e510294`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/e510294541ad27b1c76c049e87a00081d2b1b2b2)).
- Harden `SessionStoreV2` newline-heal rebuild path ([`0ee28af`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/0ee28af45a60ce72860200821bbccfa3fa5c2c88)).
- Harden session index and auth async flows ([`609ba33`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/609ba33ffeca4fda53faf5480b6444844996db7b)).
- Clear stale session index names ([`38a37d3`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/38a37d36f24fca78d96b6a0ab82f58a72a52419c)).
- Propagate read errors during JSONL session loading instead of silently skipping ([`a1c725c`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/a1c725c6bde6f4e1c4a2d90ed5a30e84b5d2f29a)).

### Extensions & RPC

- Refine extension JS bridge, RPC protocol, and tool dispatch ([`02b49de`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/02b49de3ae48c5cd2acf8df2a532aa49232dad53)).
- Resolve remaining O(N^2) string concatenations in extension polyfills and WASM memory cloning ([`ec1dfeb`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/ec1dfeba2f0e22ef5dc2a549ee8dfcbd4f213fa7), [`b56b156`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/b56b1565f3da8b85f5dc1aaa0b2cfb6d77c83b00)).
- Fix exec wrapper double-buffering that lost stdout/stderr on close ([`fc1f91f`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/fc1f91fd5bb51ef0c38e0e5cf35b4f7b99f10e3f)).
- Fix RPC extension precedence and queue mode propagation ([`af4f1a7`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/af4f1a78c853e3a0371a61d0c10f1715923c38aa), [`c1f97e6`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/c1f97e62e37cc53b06a6fd0b4a58ee00c6e27bd5)).
- Sync RPC session state with typed SDK ([`14736c1`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/14736c1a5fc222720f21825abb442553dcd3167b)).
- Expand extension stress testing, tool dispatch, and tree view ([`fcf99ca`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/fcf99cad08e3021c96998ded5d02be91e3e57565)).
- Skip empty extension runtime boot ([`4e70a65`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/4e70a65e3ba88fdb5cc6d69e23c9dc4de14f9d43)).

### Interactive TUI

- Allow empty keybinding overrides to unbind defaults ([`60d63a6`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/60d63a678302e72a463f06e553e08987904312ab)).
- Refine keybinding dispatch and tool argument handling ([`d6fc889`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/d6fc889114d0094b2a26d7638349a4a74fc2c0de)).

### HTTP & Networking

- Make chunked transfer-encoding and header parsing tolerant of bare LF line endings ([`d1e7166`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/d1e71664fb5f1e58d73e1c9d4413a06a1a8d59b8)).
- Reject unsupported transfer-coding chains ([`184a2a6`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/184a2a6e7c90e3b1c8afcbd36e58f5a5c07e6ea1)).
- Harden Transfer-Encoding aggregation and outbound header names ([`b613709`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/b613709ff6a77b1d9f67f74e1bcb2b2b0e51e879), [`1df7610`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/1df76106f2bd05e55d27555c5fc71b7d8c62ef6c)).
- Drop caller-supplied `Transfer-Encoding` header from outbound requests ([`b3625e2`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/b3625e25db75e6e2e00e2adf4aba34cc0e39fa28)).
- Stream JSONL migration instead of `read_to_string` for lower peak memory ([`ed47943`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/ed479435b42c64f59c5f1a5c6b44b8a0b84d57a0)).

### Providers

- Avoid duplicating first string stream delta ([`c354a0c`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/c354a0ccf2b426139c0156ed273d966e1dfec2da)).
- Honor native adapter defaults and sync session connector channel ([`e65dcad`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/e65dcadc6034d6b79d0bec4345fff6b39ec6c08b)).
- Expand provider module and e2e live test coverage ([`a5a27a4`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/a5a27a43433cfcc8ca215bb9306490522cc4279c)).
- Normalize bare OpenAI/Cohere origins to `/v1` endpoints and always persist thinking level ([`7145c6e`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/7145c6eab3e3a7d99f1b036a6a349b5754da5c0b)).

### Tools

- Enable grep/find/ls by default and document 8 built-in tools ([`7fc5cc5`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/7fc5cc52cceeaf2a56265d2f3b8f2bffc4316d2a)).
- Discard incomplete bash spill files ([`e8b94d3`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/e8b94d31a5e36f09cc0fee58b1c3e8fa2a834f4c)).
- Clean up incomplete bash spill files ([`e8b81e4`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/e8b81e41db4b1f79b2bbe1c5ba95d77a88c83b80)).

### Performance

- Batch session entry inserts in chunks of 200 for SQLite ([`86368c2`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/86368c203f9f400d0ab7f217b99a9a0b641cf4a9)).
- Skip resolution when all resource categories disabled; deduplicate cached extension entries ([`996dcf3`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/996dcf3c63f0b6b0aa9d70e467b27b344d186119)).

### CLI & Build

- Embed changelog into binary ([`c3385cb`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/c3385cb0eb87cdab45252be3156ae0f8607f8d03)).
- Harden ARM64 Linux release build against future regressions ([`aef5fb8`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/aef5fb8323bac64ede978fbccf9f7bcd08b54b97)).
- Fix SSE retry field parsing ([`f5457f6`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/f5457f6e38d6c2aa54f5166fcdf51adb0d0d3425)).

---

## [v0.1.9] -- 2026-03-12 -- **Release**

Tag: [`81bf62d`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/81bf62d459e525c79bb180c618212e97a326a61d)

A reliability-focused release with deep hardening of the backpressure model,
HTTP RFC compliance, session lifecycle, and the asupersync runtime migration.

### Backpressure & Event Reliability

- Systematically prevent silent message drops under backpressure in the interactive event loop ([`3dcbf0a`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/3dcbf0a554c136372f76246b9f994350b238cedd)).
- Replace `try_send` with backpressure-aware enqueue for extension host action events ([`cc97fa5`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/cc97fa516390e831679430e0ba7a7a656fa96267)).
- Preserve async error events under backpressure ([`ab3ac63`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/ab3ac63817dee4e54184dc2be0a79dbf6c2e1193)).
- Use guaranteed send for `UiShutdown` on bounded event channel ([`1d995da`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/1d995dae22a196b4d48cf8317480cbf682997c51)).
- Send `UiShutdown` event to unblock async bridge after TUI exits ([`bc645f0`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/bc645f03b58cb0b9a35e5b7b6dd3cec8f3987e0a)).

### HTTP Protocol Compliance

- Validate `Content-Length` headers and reject malformed or conflicting values ([`c25efc6`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/c25efc65d9e4b01bb70ed8a8fd2c3674f85b630c)).
- Handle coalesced `Content-Length` headers per RFC 9110 ([`4dbd0d4`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/4dbd0d43de5457de62a022db1937d869eb800643)).
- Treat 1xx/204/205/304 responses as empty-body per RFC 9110 ([`6e6c123`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/6e6c12344fd00f9313c7ae537f670d07ccecf243)).
- Add `write_all_with_retry` to handle transient `Ok(0)` from TLS transports ([`7722a14`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/7722a148d471003863ee3b9049dfa7e3c26d9df5)).
- Flush TLS write buffer on zero-byte retry ([`8dcf945`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/8dcf9458e6bc0dd9fd02b1ef9a1bbd777c2ad51d)).

### Asupersync Runtime Migration

- Migrate compaction worker from std threads to asupersync runtime ([`009c97b`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/009c97ba7d0c1b2c1e90e3c76b0f8b6aa0b93d16)).
- Migrate compaction worker to externally-injected runtime handles ([`95081cd`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/95081cdd0b89b3bab46e68b3f7e80b8d4a3f5ece)).
- Propagate asupersync `Cx` through RPC, agent, interactive, and session layers ([`ae91ad9`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/ae91ad9aafc18a8e4d12c6baa38b0da48a8c63f2)).
- Migrate Session save-path metadata to asupersync async filesystem ([`8179452`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/81794526d84aa96b2f3c8d74d8ef28e4989f5b08)).
- Migrate GrepTool `fs::metadata` to asupersync async filesystem ([`2a36d88`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/2a36d882a70ffd7f1a3b14a8c9ef66d27f10a98d)).
- Add binary body support to fetch shim ([`2edd6c8`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/2edd6c84ac29dca4e9dc2ca019dfafc8b2ee9f9c)).

### Session & Persistence

- V2 sidecar staleness detection with JSONL rehydration fallback and cross-format chain hash verification ([`7d0cc00`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/7d0cc000d0bbb1e9e34c7de3cc0a87e68d5c34fc)).
- Atomic sidecar rebuild with staging/backup swap and trash-based deletion ([`e578369`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/e5783692e4c3fbbf96b0a0e547a8edee71f12eac)).
- Honor SQLite WAL sidecars in metadata and deletion ([`5239849`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/5239849cc3834a3d15917bc7de0862e2d4a5a15c)).
- Unify file stat logic through WAL-aware `session_file_stats` helper ([`4806396`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/4806396c3d5fb5d2c0bce3d3b2b45e7f22a3af5d)).
- Session health validation and extension index NPM package name preservation ([`f9731cc`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/f9731ccc6ad43fa0987ca476c44c4f8e1d3c7be0)).
- Skip re-parsing unchanged session files in session picker ([`6e8b817`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/6e8b817e5f34a17145fccef45e15f9d83baa8c8e)).
- Harden auth migration for malformed entries and fix config path fallback ([`ae105be`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/ae105be3f1e2933192644de1047980d3e2b2b530)).

### Interactive TUI

- Handle agent-busy state gracefully in tree navigation and branch picker ([`66e2c89`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/66e2c89b447f986eaccf295fa3a25dfc3b845ae6)).
- Handle busy session locks correctly in branch navigation UI ([`36640ec`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/36640ec0fb19d7083f04dd0788cd92965f6697de)).
- Consolidate interactive model-switching into `switch_active_model` with thinking-level clamping ([`2614d57`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/2614d57c5b1c6b0b2a0a9e0bafa076c1d9e7c14e)).
- Atomicize model/thinking-level management with session header sync and deduplication ([`720dc56`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/720dc56a10b4a74dac07dec8e05f0e0f9d07d94e)).
- Fix thinking-level fallback on header-less sessions and model-switch gating ([`790a362`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/790a3626f33a26caf0ffbfaf2b4abb5b9e558a2a)).
- Scope tmux mouse-wheel override to current pane, traverse parent dirs for git HEAD ([`60fe95e`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/60fe95eef57e6ed6eb4fa8a1e7ccc2eb8dea04e6)).
- Correct tmux wheel binding save/restore and reduce mouse event noise ([`72a1082`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/72a10823e27faa3de0b1e5add3c49c08e08cbefc)).

### Extensions & Security

- Scope extension filesystem access by extension ID and harden path traversal controls ([`4d6ab94`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/4d6ab94a2e47a09b9a77a2b55a6c42e76f3f2ccb)).
- Heuristic token fallback, module path traversal guard, and config-driven queue modes ([`77a44af`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/77a44af417437ea72284862a05ae0adf523e2c86)).
- Preserve runtime IDs for permission cache ([`08922e4`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/08922e45d9a0de37c33dd9c40cfea8cae4fea18c)).
- Use explicit permissions path, add `empty_at` fallback constructor ([`670c935`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/670c9351bfbf5e9c73b5a2afa9f8c05bfcc68ce1)).
- Extract `safe_canonicalize` to deduplicate path validation ([`6cd8211`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/6cd82114a5fe0f73c79cb5f97e424fd8f1c60e9e)).
- Replace hand-rolled semver parser with `semver` crate for correct pre-release ordering ([`add7336`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/add73369fa65d5e4bf2e76f15d3abf7c1c78a825), [`cf729a2`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/cf729a26e5e6f71a7e2f1a4c83b2a1a5e02d3e1b)).
- Preserve exact bare version constraints in extensions ([`90f60c5`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/90f60c53c5a36e0e9b02dc4d1f0eaa0cdcd52c47)).

### VCR / Replay Testing

- Redact normalized text request bodies and single-pair sensitive form bodies in VCR cassettes ([`ae24ad4`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/ae24ad4ce0fc44c85b67a6f3e93b9e6d64af8dc1), [`7d929dd`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/7d929dd4e4b9bff4d2f71a40d6d9e30ef20f6ed8)).
- Recover poisoned env override lookups and distinguish absent vs explicitly-unset overrides ([`babd3cd`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/babd3cde32cd38a9a09d5bbcbb1ebab3c0a4cace), [`421ad64`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/421ad641fb3fc6d1e17eb3e0aed6f7d8e1d20bc8)).
- Fail closed on zero-baseline telemetry in replay ([`eae5a87`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/eae5a877a3ec1c14aeda26e8f91ac72c91cb8a8d)).
- Report schema mismatches during comparison ([`52527d0`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/52527d08e62e7abe0dbb7e3f64f8e993bcade985)).
- Scope SSE buffer guard to retained tail ([`3f6ebe0`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/3f6ebe0c32aeab8be4ff45b2e8ebf3d0f3e8c53b)).

### Providers

- Preserve decorated URL normalization ([`6ebfef8`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/6ebfef871e35ad0233b059407836a207ac0fbb48)).
- Extension session with compaction state, Unknown credential variant, URL origin validation, and tilde fence support ([`89c92cc`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/89c92ccbf4aa8fa7abaec99a823d179323229255)).
- Harden extension popularity, file tools, and JS-to-JSON conversion ([`f29ae31`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/f29ae315fd3d89d267e92b153d6644ecc7829ae8)).
- Normalize bare official OpenAI/Cohere origins to `/v1` endpoints ([`7145c6e`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/7145c6eab3e3a7d99f1b036a6a349b5754da5c0b)).

### Permissions & Configuration

- Use `BTreeMap` for `PermissionsFile` deserialization for deterministic JSON diffs ([`cbc94ca`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/cbc94ca5d6d0f20f8dc2d4f0e0baf0aab2feece7)).
- Stabilize JSON serialization order for deterministic diffs ([`3537b9c`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/3537b9cf1b6e94ee2c50d5a54a99c5e4df5c3be4)).
- Add schema version validation and proper timestamp-aware expiry checking ([`42b2c7d`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/42b2c7d9b2eb5fd06e2e00d0daee3f1b0ad4f3a7)).

### CI / Build

- Use `macos-15-intel` runner for x86_64 Darwin release builds ([`11c8e6a`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/11c8e6aea625f9cf04dbe1c47257f61f89392b0b)).
- Extract `drain_bash_output()` with selective cancellation semantics ([`9efb65e`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/9efb65eee42fe7e4c31b30e0b3a41282b5e0f3b4)).

---

## [v0.1.8] -- 2026-02-28 -- **Release**

Tag: [`ae35bfe`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/ae35bfe6b3de85c28c80ef3e6998fd02c6ddaca5)

Major feature release introducing the HashlineEditTool, image tool results for
more providers, deep security hardening, and dozens of reliability fixes across
the agent, streaming, and session subsystems.

### New Tools

- **HashlineEditTool**: line-addressed file editing tool that supports edits by hashline reference, reducing ambiguity in multi-edit sessions ([`0b1baad`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/0b1baada2ba62c1e9cf07cf7d981f3a5ba9cb30c)).
- Enable `hashline_edit` in the default tool set and add system prompt guidelines ([`c947acc`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/c947accc9adbf249ab62119edbdb64f04fc346b5), [`332d1e0`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/332d1e068548cbbf9ee29e7d4d841e8254262c40)).
- Add hashline output mode to GrepTool ([`72d8125`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/72d8125dd7ff71b0314d879df56df3a0ba1511d3)).
- Merge overlapping context windows in GrepTool output ([`f68fb76`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/f68fb767ce0dcf2f32962dcf6cba9a84b61fb94a)).

### Provider Improvements

- Support image content in tool results for Azure and Bedrock providers ([`35ed28b`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/35ed28ba7299550799511caf9a6cd97151c59e24)).
- Accumulate streaming tool call deltas instead of overwriting ([`3df7373`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/3df7373cbf102b1f53aaf9cd86435c83b22ea6fd)).
- Per-model reasoning detection for Claude 3 Sonnet/Opus/Haiku, DeepSeek, Mistral, Llama, Gemini variants, and QWQ ([`0189ed4`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/0189ed48a7c85e05fc3e3dd65d0b59a69a6c03d0), [`06595fe`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/06595fea0d81e54aebc9ed69e8d0e6c1a35b3fcf), [`e382c78`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/e382c78a48a26e69c0ccb17a0ea8bc72cb78b993)).
- Consolidate reasoning detection into `model_is_reasoning` ([`b88817b`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/b88817ba73e6fa9c6908da9d781efe66840f65b2)).
- Canonical provider preference and per-model reasoning detection ([`f799a7b`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/f799a7b1dda4e07e5b27f3a9f0e3e84c86a7f5fd)).
- Add `WriteZero` retry to Anthropic provider and reduce idle worker threads ([`012f0f6`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/012f0f67e36a1037816c8f220d253c76bb0a5b52)).
- Include response data in Bedrock/JSON parse error messages ([`e224d5d`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/e224d5d75b1b00c3e7f0b4eb64f22eb1db3aaef0), [`e8321b7`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/e8321b7374abb07fae38df8e51fbc5e68e69aab4)).
- Populate error message metadata and include thinking in response detection ([`5834f24`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/5834f24ea2b524acbeddc56520f522234cab485a)).

### Security Hardening

- Set `0o600` permissions on migrated `auth.json` and session files; shared file locks for auth reads ([`4dafcd9`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/4dafcd9565e466d044e9d16b6bdd7f8d0b285842), [`6ec4ac2`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/6ec4ac22f3b911a8488172952a506cdf901adb73)).
- Prevent zero-filled output on `getrandom` failure in crypto shim ([`96f8187`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/96f818729009493c2fcb8fbcd5d52cbcf0c43fb4)).
- Canonicalize extension read roots before path comparison ([`6e65575`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/6e6557522c4545ef92e3434654aad16930c73844)).
- Block extension `fs_write` outside workspace root (adversarial gap G2) ([`d408bd0`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/d408bd079fc3f682fd225c49ace95fbc1cb5cfc9)).
- Close monorepo escape path traversal bypass ([`c84feac`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/c84feacbc74d3312d1b1110226cf421845d83518)).
- Harden JS extension module resolution with fail-closed empty-roots and canonicalized escape detection ([`0f0a899`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/0f0a8992bf72f32eaea295ce71b0f5a071445a5d)).
- Cap all subprocess pipe reads to `READ_TOOL_MAX_BYTES` to prevent OOM ([`617f571`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/617f571aae11541effbafca5f9ff4869c8fcd85e)).
- Harden JS array limits, process cleanup, mutex recovery, and oscillation guard ([`42a9174`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/42a9174d9d786ec91d669db72f9b1f1bea0ce1e9)).

### Agent & Interactive

- Race tool execution against abort signal for prompt cancellation ([`bf77ad6`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/bf77ad6fe9887368dddb045502c355a4fd9835c2)).
- Prevent context duplication on API retry; strip dangling tool calls on error ([`4a11c20`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/4a11c2091de468df9562476e01795bc75ea28d7d)).
- Emit `TurnEnd` events on error paths; enforce write access in `mkdtempSync` ([`25cd30a`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/25cd30a3ca5822480cca5e8b65a621e85fcc606a)).
- Graceful error recovery during stream event processing ([`888c614`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/888c6144d2e2cc11ba9b6d77c0d9a09455b34bd5)).
- Fix slash menu pre-selection and multi-line input overflow ([`32fce5e`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/32fce5e151bc7b90d3110f1623f96a7bfc0978f7)).
- Add `--hide-cwd-in-prompt` flag ([`37f8361`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/37f83618ccaf3e5c35ff3b5c3ed60eee2c5e2896)).

### Session Management

- Delete sidecar directory when trashing session files ([`efed69b`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/efed69ba916003081c126253ede607a2505455e8)).
- Gracefully handle missing `pi_session_meta` table; reject directory paths in file tool ([`75987eb`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/75987eb5771701de6892edf3d7f08c535b93db5d)).
- Move index snapshot writes to background thread ([`d3dad76`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/d3dad76300d521ae915aa2f85111712f5ee898ec)).
- Always break bash RPC loop after kill; restrict session store truncation to last segment only ([`1bf7b3d`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/1bf7b3df301a0ccb88e3ba9d925128435b7ed446)).

### Tools

- Add `find_rg_binary` helper and respect workspace `.gitignore` in grep/find ([`12cce2e`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/12cce2ed517093932d1c2627b03d006c673a83c2)).
- Use `subarray` for `Buffer.slice` and `getrandom` for crypto randomness in shims ([`6cdfbbe`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/6cdfbbe1bd56bd7185b7ddab7dc8452c3a0791ee)).
- Correct CRLF normalization and between-changes diff context ([`50c9e10`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/50c9e108b469ca962bc52034f797bbff997928f9)).
- Replace unbounded file read with size-limited read in EditTool ([`fa50f24`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/fa50f249bd65a4e1b0b9c2b5d5aae79ebbf2e6ac)).
- Canonicalize file paths in EditTool/WriteTool and harden GrepTool match iteration ([`e4426bd`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/e4426bd9e81f72fd5b5a8b21b89d97c09d5a9fb1)).
- Guard ReadTool image scaling against division-by-zero and simplify line counting ([`cf69a47`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/cf69a474f2bb9f7a9e4c5753fc94a4c2ce0547b3)).
- Rewrite `split_diff_prefix` with explicit byte-walking parser ([`d80d5b4`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/d80d5b4f44bc7eb1ba390a6c4320f15cc66eef5e)).
- CRLF idempotency bug, redundant counter, div-by-zero guard, `kill()` clarity ([`ba6df5e`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/ba6df5e9e22e0b0e0f1b4bd8fc17574b3c9d74e4)).

### HTTP & Streaming

- Handle transient `WriteZero` errors in SSE streaming gracefully (closes #12) ([`5360d79`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/5360d79897d1c486244375eecb8f79f065457faa)).
- Handle EINTR across all streaming read loops ([`b2e7150`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/b2e715038d04cd3bd638f68d3fb67e3c8b51b513)).
- Clamp HTTP buffer consume to bounds ([`fa66d94`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/fa66d9473ccc4ba57fb269b3788dc7372294d2c0)).
- Add `sync_all()` before atomic renames for crash safety ([`eaa63b3`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/eaa63b3c28913cd692818a79645c51a1d3888669)).

### Performance

- Preserve string buffer capacity by replacing `mem::take` with `clone+clear` ([`2927bbd`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/2927bbd0cd4ff0f441a03dfa861717877395d6dc)).

### CI

- Split clippy into per-target-kind gates to avoid rch timeout ([`d12a83a`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/d12a83ae8cddb44137e90000f852e21e40cea7e4)).
- Harden proposal validation paths, file ref quoting, and tree selection arithmetic ([`c603f8f`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/c603f8f142cc3ae2abce09e037bcafd2dea3920a)).

---

## [v0.1.7] -- 2026-02-22 -- **Draft**

Tag: [`5bffab9`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/5bffab9e715fcaa4ec2053b21c2da3e7b8db7e50)

A stabilization release focused on streaming renderer performance, session
index optimization, and CI/build reliability. Draft release (no published
binaries).

### Streaming & Rendering

- Optimize streaming markdown rendering with intelligent format detection ([`57fe905`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/57fe905a94a2ca378f4d8e81ba9b380effafdfc5)).
- Add streaming markdown fence stabilization for live rendering ([`8903571`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/8903571dca2cce22609f1b21ac59ab6235c69d36)).
- Optimize streaming renderer hot path and session serialization ([`c5d8ae6`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/c5d8ae6e56e18ef7f32a85d598dd8486c1dbf5a4)).

### Session

- Add best-effort session index snapshot update helper ([`a5c86f6`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/a5c86f6b1d733919d38b0d7fe8629beb7e1e8781)).
- Optimize session index snapshot update with borrowed parameters ([`c0e86a8`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/c0e86a83cde988a28a563a041698db5d8a47146b)).

### Agent Core

- Harden agent dispatch, SSE streaming, provider selection, and risk scoring ([`0513a80`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/0513a805dc496ce892d8f4f4389f261f74fa6244)).
- Harden abort handling, UTF-8 safety, IO guards, and extension streaming ([`ee12566`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/ee12566c698f13f75fafcf3995b0aa19a3eebb50)).
- Remove local `charmed-bubbletea` patch that breaks CI builds ([`9673414`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/9673414da3ac904aac69e9e5d21662d1c0c8f5c8)).

### Installer

- Prevent `PROXY_ARGS` unbound variable error on bash < 4.4 ([`f9f1c3d`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/f9f1c3d36eb0cdb68fa8516ab561f532a78edcc4)).

### CI & Packaging

- Repair release workflow: stub missing submodule, use ARM macOS runner ([`3357e30`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/3357e3067e9a286c9926c1bb8e785a8f7e397f38)).
- Include `docs/wit/extension.wit` in crate package ([`5bffab9`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/5bffab9e715fcaa4ec2053b21c2da3e7b8db7e50)).

---

## [v0.1.6] -- 2026-02-21 -- **Release**

Tag: [`9dd3b3b`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/9dd3b3b4cd2031e4cc9d9238e33d9fe76f326d9a)

Hotfix release addressing an OpenAI provider lifetime issue.

- Fix OpenAI lifetime issue that prevented streaming completion ([`9dd3b3b`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/9dd3b3b4cd2031e4cc9d9238e33d9fe76f326d9a)).

---

## [v0.1.5] -- 2026-02-20 -- **Tag-only**

Tag: [`c22199d`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/c22199d0c8dc0f204ef1ff38f68d8507237cbfa4)

Performance-focused release with allocation reduction, UI streaming overhaul,
and critical deadlock/memory-leak fixes. No GitHub Release was published.

### Performance

- Eliminate unnecessary heap allocations across providers, session store, and diff generation ([`606fccb`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/606fccb41b6c2eb4bae3835c3e8270887ef7591c)).
- Eliminate redundant deep clones in agent loop and provider streams ([`e9c108c`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/e9c108ca169f59bb5a298ecdae9f2db8c6772a6e)).
- Eliminate intermediate allocations in resource loading ([`6b7caaf`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/6b7caaf38582a4b1ea1ed6d1f7f7899ac88ae487)).
- Reduce allocations across agent core, model catalog, and tool output paths ([`96fec6d`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/96fec6d4379efb5b9590f7d3cae2523adff48249)).
- Fix message ordering in agent loop and pre-allocate session vectors ([`2831682`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/283168272500fe945becf7cfec9281891add32d2)).

### New Features

- Synchronous package resource resolution for config fast paths ([`695ad17`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/695ad17e19b8d35c63eb5a280c623207dc24dd9d)).
- Fast-path `config --show` and `--json` output when no packages installed ([`829dcbc`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/829dcbc3470967d823d383a2c2b40355d826a3e5)).

### Stability Fixes

- Fix potential deadlock: drop mutex guard before async channel send ([`0211bbd`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/0211bbda0534425a013d3a7e5de40e5b16a95c96)).
- Eliminate memory leak in Azure provider role name handling ([`7216f86`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/7216f865c4d6caf0a37dfff6f998c091f7f4d08e)).
- Fix scheduler `has_pending` false positive and clean up session parsing ([`f176d20`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/f176d20a4b5b58db7eeb08bdc23b9a8482e31b94)).

### UI

- Overhaul UI streaming pipeline to eliminate flicker and improve responsiveness ([`9ff9de8`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/9ff9de8c336ac80f5252a9709a7717519ab282db)).
- Harden streaming UI paths and reduce model-list churn ([`8ae9022`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/8ae9022fe89f4f3410b2de348ef36cc446d803a4)).

### Security

- Prevent XML injection in file tag name attributes ([`e9623a0`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/e9623a0ff93243b0e4659c6b95cf1a3b3caf3524)).

---

## [v0.1.4] -- 2026-02-20 -- **Release**

Tag: [`12b2e6e`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/12b2e6ed26d63d4ccc5df973216aa0b6870aa492)

Major security hardening release with path traversal prevention, command
obfuscation normalization, OOM guards, and a complete overhaul of the hostcall
scheduler. Also introduces zero-copy OpenAI serialization and multi-source
message fetchers.

### Security

- **Path traversal prevention**: `safe_canonicalize()` handles non-existent paths via logical normalization with symlink-aware ancestor resolution; module resolution enforces scope monotonicity within extension roots ([`d1982f3`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/d1982f36545de4541d8c5195f7198b6865f22808)).
- **Command obfuscation normalization**: new `normalize_command_for_classification()` strips shell obfuscation techniques (`${ifs}`, backslash-escaped whitespace) before dangerous command classification ([`d1982f3`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/d1982f36545de4541d8c5195f7198b6865f22808)).
- **OOM guards**: depth limits on recursive JSON hashing (128), `MAX_MODULE_SOURCE_BYTES` (1 GB), `MAX_JOBS_PER_TICK` (10,000), SSE buffer limits (10 MB total, 100 MB per-event) ([`0e17d52`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/0e17d5260a71028e598618288c0d5e94d3362c6d)).
- **Fast-lane policy enforcement**: extension dispatcher fast-lane now runs capability policy checks before executing hostcalls ([`d1982f3`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/d1982f36545de4541d8c5195f7198b6865f22808)).
- **Atomic file permissions**: auth storage and Anthropic device ID files use `OpenOptions::mode(0o600)` instead of post-hoc `chmod` ([`d1982f3`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/d1982f36545de4541d8c5195f7198b6865f22808)).

### Provider Improvements

- **Zero-copy OpenAI serialization**: request types use lifetime-parameterized borrows instead of owned `String`s ([`f518bb0`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/f518bb07585a027bd52b96702e40ca40fc28105b)).
- **Empty base URL handling**: all `normalize_*_base` functions now return canonical defaults for empty/whitespace input ([`f518bb0`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/f518bb07585a027bd52b96702e40ca40fc28105b)).
- **Gemini/Vertex robustness**: unknown part types (executableCode, etc.) silently skipped; `ThinkingEnd` events emitted for open thinking blocks ([`f518bb0`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/f518bb07585a027bd52b96702e40ca40fc28105b)).

### Agent Loop & RPC

- **Multi-source message fetchers**: steering and follow-up fetchers are now `Vec`-based with additive registration, enabling RPC + extensions to both queue messages ([`355d7e9`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/355d7e9dc5c2db35f309b109dd1a1e6cbdb39408)).
- **Queue bounds**: `MAX_RPC_PENDING_MESSAGES` (128), `MAX_UI_PENDING_REQUESTS` (64) prevent OOM ([`355d7e9`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/355d7e9dc5c2db35f309b109dd1a1e6cbdb39408)).
- **Error persistence fix**: `max_tool_iterations` error now synced to in-memory transcript ([`355d7e9`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/355d7e9dc5c2db35f309b109dd1a1e6cbdb39408)).

### Session Persistence

- **Batched index updates**: group by `sessions_root` for amortized DB access ([`1df017d`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/1df017dbec80dcbc6baa2fed279c7179750343ab)).
- **`RawValue` frames**: session store v2 uses `Box<RawValue>` to avoid re-serializing payloads ([`1df017d`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/1df017dbec80dcbc6baa2fed279c7179750343ab)).
- **Partial hydration tracking**: `v2_message_count_offset` for tail-mode session loading ([`1df017d`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/1df017dbec80dcbc6baa2fed279c7179750343ab)).

### Tools

- **Correct truncation semantics**: trailing newline no longer inflates line count ([`d86e144`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/d86e1440317181869639f87450befeda92371ad6)).
- **DoS guard**: `BASH_FILE_LIMIT_BYTES` (100 MB) prevents unbounded output reads ([`d86e144`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/d86e1440317181869639f87450befeda92371ad6)).

### Hostcall System

- **Log-sinking batch planner**: preserves global request ordering with log-call buffering ([`9792cf6`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/9792cf6fa209d3364ee437e836090e7ee1fa5e1e)).
- **Generation-keyed ghost cache**: `O(log n)` ghost operations replacing `O(n)` `VecDeque` scans ([`9792cf6`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/9792cf6fa209d3364ee437e836090e7ee1fa5e1e)).
- **FIFO scheduler**: `VecDeque` replaces `BinaryHeap` for monotone-sequence macrotask queue ([`9792cf6`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/9792cf6fa209d3364ee437e836090e7ee1fa5e1e)).

### Installer

- Remove auto-hook installation; add idempotent removal of installer-managed hook entries from prior versions ([`d2ffdbb`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/d2ffdbb6d215eefc7b4c2a9a2ca55ba4e7dc557f)).
- Bound network fetch latency during post-install steps ([`c7a3b2b`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/c7a3b2bb38c13e5daf18675653211af6a4fff5c7)).

### Other

- Extension events use camelCase serialization matching TypeScript wire format ([`0e17d52`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/0e17d5260a71028e598618288c0d5e94d3362c6d)).
- Doctor checks for `rg` and `fd`/`fdfind` dependencies ([`0e17d52`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/0e17d5260a71028e598618288c0d5e94d3362c6d)).
- Gist URL parser rejects profile links and non-canonical paths ([`0e17d52`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/0e17d5260a71028e598618288c0d5e94d3362c6d)).
- TUI markup parser rewritten with explicit state machine ([`0e17d52`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/0e17d5260a71028e598618288c0d5e94d3362c6d)).
- Explicitly shut down extension runtimes before session drop to prevent QuickJS GC assertion ([`fa49f58`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/fa49f5826a319a0ffeca1677b3f71cc781c3da60)).
- 9 new regression tests covering security fixes and edge cases ([`8b1db8a`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/8b1db8aba18fdf54f41581c152f7b61eaa97ae82)).

---

## [v0.1.3] -- 2026-02-19 -- **Release**

Tag: [`6637f26`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/6637f266e5fb2463d8ca9f11dd426a1644c85dbb)

Re-enables the dual JS/TS + native-Rust extension runtime, introduces
argument-aware runtime risk scoring with heredoc AST analysis, and adds a
live-provider extension validation tool.

### Extension Runtime

- Re-enable JS/TS extension runtime alongside native-Rust descriptors, restoring the dual-runtime model ([`ee78a58`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/ee78a581d632de533f5da8bfcc3472c45db96e13)).
- Recognize JS/TS files as valid extension entrypoints in the package manager ([`e9f3c9e`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/e9f3c9ea2555034a0ee2588e962b462dfeb7a94c)).

### Security

- **Argument-aware runtime risk scoring** with DCG integration and heredoc AST analysis via `ast-grep` ([`ad26a4f`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/ad26a4f9292cc40c61db38604f5db197b5f5eea5)).

### Testing & Validation

- Add `ext_release_binary_e2e` tool for live-provider extension validation against real (non-mocked) provider responses ([`409742e`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/409742e56c5a29bfde3e40c833f5c24d059e3d7b)).
- Harden release-binary extension harness for OAuth-first auth and stable capture ([`2b41b09`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/2b41b096e6042d246de882e086628a32d5d26eba)).
- Overhaul scenario harness with setup merging, shared context, and parity improvements ([`93ac8d9`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/93ac8d9f5875635bae10ebc38b76385b2c2abb9c)).
- Add `normalize_anthropic_base` unit tests and proptest ([`374f8e6`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/374f8e6a67936ce9b267d1f22098f3338133fa58)).

### Models & HTTP

- Add `codex-spark` registry entry and `xhigh` thinking support ([`cef709c`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/cef709cc7e5a91fc4787bcf1ab362e3a2c677067)).
- Make default request timeout env-configurable with explicit no-timeout mode ([`74e8f1f`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/74e8f1f1c8701aaf32927c579aa6945d4f0c0fd6)).

---

## [v0.1.2] -- 2026-02-18 -- **Release**

Tag: [`27bdebc`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/27bdebc43406046e37e705ce9a35f1bceae96e7b)

Focused on OpenAI Codex provider correctness and a comprehensive credential
resolution overhaul adding OAuth support for multiple providers.

### OpenAI Codex Provider

- Add all required Codex API fields (`instructions`, `store`, `tool_choice`, `parallel_tool_calls`, `text.verbosity`, `include`, `reasoning`) ([`0485fb4`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/0485fb4b04fd7f0a64d08b0cf9a1b90a91e40cde)).
- Send system prompt as top-level `instructions` field ([`2a56374`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/2a563748f9f5bb76f1e6ebc0bd2fe3b9e3fec4e8)).
- Skip `max_output_tokens` (unsupported by Codex API), use actual thinking level for `reasoning.effort` ([`1d7610b`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/1d7610b0c69a41a08a973ea6cc4c7af3f2b4c7d3)).
- Tolerate missing `Content-Type` header in streaming response ([`27bdebc`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/27bdebc43406046e37e705ce9a35f1bceae96e7b)).

### Auth & Provider Overhaul

- Case-insensitive provider matching across all provider lookups ([`8671312`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/8671312a85cd78a01e28fb3a6c3fce58b04dff2a)).
- Native OAuth support for OpenAI Codex, Google Gemini CLI, and Google Antigravity ([`14e5016`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/14e5016d3c00ceb02a6a2e46c2dae78e5cdc0c62)).
- Kimi Code OAuth support and credential resolution overhaul ([`8671312`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/8671312a85cd78a01e28fb3a6c3fce58b04dff2a)).
- Convenience aliases for Kimi, Vercel, and Azure providers ([`2581af0`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/2581af0e25a58caee4c5ca7ee5c22f5c7b14784e)).
- OAuth CSRF validation, bearer token marking, gcloud project discovery, and Windows home directory support ([`63265e5`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/63265e5de1eab07a17b1ad98dc78c4de4e76b4de)).
- Anthropic base URL normalization and OAuth bearer lane ([`efba9c8`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/efba9c8311eca10fe70a5c5e8e80bf8a0c3c8ab4)).
- Keyless model support for models that do not require configured credentials ([`705f692`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/705f6929b38f6c0c3a5a8c55aeb66b6c77fde8e8)).
- Filter `/model` overlay to show only credential-configured providers ([`9e350a5`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/9e350a53eb73b6fadf5a75b23b69c2a07855ce4c)).

### Installer

- Add offline tarball mode, proxy support, and agent hook management ([`be404aa`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/be404aa4e7d6de60805f8bb8ba82e95e47d92f65)).
- Reorder download candidates, suppress 404 noise ([`6025912`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/60259125dbc07e4df3fd5e37fbe0fcaf16d7f44a)).
- Handle missing SHA256SUMS and prioritize archive candidates ([`7cc84de`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/7cc84de8fe0cf86d31c7a4e0e3fb11e4d39d3f6c)).

---

## [v0.1.0] -- 2026-02-17 -- **Tag-only**

Tag: [`f8980ad`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/f8980ad6c92bc5daeeb534d25d7ae0653db19101)

First tagged milestone. The complete from-scratch Rust port of
[Pi Agent](https://github.com/badlogic/pi) by Mario Zechner, built on
[asupersync](https://github.com/Dicklesworthstone/asupersync) and
[rich_rust](https://github.com/Dicklesworthstone/rich_rust). No GitHub Release
was published for this tag.

### Core Architecture

- Single static binary, no Node/Bun runtime dependency.
- Built on `asupersync` structured concurrency runtime with HTTP/TLS and `Cx`-based capability contexts.
- Terminal UI powered by `rich_rust` (Rust port of Python Rich) with markup syntax, tables, panels, and progress bars.

### Agent & Providers

- Full agent loop with streaming token output, extended thinking support, and conversation branching.
- Anthropic, OpenAI Chat Completions, Azure, Bedrock, Gemini/Vertex, Cohere, Kimi, and Ollama provider implementations.
- Custom SSE parser with UTF-8 tail handling, scanned-byte tracking, and chunk boundary normalization.

### Tools (7 built-in)

- `read` -- file contents and image reading with automatic resizing.
- `write` -- file creation and overwrite.
- `edit` -- surgical string replacement.
- `bash` -- shell command execution with process tree management and timeout.
- `grep` -- content search with context lines.
- `find` -- file discovery by pattern.
- `ls` -- directory listing.
- All tools include automatic truncation (2000 lines / 50 KB) and detailed metadata.

### Session Management

- JSONL-based session persistence with conversation branching and tree visualization.
- Session picker with resume, recent session tracking, and project-scoped sessions.
- Model/thinking level change tracking within sessions.

### Extensions

- QuickJS-based JS/TS extension runtime (no Node or Bun required).
- Native-Rust descriptor runtime for `*.native.json` extensions.
- Node API shims for `fs`, `path`, `os`, `crypto`, `child_process`, `url`, and more.
- Capability-gated hostcall connectors (`tool`/`exec`/`http`/`session`/`ui`/`events`).
- Command-level exec mediation blocking dangerous shell signatures before spawn.
- Trust-state lifecycle (`pending`/`acknowledged`/`trusted`/`killed`) with kill-switch audit logs.
- Hostcall reactor mesh with deterministic shard routing and bounded queue backpressure.
- Sub-100 ms cold load (P95), sub-1 ms warm load (P99).
- 224-entry extension catalog with per-extension conformance status.

### Interactive TUI

- Multi-line text editor with history and scrollable conversation viewport.
- Model selector (`Ctrl+L`), scoped model cycling (`Ctrl+P`/`Ctrl+Shift+P`).
- Session branch navigator (`/tree`).
- Real-time token/cost tracking.
- Context-aware autocomplete for `@` file references and `/` slash commands with fuzzy scoring.
- Login/logout slash commands.

### Three Execution Modes

- **Interactive** (`pi`) -- full TUI with streaming, tools, session branching.
- **Print** (`pi -p "..."`) -- single response to stdout, scriptable.
- **RPC** (`pi --mode rpc`) -- line-delimited JSON protocol for IDE integration.

### Performance Foundation

- Allocation-free hostcall dispatch and zero-copy tool execution.
- Fast-path bypass for io_uring and allocation-free shadow sampling.
- Enum-based session hostcall dispatch replacing string matching.
- Session save hotpath optimization and tree traversal for large sessions.
- NUMA-aware slab pool and replay trace recording to hostcall reactor.
- Extension runtime from QuickJS/JS migrated to native Rust during this cycle.

### CI & Quality

- CI with clippy, rustfmt, test, and benchmark gates.
- Release workflow for Linux (x86_64, aarch64), macOS (x86_64, aarch64), and Windows targets.
- Conformance test suite with 224/224 vendored extension scenarios.
- FrankenNode compatibility test harness for Node.js detection and Bun shim rejection.
- Benchmark comparison reports against legacy TypeScript implementation.

### Installer

- `curl | bash` installer with platform detection, SHA256 verification, and `legacy-pi` alias creation.
- Session migration command and credential pruning.
- Clipboard via `arboard` and Copilot/GitLab OAuth providers.

---

## Pre-v0.1.0 (2026-02-02 -- 2026-02-17)

Initial development from first commit ([`37b6c7b`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/37b6c7b0e95912828da53024f53e70942474aa8b))
through to the v0.1.0 tag. This period covers the entire from-scratch port
including core architecture, all providers, the extension system, the
interactive TUI, and the benchmark/conformance infrastructure.

Key early commits:

- Initialize Rust port project with guidelines and legacy code ([`37b6c7b`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/37b6c7b0e95912828da53024f53e70942474aa8b)).
- Implement core Rust port foundation ([`53a8f7d`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/53a8f7d791e7cbbd489c73e1d541c20ffa0fc504)).
- Add SSE parser and fix compilation ([`36fcd89`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/36fcd89dd4092f6681c22db7f3e2f1b5cd39a75c)).
- Integrate rich_rust for terminal UI ([`149e54e`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/149e54eda1436765c61a43299b117a60a656c224)).
- Implement OpenAI Chat Completions provider ([`6dd3d70`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/6dd3d705b82306ccc9f69196b4902f35e34f9e1a)).
- Complete pi_agent_rust MVP with session picker and test fixes ([`a72dfca`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/a72dfca875b53af63ea5b2458252c6e0d7fb9498)).
- Add session branching, expanded conformance fixtures ([`21c735d`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/21c735d455f60d40fb9ad0c7730cfe01d480beb6)).
- Wire interactive TUI and add extension docs ([`7694d65`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/7694d659077b0721f575987b0661cc906499557c)).
- Add package manager, extensions, and fix Unicode panic ([`b2ff486`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/b2ff4862f27a4ee6aba5bb2ccbbcceaa1a0e2c16)).
- Port RPC mode, conformance, and benchmarks ([`cd23248`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/cd232486c0d1b0cf32cab77c4b3d87e74ed7a0c8)).
- Add login/logout slash commands to TUI ([`d8a093a`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/d8a093a3e8dcbab8a0e2d85a68e8c87f38f5c88e)).
- Migrate extension runtime from QuickJS/JS to native Rust ([`d20f236`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/d20f2369fef9f3dfbb3c6a5bd93c60e1d08f1c99)).
- Migrate clipboard to arboard and add Copilot/GitLab OAuth providers ([`91d3f0b`](https://github.com/Dicklesworthstone/pi_agent_rust/commit/91d3f0b1a77bb10c5dbc42a1e2f6b5c3fa5e4be8)).

---

[Unreleased]: https://github.com/Dicklesworthstone/pi_agent_rust/compare/v0.2.0...HEAD
[v0.2.0]: https://github.com/Dicklesworthstone/pi_agent_rust/compare/v0.1.23...v0.2.0
[v0.1.23]: https://github.com/Dicklesworthstone/pi_agent_rust/compare/v0.1.22...v0.1.23
[v0.1.22]: https://github.com/Dicklesworthstone/pi_agent_rust/compare/v0.1.21...v0.1.22
[v0.1.20]: https://github.com/Dicklesworthstone/pi_agent_rust/compare/v0.1.19...v0.1.20
[v0.1.19]: https://github.com/Dicklesworthstone/pi_agent_rust/compare/v0.1.18...v0.1.19
[v0.1.18]: https://github.com/Dicklesworthstone/pi_agent_rust/compare/v0.1.17...v0.1.18
[v0.1.17]: https://github.com/Dicklesworthstone/pi_agent_rust/compare/v0.1.16...v0.1.17
[v0.1.16]: https://github.com/Dicklesworthstone/pi_agent_rust/compare/v0.1.15...v0.1.16
[v0.1.15]: https://github.com/Dicklesworthstone/pi_agent_rust/compare/v0.1.14...v0.1.15
[v0.1.14]: https://github.com/Dicklesworthstone/pi_agent_rust/compare/v0.1.13...v0.1.14
[v0.1.12]: https://github.com/Dicklesworthstone/pi_agent_rust/compare/v0.1.11...v0.1.12
[v0.1.11]: https://github.com/Dicklesworthstone/pi_agent_rust/compare/v0.1.10...v0.1.11
[v0.1.9]: https://github.com/Dicklesworthstone/pi_agent_rust/compare/v0.1.8...v0.1.9
[v0.1.8]: https://github.com/Dicklesworthstone/pi_agent_rust/compare/v0.1.7...v0.1.8
[v0.1.7]: https://github.com/Dicklesworthstone/pi_agent_rust/compare/v0.1.6...v0.1.7
[v0.1.6]: https://github.com/Dicklesworthstone/pi_agent_rust/compare/v0.1.5...v0.1.6
[v0.1.5]: https://github.com/Dicklesworthstone/pi_agent_rust/compare/v0.1.4...v0.1.5
[v0.1.4]: https://github.com/Dicklesworthstone/pi_agent_rust/compare/v0.1.3...v0.1.4
[v0.1.3]: https://github.com/Dicklesworthstone/pi_agent_rust/compare/v0.1.2...v0.1.3
[v0.1.2]: https://github.com/Dicklesworthstone/pi_agent_rust/compare/v0.1.0...v0.1.2
[v0.1.0]: https://github.com/Dicklesworthstone/pi_agent_rust/compare/37b6c7b0...v0.1.0
