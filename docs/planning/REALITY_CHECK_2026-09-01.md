# Reality Check — pi_agent_rust — Historical Baseline (2026-09-01) & Current Refresh (2026-11-12)

> **Current Canonical Refresh (`2026-11-12`, `HEAD = fb5190eeee9e808c203d043b471d28ee794aaeab`):** Jump to **[§11. End-to-End Reality Check & Bridge Plan — 2026-11-12](#11-end-to-end-reality-check--bridge-plan--2026-11-12)** below. Sections 1–10 are preserved intact as the `2026-09-01..03` (`v0.4.0` era) record; this file is revised in place per `AGENTS.md` (no file proliferation).

# Reality Check — pi_agent_rust — 2026-09-01

Reality check performed against README.md, AGENTS.md, docs/program-governance.md,
docs/releasing.md, docs/perf-budgets-recipe.md, the Beads database, the GitHub
issue tracker, the checked-in evidence artifacts, and the source tree at
`origin/main` (5bd3e353, tagged `v0.4.0`). Local checkout was `f4df5dff`,
4 commits behind origin. No Cargo, RCH, or DSR build was run for this check
(AGENTS.md forbids direct Cargo/RCH as a quality path; the DSR recipe is not
registered on this host). Every "WORKING" verdict below is therefore a
code-and-shipped-binary verdict, not a fresh compile/test claim.

Previous reality check: 2026-08-23 (recorded in bd-sog97). It found 23/26
vision groups WORKING in code and the evidence trail "dark and ownerless".
This check re-validates that finding and adds what changed since.

---

## 1. Headline

**The product is real; the proof and the release pipeline are not caught up.**

- The code delivers essentially the whole README surface: 548,618 lines across
  222 source files, zero `todo!()`/`unimplemented!()`, module-reachability gate
  clean (140 reachable, 3 allowlisted, 0 unreachable), ~16,600 `#[test]`
  functions, 369 integration test files. The shipped `v0.3.0` binary runs
  correctly here (help, 50+ providers, 211 models, RPC `get_state`, `doctor`).
- Since v0.3.0 (2026-08-21) there were 901 commits, dominated by an
  Aug 24-25 swarm wave (533 commits in two days). FTUI became the default
  interactive stack on 2026-08-25 (`--classic` selects the charmed stack).
- **But**: 43 beads are `in_progress` (10 P0, 32 P1), all created Aug 24-27,
  every one of them ending in a note of the form "static fix landed, executable
  DSR/Cargo proof HOLD while 1-minute load >= 10". Host load at check time was
  55 on 16 cores. Nothing in the repository records a quality-gate run against
  any commit after the wave.
- `v0.4.0` is tagged on origin, `Cargo.toml` says 0.4.0, and CHANGELOG marks it
  "Release", but there is **no GitHub release** for it. The only published
  release is still v0.3.0.
- Every checked-in evidence gate is red or stale: `budget_summary.json`
  `claim_readiness=blocked`; extension must-pass `fail` (206/208);
  `full_suite_verdict` `fail` (2026-08-04); newest e2e run `not_ready`
  (2026-08-24). `budget_summary.json` is also internally inconsistent
  (header says 12 PASS / 5 FAIL / 2 NO_DATA; its own `budget_results` array
  says 16 PASS / 3 FAIL / 0 NO_DATA).
- Several evidence beads closed on 2026-08-28 were closed as "script shipped,
  run blocked" rather than as the outcome their title promises (see §5).
- README has drifted from the code in visible ways (§6): it never mentions
  FTUI or `--classic`, still describes the charmed/bubbletea stack as *the*
  interactive architecture, omits five opt-in tools that exist, and its FAQ
  says web browsing and image generation are out of scope while
  `src/browser.rs` and `generate_image` ship.

---

## 2. Vision checklist (README + AGENTS.md promises, tested against code)

Status key: WORKING / PARTIAL / UNPROVEN / STUB / NOT_STARTED / NO_BEAD.
"Bead" column lists the live (open or in_progress) beads that touch the goal.

| # | Goal | Source | Status | Bead coverage | Evidence |
|---|------|--------|--------|---------------|----------|
| 1 | Single native `pi` binary, installable from DSR-published releases via `install.sh` | README Quick Start, Installation | **PARTIAL** | none for v0.4.0 publish | v0.3.0 published 2026-08-22, DSR-built on operator hosts (build manifest: `dsr 0.1.2`, "no GitHub Actions"). v0.4.0 tag exists, Cargo=0.4.0, CHANGELOG says Release, `gh release view v0.4.0` = not found. v0.3.0 assets have `SHA256SUMS` only, not the per-asset `.sha256` sidecars README promises. |
| 2 | Streaming responses with extended thinking; custom SSE parser | README Features | **WORKING** (unproven live at HEAD) | bd-fouvy (streams ending before completion markers) | `src/sse.rs`, `src/http/`. RPC smoke on v0.3.0 works. No live-provider run recorded at HEAD. |
| 3 | 11 native provider modules + OpenAI-compatible presets, case-insensitive aliases, `--list-providers`, `--fetch-models` | README Providers/FAQ | **WORKING** structurally, **UNPROVEN** live | bd-x23nj (GitLab wire), bd-1cun1 (OAuth metadata), bd-sa57e P0 (models.json identity), bd-rchdj/bd-gm481 (failover primary) | 11 modules present in `src/providers/`. `pi --list-providers` lists 50+ providers, `--list-models` 211. bd-provider-live-validation-11-xme9d closed 08-28 with "initial run with no creds set" — no fresh live evidence. |
| 4 | Tiered built-in tool surface (13 essential in schema, discoverable via `xdev`, 18 in a default session) | README "28 Built-in Tools" | **WORKING**, docs drift | bd-4i212 (README out-of-scope FAQ) | `src/xdev.rs` tier table matches; CLI default list = 18 tools. Code also ships `browser`, `computer`, `inspect_image`, `generate_image`, `tts` (opt-in) that README never lists; README count "28" vs its own enumeration (29) vs real (~34). |
| 5 | Native `subagent` tool (single/parallel/chain) and `/tan` background children | README Subagents | **WORKING** | bd-f7tr4 (/tan card scoping) | `src/subagents.rs` uses `current_exe()`; `/tan` wired through hub roster. |
| 6 | Session persistence: JSONL v3 tree, SQLite index, v2 sidecar, `pi migrate`, BPE-aware compaction, checkpoints/retry | README Sessions, Deep Dive | **WORKING**, hardening unproven | bd-qxdfd P0 (fail closed on corrupt JSONL), bd-pwqrr (index refresh fail-closed), bd-35xad, bd-m83oo, bd-yn7ud, bd-afvdt | Modules present and wired; P0/P1 hardening fixes are "statically implemented" with no recorded test run. |
| 7 | Four execution modes: interactive (FTUI default), print, RPC, ACP | README Four Execution Modes | **WORKING**, TUI defects open | bd-2crrf (duplicate AgentSession init), bd-q66i1, bd-uio4v, bd-5jfkl (ACP transitions), bd-dexy7 | `main.rs:1881` selects FTUI unless `--classic`. RPC verified on v0.3.0. Open GH: #195 (heading colors/table alignment), #198 (ask hang; fix in 402ff9cd, unreleased). |
| 8 | Extension runtime: QuickJS + native descriptors, capability policy, exec mediation, trust lifecycle, kill switch, workspace TOFU, 223-corpus conformance | README Extensions | **WORKING**, gate red | bd-4t6oz P1 (split tool registry bypasses undo/workspace policy), bd-yllbn, bd-2ojzi, bd-8m21l, bd-sog97.28/.29 | must-pass gate: 206/208 pass, 2 marckrenn-pi-sub failures (triaged 08-28); stretch 10/19. Hermetic clean-checkout run reportedly yields 143/208 (bd-sog97.29). |
| 9 | MCP client (stdio + streamable HTTP) with trust gating | README tools table, CHANGELOG v0.4.0 | **WORKING**, 6 P0 bugs in flight | bd-c6cy9, bd-b2xdr, bd-qv95g, bd-ubjal, bd-z847t (all P0), bd-8alfn | `src/mcp/` 4 modules, 456 KB. All six P0s have "static implementation complete" notes and no executable proof. |
| 10 | LSP (14 ops), DAP (29 ops), eval kernels, github, security_scan, jobs, hub | README tools table | **WORKING** | bd-9zmyf P0 (job session scoping), bd-mg6s5, bd-y84fr, bd-aehbm, bd-wfcu7 | Modules present (`src/lsp/`, `src/debug/dap.rs`, `src/eval/`, `src/security_scan.rs`, `src/jobs.rs`). |
| 11 | Security: exec mediation, secret filtering, SSH URL router, package-subcommand trust gate | README Security | **UNPROVEN** | bd-t2360 P0 (SSH injection), bd-c1do1 P0 (package trust), bd-rgz8b, bd-gawl8 | Fix notes say "confirmed and statically fixed"; no gate run. |
| 12 | Performance targets: startup <100 ms, binary <48 MiB, idle RSS <50 MB, 60 fps | AGENTS.md targets, README Why Pi | **PARTIAL** (claims correctly withheld) | bd-sog97.5 (cold-load), bd-sog97.4 (tool-call data), bd-sog97.19/.27/.20 | Per-budget: 16 PASS, 3 FAIL (`ext_cold_load_simple_p95` 11.9 ms vs 5 ms; `tool_call_latency_mean`, `tool_call_throughput_min` no real data). Source commit e178a73d (Aug 27), not v0.4.0. |
| 13 | DSR is the exclusive quality/build/release authority; Actions permanently disabled | AGENTS.md, README, docs/releasing.md | **PARTIAL** | bd-csywa, bd-yj126, bd-5by7n | v0.3.0 was DSR-built. But: recipe lives only in the maintainer's `~/.config/dsr/repos.yaml` (this host's DSR registry has no pi_agent_rust entry, "no runs recorded", no signing keypair); GitHub Actions is still **enabled** at repo level with live `on: push`/tag triggers; no minisign in `install.sh`; crates.io publish on HOLD; immutable-tag ruleset check missing. |
| 14 | Release-integrity evidence system reaching `claim_ready` (bd-sog97) | README Claim-Integrity, epic | **PARTIAL** | bd-sog97 (27 closed / 3 in_progress / 4 open) | RI-AUTH not reached; RI-PHASE1 open; several children closed as "blocked on RCH". |
| 15 | README/docs describe the shipped product accurately | README citation convention, program-governance | **PARTIAL** (drift) | bd-4i212 only | See §6. |
| 16 | Quality recipe runs green (fmt, clippy, tests, conformance, installer, reachability) | AGENTS.md Compiler and Test Checks | **UNPROVEN at HEAD** | none owns "run it and record it" | No repository artifact records a passing gate after 2026-08-21. Last GitHub CI runs (Aug 19-20) all failed; those lanes are retired anyway. |
| 17 | Windows native support | GH #182 (user request), README ships windows zip | **UNKNOWN / NO_BEAD** | none | v0.3.0 ships `pi-windows-amd64.zip`; issue asks for "direct support"; no bead. |
| 18 | Model-facing `current_time` tool | GH #207 (2026-09-01) | **NOT_STARTED / NO_BEAD** | none | No such tool in `src/`. |
| 19 | Host-mediated compaction bridge for pi-better-compaction; compact deadline parity | GH #167, #178 | **WORKING** per CHANGELOG v0.4.0 | none | Shipped in v0.4.0 tree; issues remain open pending release. |

Working: 8 goals fully in code with no known blocking defect (2, 4, 5, 8-runtime, 10, 19, plus the tool and subagent surfaces).
Partial or unproven: 9. Not started: 1. Unknown: 1.

---

## 3. Beads landscape

| Metric | Value |
|---|---|
| Total beads | 3,067 |
| Closed | 3,009 (98%) |
| Open | 11 |
| In progress | 43 |
| Blocked | 8 (bv shows none actionable) |
| Deferred | 4 (incl. epic bd-63x3v, 96 closed children) |
| Ready to work | 5 |
| In-progress beads with any commit referencing their id since v0.3.0 | 7 of 43 |
| In-progress beads created 2026-08-24..27 | 43 of 43 |
| Last bead-tagged commit | 2026-08-28 |
| Commits after that | 24 (GH-issue driven, 08-31 and 09-01) |

Interpretation: the swarm stopped on Aug 28 with 43 bugs mid-flight. Their
fixes are probably inside the Aug 24-27 wave commits (which do not carry bead
ids), but nothing recorded them as compiled, tested, or gated. The 98% closure
rate is the "bead completion illusion" the reality-check method warns about:
the remaining 2% is almost entirely P0/P1 correctness and security work plus
the entire evidence trail.

---

## 4. The five questions

### 4.1 What IS working right now

- The shipped v0.3.0 binary: install, help, provider/model catalog, `doctor`,
  RPC protocol, print mode, interactive mode (with the defects in §4.2).
- In code at HEAD: everything in the vision checklist rows 2-10 and 19. The
  feature surface described in README exists, is wired (reachability gate
  proves every `pub mod` has a production call site), and has a very large
  unit/integration test corpus. The Aug 23 reality check's "23/26 groups
  WORKING" stands; this pass adds FTUI-by-default, the compaction bridge,
  CA-cert support, and `prompt_cache_key` as new working surface.
- The math-driven control stack in README ("Math at a Glance") is implemented
  and reachable; bd-math-reachability-evidence found 6 of 7 techniques
  statically reachable and IPS/WIS/DR lacking a per-decision production path.

### 4.2 What is NOT working or not proven

1. **Executable proof is dark.** No recorded fmt/clippy/test/conformance run
   exists for any commit since v0.3.0. 43 in-progress beads (10 P0) are parked
   on "gate HOLD". The v0.4.0 tag was cut on top of that state.
2. **v0.4.0 is not released.** Tag + version bump + CHANGELOG "Release" heading
   exist; no GitHub release, no artifacts, no `.sha256`/`.minisig` inventory.
3. **Evidence gates are red or stale**: perf claim readiness blocked; must-pass
   206/208 (and 143/208 hermetic per bd-sog97.29); full-suite verdict fail
   (Aug 4); e2e summary `not_ready` (Aug 24); `budget_summary.json` header
   counts contradict its own results.
4. **Known user-facing defects** on the default FTUI stack: #195 (heading
   colors, table alignment), #198 (ask hang; fix committed, unreleased),
   duplicate AgentSession initialization at startup (bd-2crrf), input-card
   atomicity (bd-q66i1).
5. **Security/trust hardening unverified**: SSH injection (bd-t2360), MCP
   trust/transport set (5 P0s), extension hostcall registry split (bd-4t6oz),
   package-subcommand trust (bd-c1do1), corrupt-JSONL fail-closed (bd-qxdfd).
6. **Release pipeline half-built**: no minisign in installer, no signing key,
   DSR recipe not portable off the maintainer's Mac, GitHub Actions still
   enabled at the repository level despite "permanently disabled" policy.
7. **Docs drift** (§6).

### 4.3 What is blocking

- Host load and RCH posture: the load-admission rule (1-minute load < 10) has
  been unmet on the swarm host; RCH is `degraded`; no build hosts cached in
  DSR here. Every gate-dependent bead is waiting on that.
- DSR recipe locality: `dsr quality --tool pi_agent_rust` only works on one
  machine. There is no in-repo recipe file, so no other host or agent can run
  the authoritative gate.
- No owner for "run the gate, record it, and adjudicate the 43 beads". RI-AUTH
  (bd-sog97.27) is the closest but is scoped to perf claims.
- Signing trust root not provisioned (bd-yj126 explicitly refuses placeholder
  keys).

### 4.4 Would completing all open + in-progress beads close the gap?

**No.** It would close most of the code-correctness gap (the 43 bugs) and the
perf-evidence gap (bd-sog97), but it would leave:

- no bead that runs and records the authoritative quality gate at HEAD;
- no bead that publishes v0.4.0 (or reconciles the tag/CHANGELOG with reality);
- no bead that makes the DSR recipe reproducible outside one laptop;
- no bead that disables GitHub Actions at the repository level;
- no bead for README/FTUI drift, the tool inventory, or the stale FAQ beyond
  bd-4i212;
- no bead for GH #182 (Windows) or #207 (`current_time`);
- outcomes that were "closed" without being achieved (§5) and now have no
  live owner: fresh 11-provider live E2E, real `pijs_workload` data, the full
  phase-1 perf refresh.

### 4.5 Vision goals with NO bead

| Gap | Severity |
|---|---|
| Run the DSR quality recipe against v0.4.0 source, record the artifact, and adjudicate the 43 in-progress beads (close, reopen, or waive each with proof) | Critical |
| Publish v0.4.0 through DSR (5 targets, per-asset `.sha256`, `.minisig`) or retitle CHANGELOG to "Tag-only" until it is | Critical |
| Make the pi_agent_rust DSR recipe a checked-in, portable file (repos.d entry + install step + preflight) so any host can run the gate | Critical |
| Disable GitHub Actions at repository level (settings) and neutralize `on:` triggers in retained workflow files, per AGENTS.md | Major |
| README: FTUI as default, `--classic`, tool inventory incl. 5 opt-in tools, FAQ scope line, TUI architecture section, dev docs consistent with DSR-only | Major |
| Fix `budget_summary.json` header/results inconsistency (partly bd-sog97.20) and rebind to the v0.4.0 source SHA | Major |
| Real (non-synthetic) tool-call latency/throughput evidence; fresh live provider E2E with credentials | Major |
| GH #182 Windows native support investigation | Minor (unknown scope) |
| GH #207 `current_time` tool | Minor |

---

## 5. Closed beads that did not deliver their titled outcome (audit sample)

| Bead | Title claims | Close note says |
|---|---|---|
| bd-provider-live-validation-11-xme9d | Fresh 11-provider live E2E run | Harness shipped; "initial run with no creds set" |
| bd-tool-call-throughput-canonical-o3ubk | Produce pijs_workload data | Script shipped; emits "synthetic-stub record set" when the workload binary is not built; budgets still FAIL |
| bd-ri-phase1-full-refresh-rndeg | Fresh full DSR perf refresh | Closed as `scripts_and_schemas_shipped_dsr_run_blocked_on_rch` |
| bd-math-reachability-evidence-k0ap2 | Prove every math technique fires in production | 6/7 static-reachable, 1 FAIL (IPS/WIS/DR) |

These should be reopened or replaced by outcome-scoped beads, and the
practice of closing on "script shipped" should be stopped: a script is not
the evidence.

---

## 6. Documentation drift inventory

| Location | Says | Code says |
|---|---|---|
| README Four Execution Modes, Interactive TUI Architecture (L1682-1730) | charmed_rust/bubbletea Elm loop is the interactive mode | FTUI is default since 2026-08-25; charmed stack only via `--classic` (aliases `--classic-tui`, `--charmed`, `--bubbletea`); README has 0 mentions of ftui/classic |
| README "28 Built-in Tools" | 28 tools, enumerates 29 | `xdev.rs` tiers + `tools.rs` registration: ~34 incl. `browser`, `computer`, `inspect_image`, `generate_image`, `tts` (opt-in, setting-gated) |
| README FAQ "Why isn't X included?" (L2788) | web browsing, image generation out of scope | `src/browser.rs`, `media_tools::GenerateImageTool` exist (bd-4i212 in progress) |
| README Performance Engineering prose | 12 PASS / 5 FAIL / 2 NO_DATA | Same artifact's `budget_results`: 16 PASS / 3 FAIL; README evidence table already reflects 3 failing |
| README Distribution contract | every DSR release ships per-archive `.sha256` sidecars | v0.3.0 (the only release) ships `SHA256SUMS` only |
| CHANGELOG v0.4.0 | "Release" | No GitHub release exists |
| docs/development.md | `rch exec -- cargo build/test ...` | AGENTS.md/README: contributors must not invoke Cargo or RCH directly |
| docs/tui.md | generic layout description | no FTUI/inline-mode/`--classic` content |
| AGENTS.md Key Files | `src/tools.rs` — 9 built-in tools | 30+ |

---

## 7. Bridge plan sketch (superseded by §10, kept for the record)

### Gap A — Restore executable truth (Critical)
**Current:** no gate run recorded after v0.3.0; recipe only on one Mac; 43 beads parked.
**Target:** `dsr quality --tool pi_agent_rust` runnable from a checked-in recipe on any registered host; a run recorded against the v0.4.0 source SHA; each of the 43 in-progress beads closed with the run id, reopened with a failing test, or formally waived.
**Plan:**
1. Add `dsr/pi_agent_rust.yaml` (or `.dsr/repos.d/`) to the repo mirroring the maintainer's registry entry (6 checks, 5 targets, target-dir override per docs/perf-budgets-recipe.md §3) plus a one-line `dsr repos add` install step in docs/releasing.md.
2. Extend `scripts/perf/preflight_dsr_recipe.sh` to accept a non-Mac DSR path and to assert the recipe file and registry agree.
3. Run the recipe on a host with headroom (or wait for load < 10); store the run summary under `docs/evidence/` with schema + SHA binding.
4. Adjudicate the 43 beads against that run. Any bead whose acceptance tests are absent gets a companion test bead.
**Success:** evidence artifact with `git_commit == v0.4.0 SHA`, all six checks green; `br list --status=in_progress` empty or every remaining item has a failing-test reference.

### Gap B — Finish v0.4.0 honestly (Critical)
**Current:** tag + CHANGELOG "Release", no artifacts, no signatures, Actions enabled.
**Target:** DSR-published v0.4.0 with 5 archives, per-asset `.sha256`, `.minisig`, `install.sh` verifying minisign; or CHANGELOG relabelled "Tag-only" until then.
**Plan:** provision the minisign trust root (bd-yj126), wire installer verification with fail-closed regressions, run `dsr build`/`dsr release`, run DSR public verification, then disable Actions at repo level (`gh api -X PUT repos/.../actions/permissions -f enabled=false`) and gate `on:` triggers in retained workflow files.
**Depends on:** Gap A.

### Gap C — Evidence coherence and real data (Major)
**Current:** blocked perf readiness, inconsistent header counts, synthetic tool-call data, stale must-pass, no live provider run.
**Target:** `budget_summary.json` regenerated from v0.4.0 source in strict mode with coherent counts; real `pijs_workload` data; `ext_cold_load_simple_p95` under 5 ms or a dated waiver (RI-WAIVER); must-pass 208/208 or waived; 11-provider live E2E with credentials recorded.
**Plan:** finish bd-sog97.19/.20/.27; reopen outcome beads for provider live E2E and pijs data; profile simple cold-load (transpile cache warm path).
**Depends on:** Gap A for the run lane.

### Gap D — Ship-blocking defects on the default stack (Major)
**Current:** #195, #198 (fix unreleased), bd-2crrf, bd-q66i1, bd-4t6oz, the P0 MCP/trust/SSH set.
**Target:** each closed with a production-path test in the recorded gate run.
**Plan:** prioritize by user visibility: #198 verify → #195 → bd-2crrf single-session startup → bd-4t6oz registry unification → P0 MCP set → remaining P1s.

### Gap E — Documentation truth (Major)
**Plan:** README sections for FTUI/`--classic`/inline mode; tool inventory rewritten from `xdev.rs` + `tools.rs` (essential / discoverable / default-enabled / opt-in incl. browser, computer, media trio); FAQ scope line; perf prose numbers bound to the artifact's `budget_results`; distribution contract wording matched to real asset inventory; docs/tui.md and docs/development.md aligned with DSR-only; AGENTS.md Key Files tool count. Add a README-vs-code drift test where one does not exist (tool inventory, default flag list).

### Gap F — Untracked user requests (Minor)
**Plan:** bead for GH #182 (scope: ConPTY/Windows Terminal behaviour, `#195` overlaps); bead for GH #207 `current_time` (essential-tier candidate, trivial); refresh `tests/e2e_results` with a run that is not `not_ready`.

### Dependency order
A → (B, C, D in parallel) → E (docs written against gated reality) → F.

---

## 8. Verification plan after bridge work

- `dsr quality --tool pi_agent_rust` recorded green at the release SHA.
- `gh release view v0.4.0` lists 5 archives + `.sha256` + `.minisig`; `install.sh` verifies a signature on a clean host.
- `budget_summary.json`: `claim_readiness.status != blocked` or an explicit waiver ledger entry per failing budget; header counts equal `budget_results` histogram.
- `must_pass_gate_verdict.json`: `status = pass` at the release SHA.
- `br list --status=in_progress` empty; every closed bead from the Aug 24-27 wave cites the run id.
- `gh api repos/.../actions/permissions` returns `enabled: false`.
- README tool inventory test and FTUI section present; `rg -c ftui README.md > 0`.

---

## 9. What changed on 2026-09-01 after this check (same session)

Done and pushed with the commit that carries this section:

- **Gap A, partially.** The DSR quality recipe is now portable: `.dsr/repos.yaml`
  (registry subset, six checks) plus `docs/releasing.md` /
  `docs/development.md` / `docs/perf-budgets-recipe.md` instructions;
  registered on hetzner2; dry-run plans 6/6. First real run exposed that
  `rch exec` from a `/data/tmp` git worktree fails worker path normalization
  and silently compiles locally, so the recipe now runs Cargo under
  `RCH_REQUIRE_REMOTE=1` (fail-closed) with raised timeouts. A recorded green
  run against a release SHA is still outstanding (see the commit message and
  bead comments for the run that was attempted).
- **Gap B, governance half.** GitHub Actions disabled at the repository level
  (`actions/permissions` → `enabled: false`); recorded in `docs/releasing.md`.
  Publishing v0.4.0 is now tracked by bd-ghfu4.
- **Gap C.** Exact header-vs-rows inconsistency recorded on bd-sog97.20;
  README prose states it. Three evidence beads that had been closed on
  "script shipped" were reopened with incident comments
  (bd-provider-live-validation-11-xme9d, bd-tool-call-throughput-canonical-o3ubk,
  bd-ri-phase1-full-refresh-rndeg).
- **Gap D, sizing only.** bd-2crrf and bd-4t6oz carry exact file/line anchors
  and candidate fixes; not implemented (no fast ftui/extension test loop on
  this host).
- **Gap E.** README, AGENTS.md, `docs/tui.md`, `docs/development.md`, and the
  FTUI module doc now describe the shipped product (FTUI default, `--inline`,
  `--classic`, 35-tool inventory incl. settings-gated tools, FAQ, distribution
  contract, evidence numbers). `scripts/check_readme_evidence_freshness.py`
  went from FAIL (2 uncited rows, 2 mismatched bindings) to PASS.
- **Gap F.** GH #207 shipped as the `current_time` essential-tier tool. Two
  implementations landed within an hour (this session's `src/current_time.rs`
  and the maintainer's `CurrentTimeTool` in `src/tools.rs`, commit d39d3366,
  which adds a `timezone` parameter and the system-prompt clock guideline);
  the registry now uses the maintainer's, `src/current_time.rs` is no longer
  declared in `lib.rs` and awaits deletion approval. GH #182 scoped into
  bd-oyckr.
- **Gate evidence.** DSR run `20260901T201627-2472296` at f97387cd: fmt PASS,
  `cargo check --locked --all-targets` PASS (first recorded compile of the
  tree since v0.3.0), clippy FAIL on two `format_push_string` errors in the
  since-removed `src/current_time.rs`, test lane cancelled after 22 lib-test
  failures and a 38-minute stall. Run `20260901T211852-2715600` at 762fd8d5:
  fmt PASS, check PASS, clippy FAIL on three `unnecessary_literal_bound`
  errors in the maintainer's new `CurrentTimeTool` impl, test lane 8327 ok /
  20 FAILED and then stuck on
  `rpc::tests::auto_compaction_rejects_stale_session_snapshot_with_paired_end_event`
  (unbounded wait). A baseline re-run of the failing set at 08485a20 (the
  v0.4.0 tree before this session) shows **20 of those failures already
  present in v0.4.0**; the other 2 were this session's and are fixed. The 20
  are classified by root cause on **bd-x8mn7** (ask-card guard ×12,
  compaction admission ×2, RPC persistence surfacing ×2, approval
  dual-confirm ×1, stale JS receipts ×1, perf-build control paths ×2). The
  receipt, perf-build, parser-contract, clippy, and hang-to-failure fixes are
  in the follow-up commit; the ask-card, compaction-admission, RPC, and
  approval clusters are left to their owning beads with exact anchors.
- **Follow-up (2026-09-02).** Run `20260901T224048-3018634` at 49112a21:
  fmt, `cargo check --all-targets`, and `clippy --all-targets -D warnings`
  all PASS (first clean trio at any SHA); its test lane was polluted by
  worker disk pressure and cancelled. Root causes since resolved with
  targeted remote runs: the 7 direct-injection ask-card tests (pending-id
  registration), the approval dual-confirm fixture, the compaction
  "exclusion window" test (split-turn summary suffix; the probe trail also
  proves auto-compaction admission and apply work in a healthy session), and
  two channel-based ask tests whose single-option fixtures failed
  `validate_request` (`MIN_OPTIONS = 2`). Still owner-decided on bd-x8mn7:
  the compaction "rejects stale" test (provider never entered; now bounded
  by a watchdog), two RPC persistence-surfacing tests, three
  `stream_delta_batcher` turn-error tests, and the FTUI ask-forwarder close
  race.

---

## 10. Bridge plan (Phase 2, full): every gap between the tree and the vision

Written 2026-09-02 against origin/main after commit f709de6a, with the
2026-09-01/02 gate evidence in hand. This section is the plan document for
the reality-check flow; revise it in place (do not fork it). Phase 3a turns
it into beads. Ordering is by vision impact, not by ease. Each gap names its
current state with file anchors, the target state, success criteria a
skeptic can re-execute, the implementation steps, dependencies, complexity
(S/M/L/XL), the vision goals it serves (§2 numbering), and whether the beads
that already exist would close it.

Principles that shape every item below:

- **Proof beats prose.** A gap is closed by a recorded run bound to a SHA
  (DSR run dir, artifact with `git_commit`), never by a note that says
  "implemented statically".
- **One lane, fail-closed.** `dsr quality --tool pi_agent_rust` from
  `.dsr/repos.yaml`, Cargo through `RCH_REQUIRE_REMOTE=1 rch exec`, run from
  `/data/projects/pi_agent_rust` (or another normalizable checkout), tree
  not edited while it runs.
- **No ceremony.** No new certificates, ledgers, or dashboards. Where a check
  is needed, it is a test in the gate or a line in an existing artifact.
- **Owners decide semantics.** Where a failing test could be fixed by
  changing code or the test, the plan names the decision and the anchor
  instead of guessing.

### 10.1 Critical gaps (the vision is undeliverable without these)

#### Gap C1 — The quality gate is red at HEAD and has never completed

**Current state.** The lane now runs (§9). fmt and `cargo check --locked
--all-targets` pass at f97387cd/762fd8d5. `cargo clippy --all-targets
-D warnings` passes with the follow-up fixes (remote run 2026-09-02, exit 0).
The lib test binary had 20 failures that already exist in the v0.4.0 tree
(bd-x8mn7) and hung on one test; after the follow-up commits the expected
remaining lib failures are: cluster B compaction admission (1–2), cluster C
RPC persistence surfacing (2), and 5 ask tests that install a real channel
(`interactive::agent::stream_delta_batcher_tests` ×3,
`interactive::keybindings::tests::quit_cmd_schedules_shutdown_when_event_queue_is_full`,
`interactive_ftui::tests::installed_ftui_ask_forwarder_observes_close_before_dispatch`).
The 369 integration test targets, the examples, and the benches have **never**
been executed through the gate at any post-v0.3.0 SHA, so their state is
unknown. `tests/installer_regression.sh` and
`scripts/check_module_reachability.py` pass individually.

**Target state.** `dsr quality --tool pi_agent_rust` reports `passed` (6/6
checks executed) at a named SHA on origin/main, with the run dir retained,
and the same SHA is what v0.4.x ships from.

**Success criteria.**
- [ ] `~/.local/state/dsr/quality-logs/pi_agent_rust/<run>/` shows
      `check-1..6` all `exit_code: 0` and the run JSON `status: "passed"`,
      `snapshot_before == snapshot_after`.
- [ ] `check-4.log` contains a `test result: ok` line for the lib binary and
      for every integration target, with zero `FAILED` lines and no test
      reporting "has been running for over 60 seconds" without finishing.
- [ ] The run's `git_commit` equals the SHA recorded on bd-csywa and the tag
      that ships (Gap C2).

**Implementation.**
1. Lib suite (bd-x8mn7): resolve clusters B and C by owner decision
   (`src/rpc.rs` compaction admission around `maybe_auto_compact` and the
   conformal admission wiring from bd-conformal-drive-compaction-6hx16;
   `src/rpc.rs:9404/9495` stdin-close persistence surfacing per bd-m83oo);
   fix the 5 channel-based ask tests (they install `install_channel_ui` and
   then see `execute` complete immediately — instrument `AskTool::execute`'s
   early-return path under test to find which `Err` it returns).
2. Integration lane: run the full gate once; expect a second wave of
   failures (network-dependent, `rg`/`fd` absent on workers, provider
   credential tests). Classify each into: real defect → fix; environment →
   gate the test on a capability probe (not `#[ignore]`); credential-bound →
   mark as the live-provider lane (Gap M2) and skip in the quality recipe via
   an explicit feature/env, recorded in `docs/testing-policy.md`.
3. Hang policy: any test that awaits an agent/runtime event gets a wall-clock
   bound. Pattern: the watchdog thread in
   `rpc::tests::auto_compaction_rejects_stale_session_snapshot_with_paired_end_event`
   (the `asupersync::time::timeout` form does not fire on a bare
   `RuntimeBuilder::current_thread()` runtime). Add the pattern to
   `docs/testing-policy.md` and apply it to the other `block_on` tests in
   `src/rpc.rs` that await provider entry.
4. Make the lane the merge gate in practice: the auto-commit sessions push
   to `main` without running it (Gap O1). Until that is fixed, run the gate
   on every push that touches `src/` or `tests/` from the swarm host and
   record the run id on the bead the commit cites.

**Dependencies.** None for step 1–3; step 4 depends on Gap O1.
**Complexity.** L (lib suite M; integration lane unknown, budget XL).
**Vision goals.** 16, and indirectly 6, 7, 8, 9, 11.
**Would existing beads close it?** Partially: bd-x8mn7 covers the lib
failures; bd-q66i1 and bd-m83oo own two clusters. Nothing owns the
integration lane run or the hang policy → new beads.

#### Gap C2 — v0.4.0 is tagged and unpublished; the release pipeline is half built

**Current state.** Tag `v0.4.0` = 5bd3e353, `Cargo.toml` 0.4.0, CHANGELOG
heading says "Release", `gh release view v0.4.0` → not found. v0.3.0 is the
only published release; it was DSR-built on operator hosts but ships an
aggregate `SHA256SUMS`, no per-asset `.sha256`, no `.minisig`. `install.sh`
has cosign support and no minisign support; DSR on hetzner2 has no signing
keypair; the build authority file `~/.config/dsr/repos.d/pi_agent_rust.yaml`
exists only on the release operator's machine. bd-ghfu4 tracks publish-or-
relabel; bd-yj126 tracks minisign; bd-5by7n tracks the immutable-tag ruleset
check; crates.io publication is HOLD by policy.

**Target state.** v0.4.x published through DSR from a gate-green SHA with the
strict inventory (5 archives + per-asset `.sha256` + `.minisig` +
`install.sh`), DSR public verification green, installer verifying the
signature, CHANGELOG heading truthful at every moment.

**Success criteria.**
- [ ] `gh release view v0.4.x --json assets` lists exactly the strict
      inventory; `dsr verify` (public release verification) exits 0.
- [ ] Clean-host `curl … install.sh | bash -s -- --version v0.4.x` installs a
      binary whose `pi --version` prints that version, with the installer
      log showing signature verification, not just checksum.
- [ ] `git show <tag>:CHANGELOG.md` heading matches the published state
      (Release) and no unpublished tag carries "Release".

**Implementation.**
1. Immediately: relabel the v0.4.0 CHANGELOG heading "Tag-only" (the
   legend at the top of the file defines it) in a commit that says why, or
   publish. Do not leave the contradiction.
2. Provision the minisign trust root (operator action; no placeholder key —
   bd-yj126's no-claim), pin the public key in the installer and in
   `release_contract.minisign_public_key_file`, add fail-closed installer
   regressions (missing/swapped/wrong-key/bad-signature) to
   `tests/installer_regression.sh`.
3. Check the build authority file into the repo next to `.dsr/repos.yaml`
   (`.dsr/repos.d/pi_agent_rust.yaml`) so `dsr repos validate` can run on any
   host; keep host-specific paths under `host_paths`.
4. Cut v0.4.1 (not v0.4.0: the tag is already public and immutable by
   ruleset) from the first gate-green SHA; run `dsr build`, `dsr release`,
   `dsr verify`; then flip the CHANGELOG heading.
5. Add the live ruleset check DSR needs (bd-5by7n) before the release, or
   record explicitly that it was verified by hand for this release.

**Dependencies.** C1 (green SHA); operator-held secrets for step 2.
**Complexity.** M (mostly operator steps; installer regressions S).
**Vision goals.** 1, 13.
**Would existing beads close it?** Partially: bd-ghfu4 + bd-yj126 + bd-5by7n
cover it if all three close; the checked-in build authority file is new.

#### Gap C3 — 43 P0/P1 beads are "in progress" on static evidence only

**Current state.** 42 beads (10 P0) created 2026-08-24..27 end in "static fix
landed, executable proof HOLD". Their referenced files all exist at HEAD; the
fixes are probably inside the Aug 24–27 wave. The security subset matters
most: SSH URL router injection (bd-t2360), package-subcommand trust
(bd-c1do1), MCP trust/transport/denial/delivery/discovery (bd-b2xdr,
bd-c6cy9, bd-qv95g, bd-ubjal, bd-z847t), corrupt-JSONL fail-closed
(bd-qxdfd), extension registry split (bd-4t6oz, also Gap C4), capability
prompt queueing (bd-yllbn), hostcall deadline cancel (bd-2ojzi).

**Target state.** Every one of the 42 is closed with a run id from Gap C1
and the specific test names that prove its acceptance criteria, or reopened
with a failing test, or formally waived by the owner.

**Success criteria.**
- [ ] `br list --status=in_progress` contains none of the 42, and each
      closing comment names the DSR run dir and ≥1 test function that
      exercises the bead's acceptance criteria (mutation-sensitive where the
      bead demands it).
- [ ] For the security subset, the named tests include a planted negative
      (an injection/trust-bypass attempt that must fail closed).

**Implementation.**
1. Group the 42 by test file (the scan in this session found all referenced
   files present; extend it to extract test function names from bead notes).
2. After C1 step 1 lands, run the gate; for each bead, grep the run's
   `check-4.log` for its named tests; close with evidence or reopen.
3. For beads whose notes name no test, the owner writes one before closing;
   "static review" closes nothing (AGENTS.md).

**Dependencies.** C1.
**Complexity.** M (mechanical once the gate is green; the security subset
needs a reviewer to read the planted negatives).
**Vision goals.** 6, 8, 9, 11.
**Would existing beads close it?** Yes, they are the beads; what is missing
is the evidence, which C1 supplies.

#### Gap C4 — Finished code that is not wired (two product defects)

**Current state.**
- bd-2crrf: every default (`ftui`) launch builds a full classic
  `AgentSession` with extension runtime + MCP (`src/main.rs:1738–1760`,
  `1905–1960`), then `drop(agent_session)` at `src/main.rs:2228` and builds a
  second SDK session in `interactive_ftui::run` (`src/main.rs:2308`). Double
  extension boot on every start; init-time extension UI prompts can miss
  the FTUI surface.
- bd-4t6oz: the extension runtime's `pi.tool` hostcalls resolve against a
  plain `ToolRegistry::new(...)` built for prewarm (`src/main.rs:1396`,
  `1443`; `PreWarmedExtensionRuntime.tools`, `src/agent.rs:5166`,
  `13389`), not the Agent's registry with the undo recorder and workspace
  handle (`src/main.rs:1738`; `Agent.tools: ToolRegistry` by value,
  `src/agent.rs:1503`). Extension writes bypass `/undo` and workspace
  confinement; later-mounted MCP/extension tools are invisible to
  extensions.

**Target state.** One session owner on the default launch; one authoritative
registry shared by the Agent and the extension runtime; `setActiveTools`
changes the next schema atomically.

**Success criteria.**
- [ ] A launched-binary test (`tests/e2e_ftui.rs`) asserts the extension
      runtime boots exactly once per launch (count
      `pi.extension_runtime.prewarm.success` / `engine_decision` events in
      the log) and that a startup hook `ctx.ui.confirm` reaches the FTUI ask
      surface.
- [ ] A production-path test: extension `pi.tool("write", …)` under the
      default launch records a `/undo` entry and is confined by the
      workspace handle; an MCP tool mounted after extension load is callable
      from `pi.tool`.
- [ ] Both tests fail against the pre-fix tree (planted negative recorded in
      the bead).

**Implementation.**
1. bd-2crrf, option (a): gate the classic `AgentSession` construction and
   `enable_extensions_with_policy` behind `!ftui_requested`; everything FTUI
   needs (provider/model selection, resources, workspace trust, approval
   state, enabled tools, extension flags) is already threaded into
   `SessionOptions`. Audit the code between `main.rs:1760` and `2227` for
   side effects FTUI relies on (extension-provided providers/models,
   `--continue` resume, pre-start extension flags) and move those into the
   SDK path. Option (b) — pass the built session into `interactive_ftui::run`
   — is the fallback if (a) loses semantics.
2. bd-4t6oz: build the recorder+workspace registry once before prewarm
   (both are cheap and available at `main.rs:1388`), hand the same
   `Arc<ToolRegistry>` to the prewarm runtime and to `Agent::new`; change
   `Agent.tools` to `Arc<ToolRegistry>` (or `ArcSwap` for later mounts) and
   delete the `pre_tools` constructions. Mirror in `src/sdk.rs:2344`.
3. Land each behind its bead with the tests above; run the gate.

**Dependencies.** C1 for proof; independent of each other.
**Complexity.** L each (main.rs is 12k lines; agent.rs 20k).
**Vision goals.** 7, 8, 11.
**Would existing beads close it?** Yes (bd-2crrf, bd-4t6oz) — the beads are
correct; they lacked a compile/test loop and anchors, both now supplied.

### 10.2 Major gaps (the vision is significantly degraded)

#### Gap M1 — Performance evidence is blocked and internally inconsistent

**Current state.** `tests/perf/reports/budget_summary.json`: rows 16 PASS /
3 FAIL, header 12/5/2, `claim_readiness.status = blocked`, source
`e178a73d` (not a release SHA). Failing: `ext_cold_load_simple_p95` 11.9 ms
vs 5 ms; `tool_call_latency_mean` and `tool_call_throughput_min` have no
real data (the "canonical" script falls back to a synthetic stub —
bd-tool-call-throughput-canonical-o3ubk reopened). README withholds numbers
correctly. `scripts/check_readme_evidence_freshness.py` binds only header
counts, so it cannot see the header/rows mismatch (bd-sog97.20).

**Target state.** A strict `budget_summary.json` regenerated by the DSR perf
lane at the release SHA whose header equals its rows, with real data for
every CI-enforced budget; either all budgets pass or each failure has a
dated waiver (RI-WAIVER, bd-sog97.12); `claim_readiness.status` is `ready`
or `ready_with_advisories`; README prose rebound.

**Success criteria.**
- [ ] `jq '[.pass,.fail,.no_data]'` equals the histogram of
      `.budget_results[].status`; enforce this in
      `scripts/check_readme_evidence_freshness.py` (one assertion) so the
      mismatch class cannot recur.
- [ ] `tool_call_*` rows cite a `pijs_workload` artifact whose provenance
      says `source_kind != synthetic`.
- [ ] `ext_cold_load_simple_p95 ≤ 5.0 ms` or a waiver entry with expiry.

**Implementation.**
1. Add the header==rows assertion to the README checker (S).
2. Build `examples/pijs_workload.rs` in the DSR perf lane and delete the
   synthetic fallback path from `scripts/perf/run_pijs_workload.py` (a
   fallback that fabricates data is the defect) (S).
3. Profile simple cold load: transpile cache warm path, realm creation
   (`src/extensions_js.rs` cold realm factory); target the 5 ms budget or
   justify raising it via calibration (bd-sog97.15) (M–L).
4. Run the DSR perf lane at the release SHA (bd-ri-phase1-full-refresh-rndeg
   reopened; bd-sog97.19/.27) (operator, M).

**Dependencies.** C1 (same lane); C2 for the release SHA.
**Complexity.** M overall.
**Vision goals.** 12, 14.
**Would existing beads close it?** Mostly yes (bd-sog97 children + the two
reopened beads); the checker assertion and the synthetic-fallback deletion
are new.

#### Gap M2 — Provider and extension corpora are not live-validated at HEAD

**Current state.** 11 provider modules exist; the "fresh 11-provider live
E2E" bead was closed with no credentials set (reopened). Extension must-pass
gate: 206/208 at 2a8e0862 (marckrenn-pi-sub ×2, triaged), and bd-sog97.29
reports a hermetic clean-checkout run yields 143/208 with empty-observation
failures — meaning the 206/208 number depends on local state. Stretch set
10/19.

**Target state.** A live-provider run with credentials for all 11 modules
recorded at the release SHA; must-pass 208/208 hermetic (or waived per
extension with a reason); the release-binary E2E lane (README "Extension
Validation Pipeline" step 3/4) executed once on the v0.4.x binary.

**Success criteria.**
- [ ] `pi.perf.provider_live_e2e.v1` artifact with 11 entries, each
      `status: pass`, `git_commit` = release SHA, credentials resolved from
      env (never stored).
- [ ] `must_pass_gate_verdict.json` `status: pass` from a clean checkout
      (`git clean`-equivalent worktree) with `must_pass_failed: 0`.
- [ ] `tests/ext_conformance/reports/release_binary_e2e/*` regenerated for
      the shipped binary with `fail=0`.

**Implementation.**
1. Operator supplies provider credentials to the swarm host env for one run
   of `scripts/perf/run_provider_live_e2e.py`; fix what fails (bd-x23nj
   GitLab wire contract and bd-fouvy early stream end are the known suspects).
2. Make the must-pass corpus hermetic (bd-sog97.29): find the local-state
   dependency (likely `.tmp-codex-unvendored-cache/` or fixture paths) and
   pin it in the fixture set.
3. Remediate or de-scope marckrenn-pi-sub (bd-sog97.28).
4. Run the release-binary E2E on the v0.4.x archive (ollama + qwen2.5:0.5b
   per README) and check the artifacts in.

**Dependencies.** C2 for step 4; credentials for step 1.
**Complexity.** M.
**Vision goals.** 3, 8.
**Would existing beads close it?** Yes: bd-provider-live-validation-11-xme9d
(reopened), bd-sog97.28/.29/.6.

#### Gap M3 — User-visible defects on the default stack and open issues

**Current state.** Open: #195 (heading colours fixed in e0897567; table
alignment with truncated cells not addressed), #198 (ask hang fixed in
402ff9cd, unreleased), #182 (Windows, scoped in bd-oyckr), #178/#167
(compaction bridge shipped in the v0.4.0 tree, unreleased), #207 (done,
unreleased). The interactive ask-card machinery has 12 stale unit tests
(Gap C1), which is itself a signal that the card lifecycle changed three
times in a week (2cd3871e, 913d0eb3, 402ff9cd) without its tests following.

**Target state.** Every open issue either closed by a released fix or
answered with a bead id; ask-card lifecycle covered by tests that use the
real channel path.

**Success criteria.**
- [ ] #195 table rendering: a gallery visual-regression case in
      `src/gallery.rs`/`tests/e2e_ftui.rs` with a wide table and a narrow
      terminal renders without truncated cells; fails before the fix.
- [ ] #198 verified on the released binary by the reporter's steps (ask card
      appears within the turn, not minutes later).
- [ ] All 5 channel-based ask tests pass through the gate.

**Implementation.**
1. Table rendering in the FTUI markdown path (`src/markdown_rich.rs`,
   `src/interactive_ftui.rs`): compute column widths against the viewport,
   wrap or elide with an explicit marker instead of truncating cells.
2. Rewrite the direct-injection ask tests to the channel path once the 5
   channel-based failures are understood (Gap C1 step 1), then delete the
   test-only registration hook if no longer needed.
3. Reply on #195/#198/#207/#178/#167 with the release that carries the fix
   (operator-facing; outward).

**Dependencies.** C2 for verification on a release.
**Complexity.** M.
**Vision goals.** 7, 19.
**Would existing beads close it?** Partially: bd-oyckr (Windows), bd-q66i1
(cards). Table rendering has no bead → new.

#### Gap M4 — Session and swarm hygiene is defeating the gate

**Current state.** Auto-commit sessions sweep any working-tree change into
generic commits and push to `main` within minutes ("chore(beads)…",
"feat(src): …"). In this session that produced: my staged work committed
under someone else's message, an ineffective timeout committed mid-fix, and
two `current_time` implementations landing within an hour (14760976 vs
d39d3366) with the registry pointing at the wrong one. AGENTS.md's
Definition of Done (DSR green before integration) is not enforced by anything.

**Target state.** Work reaches `main` only after the gate has run on that
tree, and concurrent sessions coordinate on file ownership.

**Success criteria.**
- [ ] The auto-commit sweeper either runs `dsr quality` (or at minimum
      `cargo fmt --check` + `RCH_REQUIRE_REMOTE=1 rch exec -- cargo check
      --all-targets`) before pushing, or pushes to a branch that a gated
      merge promotes.
- [ ] `file_reservation_paths` (Agent Mail) is used for `src/**` edits by
      every session, so a second implementation of the same feature is
      refused at reservation time.

**Implementation.**
1. Find the sweeper (it commits as Dicklesworthstone from another session on
   this host); add the gate call or the branch push. Owner decision — it is
   the maintainer's automation.
2. Add the reservation step to the session start macro every session runs
   (AGENTS.md already prescribes it; make the tooling call it).

**Dependencies.** None.
**Complexity.** S–M (mostly policy + one script).
**Vision goals.** 13, 16.
**Would existing beads close it?** No bead exists → new.

### 10.3 Minor gaps (polish and drift prevention)

#### Gap P1 — README/code drift prevention

**Current state.** README, AGENTS.md, docs/tui.md, docs/development.md,
docs/releasing.md now match the code (§9). Counts (35 tools, 19 default,
14 essential) are hand-maintained.

**Target.** One test asserts the README tool bullets equal
`xdev::ESSENTIAL_DEFAULTS` + `default_enabled_tools()` + the settings-gated
and opt-in names from `ToolRegistry`, and that the README default-session
count equals `default_enabled_tools().len()`.
**Criteria.** `tests/readme_tool_inventory.rs` fails when either side
changes alone. **Complexity.** S. **Beads.** None → new.

#### Gap P2 — Repository leftovers from this session (need your permission)

- `src/current_time.rs` vs the in-file `CurrentTimeTool` in `src/tools.rs`:
  one of the two must go, and that is the maintainer's call. Sequence on
  2026-09-02: 416cabe9 (maintainer) dropped the in-file copy in favour of
  the module but left main uncompilable; the wiring landed (8d8fdd6e); the
  maintainer reverted the swap (74490cb8); I followed the revert (b802d9a6);
  another session re-wired the module (82fd0468) while gate run7 was
  executing. HEAD ships the module (registry, one-liner, prompt index all
  point at it) and carries the in-file copy as dead code with live unit
  tests. Removing either is a code deletion inside a tracked file, not a
  file deletion, but the choice is a product decision.
- `/data/projects/pi_agent_rust_baseline`: throwaway clone at 08485a20 used
  for the baseline classification; delete.
- `<scratchpad>/gate-wt`: git worktree (registered in `git worktree list`);
  `git worktree remove`.
- Untracked `bead` (empty) and `scripts/agent_verify.sh` (hardcoded Mac path)
  in the repo root: not mine; ask before touching.
- Worker `hz3` drained in rch for disk pressure; re-enable when its disk is
  reclaimed (`rch workers enable hz3`).

#### Gap P3 — Closure discipline for evidence beads

**Current state.** Three beads were closed on "script shipped" (reopened
2026-09-01). The pattern is structural: the closer had no way to run the
lane.
**Target.** A bead whose title names an outcome closes only with the
outcome's artifact path + SHA in the close reason. **Implementation.** One
sentence in AGENTS.md "Beads" section; `br close` templates in the session
macro. **Complexity.** S. **Beads.** None → new (docs).

### 10.4 Order of work and dependency graph

```
C1 lib-suite green ──┐
C1 hang policy ───────┼──► C1 full gate green ──► C3 adjudicate 42 beads
C4 (2crrf, 4t6oz) ────┘            │
                                    ├──► C2 v0.4.1 via DSR ──► M2 release-binary E2E
M1 perf lane ───────────────────────┘            │
M2 provider live E2E (creds) ────────────────────┘
M4 sweeper gating ── independent, do first (it protects everything else)
M3 #195 table fix ── independent
P1, P3 ── independent, S
P2 ── after your answer on deletions
```

Recommended sequence for the next sessions: **M4 → C1 (lib) → C4 → C1
(integration) → C3 → C2 → M1/M2 → M3 → P1/P3**, with P2 whenever approved.

### 10.5 "Would completing all existing beads close every gap?"

No. Existing beads cover C3, C4, M1, most of M2, part of M3, and P2's
current_time deletion is a permission question. **No bead exists for:** the
integration-lane run and hang policy (C1 steps 2–3), making the sweeper
respect the gate (M4), the checked-in DSR build authority file (C2 step 3),
the header==rows checker assertion and synthetic-fallback deletion (M1
steps 1–2), the table-rendering defect (M3), the README inventory test (P1),
and closure discipline (P3). Those are the beads Phase 3a must create.

### 10.5a Execution log (2026-09-02, Phase 3a beads created and worked)

Beads created for the no-bead gaps: bd-ew6h7 (M1 checker + synthetic
fallback), bd-dwh6g (P1 inventory test), bd-yqo76 (C1 gate green), bd-0x31m
(M4 sweeper), bd-ikl7j (C2 build authority file), bd-0znp2 (M3 tables),
bd-vchwp (P3 closure rule). Dependencies: bd-yqo76 blocks on bd-x8mn7 and
bd-ew6h7; bd-ghfu4 blocks on bd-yqo76.

Done the same day: bd-vchwp closed (AGENTS.md rule); bd-dwh6g closed
(`tests/readme_tool_inventory.rs`, 4/4 remote); bd-ew6h7 mostly done (the
README checker now validates the v2 perf contract for path-only release-facing
citations and fails closed on the current artifact; `run_pijs_workload.py` has
no fabrication path; a fixture unit test is still owed); bd-ghfu4 step B
(CHANGELOG "Tag-only"); bd-0znp2 fix landed (table budget + width-change cache
flush + unit test); bd-ikl7j and bd-0x31m sized with exact requirements on the
beads (both need maintainer-held inputs); AGENTS.md also gained the
"stage only your own files" rule.

Run6 of the gate (0c68d1b4, run dir `20260901T233513-3206383`, receipt in
the run dir, source snapshot identical before and after): 4/6 checks passed.
fmt, `cargo check --all-targets`, `clippy -D warnings` and module reachability
green; `cargo test --all-targets` red on 8 lib tests (8340 passed), all in
the owner-decided set on bd-x8mn7, and cargo stopped there, so the
integration/conformance binaries still never executed; and
`tests/installer_regression.sh` red on 5 of 69 the first time it ever ran.
The same 5 fail at the v0.4.0 tag (reproduced in a scratch worktree), so
they predate this session: the preflight HEAD-probed the aggregate
`SHA256SUMS` manifest even when the per-asset sidecar was canonical (product
fix in `install.sh`: probe the release tag page); two naming fixtures served
a bare binary for the canonical `.tar.xz` candidate (fixtures now serve it for
the bare-binary candidate only); and one doc-guard test grepped the disabled
release workflow for a sentence it never contained (workflow assertions
dropped, runbook assertions kept). Local suite after the fixes: 69/69. Two
gate-facing edits are disclosed here rather than buried: the cargo test
check now carries `--no-fail-fast` so every test binary runs and reports in
one pass (the check still fails on any failure), and the cluster B
"overlapping turn" test now expects three provider calls because a split-turn
compaction issues two (history + turn prefix; design unchanged since
February). My earlier note that the summary-filter change had fixed that test
was wrong: it only advanced the failure to the call-count assertion.

Run7 (b802d9a6, run dir `20260902T002444-3404080`) was cancelled after its
lib binary: fmt, check and clippy green; 8040 passed / 376 failed, and 370 of
the failures had one environmental cause. rch exports `TMPDIR` as the
project-local `.rch-tmp` on the worker, that directory was owned by a
different uid than the test process on this worker (vmi1153651, as on
vmi1149989 in run5), and pi's mode-class check (`src/platform.rs`, which
deliberately gives root no DAC bypass) denies every temp-dir write. The
recipe's test check now runs `env TMPDIR=/tmp cargo test …` (gate-facing,
disclosed); the worker hygiene itself is an rch matter for the maintainer.
The run was also invalidated by the tree moving under it: another session
pushed three commits at 04:30Z, one of them (82fd0468) re-wiring
`src/current_time.rs` as the shipped module ten minutes after the
maintainer's revert had restored the in-file copy, so HEAD now carries both
implementations (the in-file one is dead code with live unit tests) and the
one-liner/prompt-index text follows the module. Landed afterwards, all
pending run8 evidence: the FTUI startup no longer boots the classic
extension runtime (bd-2crrf slice); the extension runtime's hostcall registry
shares the session's undo recorder and workspace roots (bd-4t6oz slice); RPC
stdin close after a rejected command no longer fails with a "quarantined"
terminal error when there is nothing to preserve (bd-m83oo, product); the
interactive test doubles carry the app's model identity so the
pre-submission runtime sync keeps them installed (three stream_delta tests);
the FTUI ask-forwarder test drives the guard deterministically (asupersync's
`current_thread()` runtime runs spawned tasks on a worker thread).

Run8 (cbeb3705, run dir `20260902T015053-3764248`): 5 of 6 checks green,
including the first DSR-recorded pass of `tests/installer_regression.sh`
(69/69), and the first complete test lane in the project's DSR history: 404
binaries, 37,830 passed, 187 failed in 41 binaries. The failures are
inventoried on bd-yqo76 by cluster: stale performance-evidence artifacts and
their validators (owner bd-sog97.*; the Rust validators also reject the
artifact's `git_commit` field that the Python checker tolerates), orchestrate
harness tests that call the live rch fleet (bd-b3yao), 28 insta snapshots
stale after the title/status-line TUI work (bd-hey4b), seven fault-injection
tests that expect a healing rewrite the fail-closed source-integrity rule
refuses (bd-te4ks), five MCP HTTP-transport aborts (bd-z6004), and a tail of
worker-environment cases (tmux, gh, root, dcg). Fixed in the same round: the
`cli_flags` conformance fixture and a parser test that still used `--plan`
as an extension flag (built-in since 2026-08-14), a crash-consistency test
that predates fail-closed duplicate-id loading, two error-code assertions,
root skips for two permission-denial tests, four unclassified test files and
eighteen source files missing from the coverage matrix. One reversal: my
terminal-persistence reorder from the previous round contradicted the
maintainer's `rpc_retry_restore_save_failure_latches_without_live_mutation`
(quarantine must surface even with nothing to preserve), so the product
change was reverted and the two RPC loop tests now expect the surfaced error;
only the rejection wording change survives.

Round 3 (after cancelling run9, whose cold worker projected past the lane
timeout while repeating run8's clusters): two of the "owner" clusters turned
out to be single-line defects. `scripts/perf/orchestrate.sh` referenced an
undefined `ROOT_DIR` (since 2026-08-26) and aborted under `set -u` in every
orchestration that reached the extension-benchmark validation, which is what
the 31 harness failures were; and a zero-byte regular file named `.codex` at
the repository root (git-ignored, synced to the workers) made the
workspace-trust scan fail startup with "Not a directory" on
`.codex/config.toml`, which is what eleven url-read, web-search, advisor and
failover e2e failures were. Both fixed, the second with a unit test. The 28
`tui_snapshot` diffs were reviewed line by line: every one is the OSC title
prefix and the new status line (four zero-usage cases swap the old token
footer for it), i.e. the deliberate #200/#201 rendering; the goldens were
regenerated on a worker and re-checked against that classification before
committing.

Run10 (e3d16a72, run dir `20260902T045103-207150`, receipt): 5 of 6 checks
green; the lane fell from 187 failures in 41 binaries to 119 in 31, with 68
tests fixed and nothing newly broken. `tui_snapshot` is green (bd-hey4b
closed on that evidence), the eleven `.codex` startup failures are gone, and
`bench_schema` went from 31 to 13 (the rest are fake-toolchain contract
mismatches, left on bd-b3yao). Two lib failures in that run were transient
and instructive: the embedded-changelog round-trip failed because the worker's
build-script output was stale (I edited `CHANGELOG.md` seconds after a sync,
and the next sync delivered it with an mtime older than the build-script run,
so cargo's mtime-based `rerun-if-changed` never fired; a `touch` before the
next sync cures it, and the hazard is recorded on bd-yqo76), and my reworked
pending-input test lost a real race with the spawned turn's session lock
(rewritten to deliver the note before the turn starts). What remains red is
owner-decided or worker-environment: stale performance evidence and its
validators (bd-sog97.*), the fault-injection healing contract (bd-te4ks),
the MCP HTTP transport aborts (bd-z6004), tmux/gh-driven e2e tests, and a
tail of singletons inventoried on bd-yqo76.

Decide-everything round (2026-09-02, on the maintainer's instruction that
there are no owner-decided items): the MCP aborts were the optional GET
stream treating any non-405 answer as fatal (fixed: such answers now mean "no
server stream"); hidden extension messages now reach the model, with pi's own
provenance records as the one explicit exception; the three Rust
performance validators accept the harness's `git_commit` field; the JSON-mode
envelope key `capability_prompt` became `capabilityPrompt`; the inventory
hash pin turned out to be correct and the checked-in budget artifact stale
(it predates the 2026-08-24 cold-load amendment), so the remedy is a real
perf regeneration, not a re-pin; the fail-closed session-integrity contract
(2026-08-27, seven lib tests) wins over the older healing suite, whose seven
tests now expect the refusal and preserved bytes; the RPC session-stats tests
run with saving enabled because a save-disabled session truthfully reports
"disabled"; the classic-era tmux tests pass `--classic`; the bash-mediation
tests opt out of `dcg` to pin the in-tree classifier; the Node fs shim tests
use in-root paths for ENOENT and pin the outside-root denial; the bench
harness writes per-process artifacts outside the orchestrator; and the
JSON-parse variance class is enforced only in the perf lane. Coverage debt
recorded: FTUI equivalents of the share/continue/tan e2e scenarios.

Second decide-everything pass (2026-09-02 afternoon). The rule applied
throughout: when a maintainer test and a maintainer rule disagree, `git log`
both and the newer deliberate contract wins; product fail-closed rules are
never weakened to make a test pass, and every gate-facing test edit is
disclosed here and in the commit. Findings and decisions: the five remaining
MCP HTTP transport failures were not the GET stream after all but the
2026-08-27 15:24 retirement rule (a malformed, mismatched, or 202-acknowledged
request retires the transport) landing three hours after the resilience tests
that assumed continued reuse; four tests now expect retirement, and the
nested-404 test expects the one `notifications/cancelled` that the
indeterminate abort sends for the accepted call. The compaction "rejects
stale snapshot" hang was a lock-order deadlock inside the test (the
auto-compaction path holds the agent-session lock across the provider call
and the test swapped the session through that same lock); swapping through
the shared inner handle makes it pass in under two seconds, and the watchdog
comment now states the real cause. The three `/share` tmux tests write
`.pi/settings.json` into the tmux working directory, which is a
workspace-trust surface, so the classic TUI showed the trust prompt instead of
the welcome banner; they now set the documented automation override. The RPC
plan-mode e2e test approved the plan as soon as the mock server had seen the
last request of turn one, while the agent was still streaming that turn, so
the RPC loop refused both the approval and the follow-up prompt ("Agent is
currently streaming"); waiting for `agent_end` then hit the second window,
the post-turn compaction handoff that the turn runner claims before any
further await ("Agent is currently compacting; wait before running"). Both
refusals are the documented client contract, so the test now retries until
the command is admitted, and it drains the child's output pipes, which it
never read until the end. The FrankenNode matrix check compared
the generating host's Node and Bun versions, which no DSR worker shares; it
now compares verdicts and pass rates only. The extension stress test counted
first-load costs as leak; RSS growth is measured after a warm-up cycle. The
OCO budget tuner had a state struct and no test; it now has regret-accounting
and rollback tests against the real implementation. The traceability matrix,
e2e scenario matrix, and perf SLI contracts missed 33 test files, four e2e
suites, and the background `/tan` workflow; all are mapped and the governance
script passes at 100%. The swarm runpack golden lagged a script change that
adds two retained-artifact inventory entries; regenerated after reading the
diff. Two phase-1 matrix validator fixtures left an incomplete cell marked
"pass", which the newer completeness check rejects first; fixed. Three
smaller ones surfaced by the re-run: the `/share` success message
soft-wrapped in the TUI so "Share URL:" straddled two terminal lines (the
message now uses paragraph breaks — the one user-visible change of this
pass); the math-reachability table looked for weighted bottleneck
attribution in `src/extensions.rs`, where it never lived (it is the perf
orchestrator's computation, validated by the phase-1 matrix tests); and the
interruption "fresh run after abort" test resumed a session with a different
mock provider, which the session-bound model selection rule now refuses (the
fresh provider reports the bound identity). Run12 (2518c8c1, run dir
`20260902T121239-1740587`) then reported fmt/check/installer/reachability
green, the test lane at 37716 pass / 72 fail (run11: 119 fail), and clippy
red on three lints from the FTUI autocomplete commit (75493a02) that landed
between run11 and this round; DSR also invalidated the receipt because
recording the launch on the beads changed tracked `.beads` files mid-run
(lesson: no tracker writes while a DSR run is in flight). Four of the 72
were new and mine: the February context-filter test still expected hidden
messages to be dropped (realigned to the hidden-messages-reach-the-model
rule, with the provenance exclusion pinned), two replay-bundle checks caught
that the four new scenario rows had copied the `/tan` replay command (now
point at their own suites), and the embedded-changelog byte test tripped on
the worker's stale build-script output (the known rsync-mtime hazard; the
file is touched again before the next run). The three clippy lints (and a
fourth on the stress test's warm-up refactor, masked until the lib was
clean) were fixed concurrently by the maintainer's own commits (8d965337,
a06fe698) while equivalent fixes were being verified here; the rebase kept
the maintainer's versions. Run13 (197df581, run dir
`20260902T132856-2042977`, valid receipt) is the first run of this round
with a clean receipt: five of six gates green (fmt, check, clippy with
`-D warnings`, installer, reachability) and the test lane at 37715 pass /
73 fail. The 73 are the parked clusters (perf-evidence validators 47,
orchestrate contract tests 11, ext-conformance and risk-review artifacts 8,
drop-in slash differential 3) plus four singletons: the `/share` gist test
captured the pane between two frames of the success message (it now waits
for the last paragraph), the managed-skill "invalid name" test only ever
passed on hosts with a leftover skill of the slugified name (the learn tool
documents that an unusable requested name falls back to a lesson-derived
one, and the test now pins that fallback plus the refusal on a repeat
promotion), and two `/new`-cancellation
TUI tests that had passed in every earlier run and ran on a root worker this
time (re-run in isolation before deciding). Run14 (605f4dd7, run dir
`20260902T152233-2446904`, valid receipt): five of six gates green again,
test lane 37719 pass / 69 fail; the skill test and the two root-worker TUI
failures are gone, and the one singleton left is the `/share` gist test,
whose pane finally explained itself: `/share` was sent while the preceding
`/name` update was still persisting and the product answered "Session is
busy; retry `/share` after the current session update finishes". That is
the documented retry contract, so the test retries on that message instead
of racing it.

Round 6 (2026-09-02 evening) turned to the wiring gaps the reality check had
anchored. bd-4t6oz slice 2: the extension runtimes no longer get their own
plain tool registry at pre-warm. One `SharedToolRegistry` (a snapshot-swap
handle: readers take an `Arc` snapshot and never hold a lock across a tool's
execution; mounts clone-and-swap) is built once in `main` before the
pre-warm, handed to the JS/native pre-warm and to the Agent
(`Agent::with_shared_tools`), and the inline enable path used by the SDK and
FrankenTUI sessions reuses the agent's handle instead of building a fresh
registry without a workspace. Every hostcall context, the WASM host, the
manager's WASM loader, and the dispatcher take a per-call snapshot, so a
`pi.tool` hostcall sees tools mounted after boot (extension wrappers, MCP
tools, plan tools) under the same undo recorder and workspace roots as the
agent's own calls. The production-path test
`hostcall_sees_tool_mounted_after_runtime_start` boots the runtime on the
shared handle, mounts a Rust tool through the agent afterwards, and drives a
`pi.tool` hostcall from an extension tool to it; under the split registry it
fails with "Unknown tool". Slice 3 closed the `setActiveTools` gap on the
same handle: tools carry an origin (extension wrappers answer
`Extension`), the registry keeps a shelf for de-activated extension tools,
each snapshot carries a weak back-pointer to its shared handle so the events
hostcall can publish the swap it needs without threading a new field
through the hostcall context, and the shared handle's version is folded
into the agent's tool-schema cache key so the next provider request sees
the change without any out-of-band invalidation. The test drives
`pi.setActiveTools` from an extension tool and checks that the agent loses
and regains the shelved tool. bd-8m21l followed on the same seam: an MCP
server registered by an extension after startup only ever reached the
extension manager's snapshot; the SDK session handle now drains that
snapshot at the start of every prompt (`sync_extension_mcp_registrations`),
registers unknown definitions through the startup trust gate, and reuses
the existing connect-and-mount path that skips tool names already present.
The feature-free test in `tests/mcp.rs` registers a late definition the way
the hostcall does and checks: registered once, extension provenance, pending
trust (nothing mounts), second sync registers nothing. The FrankenTUI and
SDK paths got it first; the classic TUI's three turn tasks now run the same
shared implementation (`pi::mcp::sync_extension_registrations`) right after
locking the agent, and only the RPC loop, which owns no MCP manager, still
lacks it. Run17 (099c4b43, run dir `20260902T182315-3052505`, valid
receipt) is the evidence for the registry work: five of six gates green and
the test lane at 37731 pass / 73 fail with every extension, MCP, and
hostcall suite green unfiltered; bd-4t6oz closed on it. The 73 are the four
parked clusters plus five singletons in the interactive layer, two of which
(`/new` cancellation) now look flaky across workers rather than root-bound,
and three of which appeared with the maintainer's same-day interactive
changes. Run18 (c56c08ca, run dir `20260902T201410-3472526`, valid
receipt) confirmed the classic-TUI sync in the full lane: five of six gates
green, 37736 pass / 69 fail, the residual being exactly the four parked
clusters plus the one provider-error rendering singleton; bd-8m21l closed
on it. Parked with
reasons: the ext-conformance artifact cluster is blocked by a stray ignored
file (`tests/ext_conformance/artifacts/doom-overlay/tiny.wad`, bd-n4ov9,
deletion needs written approval); eight orchestrate contract tests fail
because the post-generation evidence contract is blocked under the stub
toolchain (bd-b3yao); the drop-in slash differential needs the legacy tsx
runner on the workers; the perf-evidence cluster needs a real regeneration
bound to a clean committed HEAD (bd-ri-phase1-full-refresh-rndeg).

Round 9 (2026-09-03, executing the Phase 3a bead set in dependency order).
Twenty granular beads were created through `br` (children of bd-b3yao,
bd-n4ov9, bd-2vmu6, bd-oqo03, bd-gm481, bd-0x31m plus bd-werhk, bd-tt1g6,
bd-7n3y7, bd-z7267, bd-ydz1t, bd-qonnl) with twelve blocking edges, and the
ready ones were worked. Product: the RPC loop's session now carries the MCP
manager and syncs late extension registrations on the first attempt of each
prompt (bd-1wr1n); print JSON mode and the RPC loop emit exactly one
`failover_end { restoredPrimary: false }` for a fallback turn before its
terminal `agent_end` (bd-2vmu6.1); the persistence fault runner compares
each case's result against the cargo test name instead of the harness id
(bd-b3yao.3); the orchestrator logs contract failures, blocked staging ids
and per-layer evidence when it blocks. Tests: the provider-error rendering
singleton was a deliberate #209 contract change and the test now asserts one
card after partial text (bd-tt1g6); a classic-TUI test and an RPC test prove
late MCP registration reaches the session at the next turn (bd-z7267); the
orchestrate stub fixtures were realigned to the current staging, comparison,
fence and validator contracts (bd-b3yao.1/.2/.4: semantic_context estimates,
the Criterion-produced pijs comparison contract, invocation-keyed drift,
suite-level lineage refusal, captured consumer stdout); the three drop-in
parity tests are ignored with their reason (bd-werhk). Found on the way:
nothing in the repo produces `pi.perf.cross_runtime_comparison.v1`, so a real
full orchestration can never pass Phase 5g until a producer exists
(bd-ri-phase1-full-refresh-rndeg.1, now blocking the refresh). The
concurrent sweeper session committed five intermediate states of this work
(2b30e8c9..9ef2784e) while exclusive reservations were active; recorded on
bd-0x31m. Run19 evidence for all of the above is recorded on the beads.

### 10.6 Verification plan (what "done" looks like, re-executable)

1. `DSR_REPOS_FILE=.dsr/repos.yaml dsr quality --tool pi_agent_rust` → 6/6
   executed, passed, at SHA S.
2. `gh release view v0.4.1 --json assets,tagName` → strict inventory; `dsr
   verify` green; clean-host install prints 0.4.1 and verifies a signature.
3. `br list --status=in_progress` → empty of the 2026-08-24..27 wave; each
   close cites S and test names.
4. `tests/perf/reports/budget_summary.json` at S: header == rows, no
   synthetic sources, `claim_readiness.status` ∈ {ready, ready_with_advisories}
   or waivers with expiry.
5. `pi.perf.provider_live_e2e.v1` at S: 11/11 pass;
   `must_pass_gate_verdict.json` pass from a clean checkout.
6. GH #195/#198/#182/#207/#178/#167 closed or answered with a bead id and
   release.
7. A commit that only edits README tool bullets fails
   `tests/readme_tool_inventory.rs`; a commit that only edits
   `default_enabled_tools()` fails it too.

---

## 11. End-to-End Reality Check & Bridge Plan — 2026-11-12

Audited against `AGENTS.md`, `/data/projects/AGENTS.md`, `README.md` (all 3,800 lines), `docs/*.md`, `.dsr/repos.yaml`, `.dsr/repos.d/pi_agent_rust.yaml`, `.beads/issues.jsonl` (`3,215` visible beads after `br sync --import-only --rebuild`), all 12 open GitHub issues, the published `v0.8.0` GitHub release (`2026-10-14`, 32 assets), the installed `/home/ubuntu/.local/bin/pi` (`0.8.0`) binary, and the source tree at `HEAD = fb5190eeee9e808c203d043b471d28ee794aaeab` on `main` (in sync `0/0` with `origin/main`, `master`, and `origin/master`).

### 11.1 Executive Verdict

**Substantially closer to the product vision than on 2026-09-01, with a genuinely massive Rust implementation (~714k LOC across `src/` and `tests/`, zero `todo!()`/`unimplemented!()`, 36 real built-in tools, 11 provider modules, 7 execution surfaces, and real `v0.5.0`..`v0.8.0` DSR releases) — yet critically drifted across five axes:**

1. **733 Unreleased Commits & Binary Budget Breach:** `main` is **733 commits ahead of `v0.8.0`** (`2026-10-14`), while `Cargo.toml:3` still says `0.8.0` and `CHANGELOG.md:15-55` `[Unreleased]` records only 3 commits. Every shipped `v0.8.0` binary violates the `<48 MiB` release-size budget (`AGENTS.md:399`, `README.md:2756`, `src/perf_build.rs:28`): `pi_linux_amd64` is **74.25 MiB** (`pi_windows_amd64.exe` **70.72 MiB**, `pi_darwin_amd64` **67.23 MiB**, `pi_linux_arm64` **60.35 MiB**, `pi_darwin_arm64` **49.89 MiB**) because `Cargo.toml:588-594` ships `[profile.dist]` (`opt-level = 3`) while `scripts/perf` measures `[profile.release]` (`opt-level = "z"`) and `.dsr/repos.d/pi_agent_rust.yaml:65-70` still declares `--release`.
2. **Authoritative DSR Quality Gate Is Red & Unregistered on `trj`:** Running `DSR_REPOS_FILE=.dsr/repos.yaml dsr quality --tool pi_agent_rust` (run `20261112T151944-cwDIpOFF`) fails immediately on Check 1 (`cargo fmt --check`, `exit_code: 1`, **33 unformatted Rust files** in `check-1.log`). Bare `dsr quality --tool pi_agent_rust` fails before Check 1 because `~/.config/dsr/` on `trj` has stale Sep-10 configs (`tool_name: pi`, zero quality checks). Meanwhile Check 5 (`tests/installer_regression.sh`: 73 pass / 0 fail / 1 skip), Check 6 (`check_module_reachability.py`: 153 pass, 3 allowlisted), and Check 7 (`check_fixture_read_patience.py`: pass) are green; `check_readme_evidence_freshness.py` is **RED** (19.1-day-old `budget_summary.json` > 14-day threshold) and `report_swarm_claim_readiness.py` is **BLOCKED** (25 blocking issues across all 5 categories).
3. **Silent Feature Black-Holes in Tools, CLI Flags, and Providers:**
   - **12 of 36 built-in tools are invisible to the LLM even when enabled in `settings.json`:** `manage_skill`, the 5 memory tools (`retain`, `recall`, `reflect`, `memory_edit`, `learn`), `browser`, `computer`, and the 4 media tools (`inspect_image`, `read_media`, `generate_image`, `tts`) register as `LoadMode::Discoverable` (omitted from `ToolRegistry::definitions()` until promoted via `xdev`), yet `src/app.rs:228-239` builds the system prompt from `xdev::prompt_index_for(cli.enabled_tools(config), ...)` where BOTH `src/cli.rs:3617-3633` and `src/xdev.rs:148-193` (`builtin_one_liner` `_ => None`) omit all 12 tools — while `ToolRegistry::discoverable_index()` (`src/tools.rs:6801`) sits dead outside tests.
   - **`--wait-async` (`bd-omp-wait-async-8e2bd`) fails at CLI entry:** `src/cli.rs:550` added `--wait-async` without adding `"wait-async"` to `known_long_option()` (`src/cli.rs:67-157`), so `pi -p --wait-async "prompt"` diverts `--wait-async` into `extension_flags`, consumes `"prompt"` as its flag value, and errors with `"No input provided"`.
   - **2 of 11 native providers (`gitlab`, `cursor`) cannot run an agentic coding loop:** `src/providers/gitlab.rs` is non-streaming and drops all tool definitions/calls/results; `src/providers/cursor.rs:1065-1070` sends only `latest_user_text` (dropping multi-turn conversation history) and drops all tools.
4. **Architectural Dishonesty Between README Claims and Production Wiring:**
   - `SessionStoreV2` (`src/session_store_v2.rs`) is never created or appended during live sessions; `Session::save_inner` (`src/session.rs:1502, 1844, 5327, 5361`) writes JSONL/SQLite and marks any `pi migrate` `.v2` sidecar stale on first save, contradicting `README.md:186`.
   - Four "Alien Artifact" math subsystems (`ExtensionDispatcher` + `ResourceGovernor`, `VOI`, `IPS/WIS/DR`, `IntentGraph`) have zero production callers; `HostcallReactorMesh` (`src/extensions.rs:24245`) is `None` in production; `io_uring_executor_available()` (`src/hostcall_io_uring_lane.rs:114`) is `const false`; `HostcallRewriteEngine::marshal` (`src/extensions/protocol.rs:582-609`) hashes payloads twice per hostcall; and `record_budget_overload_signal` (`src/extensions/extension_manager_impl.rs:928-963`) overwrites `cusum.last_observation_ms = Some(now_ms)` before computing `now_ms - prev`, feeding `latency_proxy = 0.0` to Conformal prediction on 100% of calls.
   - Load-only JS shims (`zod` in `src/extensions_js.rs:18423`, `node:net` at `15566`, `ws` at `18567`, `node-pty` at `17859`) silently pass unvalidated payloads or emit fake `'connect'`/`'open'` events while dropping writes.
5. **Supply-Chain, Security Advisory, & Swarm Hygiene Gaps:**
   - Neither `install.sh:1931-1995` nor `src/self_update.rs:289-305` verifies `.minisig` signatures (`bd-yj126`), and `PackageManager::resolve()` (`src/package_manager.rs:516-596`) never verifies `package-lock.json` `digest_sha256` at startup.
   - `Cargo.lock:8813` pins `wasmtime 47.0.4` (5 RUSTSEC advisories requiring `>=48.0.4`, GH `#258`) and `fuzz/Cargo.lock:4034` is frozen at `0.6.1` (3 RUSTSEC advisories).
   - RPC `rewind` (`bd-rpc-rewind-atomicity-c349e`) carries a ghost bead note claiming implementation + 4 tests landed while `src/rpc.rs:5174-5292` remains untouched (still holding `session` + `guard.session` locks across `.await` and truncating context when summarization fails).
   - ~29 open/`in_progress`/`blocked` beads are `LANDED_UNCLOSED` on `main` awaiting a clean `dsr quality` gate, while 7 of 12 open GitHub issues (`#282, #271, #267, #264, #262, #257, #250`) are already fixed on `main` and awaiting `v0.9.0`.

---

### 11.2 Vision Checklist (2026-11-12 Ground Truth vs Shipped `v0.8.0` vs `HEAD`)

| # | Vision Goal (`README.md` / `AGENTS.md` / `docs/*`) | Status | Bead Coverage | Concrete Evidence (`file:line` & Live Execution) |
|---|---|---|---|---|
| 1 | Native single-binary distribution via DSR (`5` targets, `<48 MiB`, `.sha256` + `.minisig`, `install.sh`, `self_update`) | **PARTIAL / REGRESSED** | `bd-yj126` (minisign); **NO_BEAD** for `74.25 MiB` budget breach & `dist` vs `release` split | `v0.8.0` shipped 32 DSR assets (`SHA256SUMS` + `.sha256` + `.minisig`), Actions disabled (`enabled: false`), `installer_regression.sh` 73/73 pass. **Gaps:** all 5 binaries breach `<48 MiB` (`74.25 MiB` linux-amd64); `733` commits unreleased since `2026-10-14`; no minisign verify in `install.sh` or `src/self_update.rs:289-305`. |
| 2 | Authoritative DSR quality gate (`fmt`, `check`, `clippy`, `test`, installer, reachability, fixture patience) + Windows lane | **REGRESSED / PARTIAL** | `bd-gate1-fmt-chronically-red-aqxxe`, `bd-m7im4`, `bd-o6hte`; **NO_BEAD** for host DSR drift & false-closed `bd-7ilwr` | Check 1 (`cargo fmt --check`) fails across 33 files (`check-1.log`); `~/.config/dsr/` broken on `trj`; `.dsr/repos.yaml:76-77` still lacks `--keep-going` despite closed `bd-7ilwr`; RCH Windows worker `wsurf` administratively disabled. |
| 3 | Core agent loop (`asupersync` Cx/cancel, steering, streaming SSE, BPE compaction, `/goal`, cost budgets, failover) | **WORKING** (minor parity gaps) | `bd-dowvm` (media cap); **NO_BEAD** for `/login` frozen failover | `src/agent.rs`, `src/sse.rs`, `src/compaction.rs`, `src/goal.rs`, `src/cost.rs` real and deep. **Gaps:** `/login`/`/logout` fail to refresh `FailoverOptions` (`src/sdk/recovery.rs:397-400`); 3 separate retry loops (`interactive.rs`, `rpc.rs`, `sdk/recovery.rs`). |
| 4 | 11 native provider modules + 50+ OpenAI-compatible presets + OAuth/API-key auth | **PARTIAL** | `bd-provider-live-validation-11-xme9d`; **NO_BEAD** for `gitlab`/`cursor`/`copilot` | 9/11 providers support streaming + tool use (`anthropic`, `openai`, `openai_responses`, `gemini`, `cohere`, `azure`, `bedrock`, `vertex`, `copilot`). **`gitlab` (`src/providers/gitlab.rs`) and `cursor` (`src/providers/cursor.rs:1065-1070`) drop all tools; `cursor` drops multi-turn history; `copilot` drops reasoning/cost on Chat Completions (`src/providers/copilot.rs:382-403`).** |
| 5 | 36 tiered built-in tools + `xdev` dynamic activation + `subagent` + local memory bank | **PARTIAL / NO_BEAD** | `bd-tools-known-names-missing-seven-mbo3g` (`LANDED_UNCLOSED`); **NO_BEAD** for 12 invisible tools | All 36 tools have real `impl Tool` implementations. **Critical bug:** 12 discoverable/settings-gated tools (`manage_skill`, 5 memory, `browser`, `computer`, 4 media) are omitted from BOTH `ToolRegistry::definitions()` AND `xdev::builtin_one_liner()` (`src/xdev.rs:148-193` / `src/cli.rs:3617-3633`), making them invisible to the model; `DebugTool::new` (`src/debug.rs:62`) ignores `_config`. |
| 6 | Default FrankenTUI (`--inline` / `--alt-screen`) + `--classic` TUI + terminal image protocols | **PARTIAL** | `bd-2crrf` (`LANDED_UNCLOSED` partial), `bd-dxz76`, `bd-oyckr`; **NO_BEAD** for dead `terminal_images` & double session init | FTUI (`src/interactive_ftui.rs`) and Classic (`src/interactive.rs`) work, `/goal` wired in both on `main`. **Gaps:** `src/terminal_images.rs` has zero non-test callers; 15 `KeyAction`s inert on FTUI (`src/keybindings.rs:1228-1247`); `main.rs:2103-2537` + `sdk.rs:4394-4481` double-loads `Session`/`Config`/`AuthStorage`/`ToolRegistry`; shipped `v0.8.0` lacks `#282`, `#271`, `#267`, `#264`. |
| 7 | Headless Print/JSON (`-p`, `--mode json`, `--wait-async`), RPC (`--mode rpc`), ACP (`--acp`), Web Remote (`pi web`), Rust SDK (`pi::sdk`) | **PARTIAL / NO_BEAD** | `bd-omp-wait-async-8e2bd` (broken at CLI entry), `bd-rpc-rewind-atomicity-c349e` (untouched); **NO_BEAD** for exit codes, ACP extensions, SDK root visibility | **Print:** `--wait-async` broken in `known_long_option()` (`src/cli.rs:67-157`); `EXIT_CODE_COST_CAP == EXIT_CODE_APPROVAL_UNAVAILABLE == 3` (`src/main.rs:90,103`); `--mode json` exits `0` on errors (`10274, 10394`). **RPC:** `rewind` holds locks across `.await` & truncates on summarizer error (`src/rpc.rs:5174-5292`). **ACP:** no extensions (`src/acp.rs:2507`), ephemeral sessions by default (`787`). **Web:** `pi web` lacks TLS on `--bind lan`; stub QR (`src/web_remote.rs:898`). **SDK:** 35 root `pub mod`s lack `#[doc(hidden)]` (`src/lib.rs:89-373`); 4 mutators bypass quarantine (`src/sdk.rs:3312-3386`). |
| 8 | Extensions (QuickJS + Native + WASM), MCP client, Package Manager + lockfile supply chain | **PARTIAL / NO_BEAD** | `bd-sog97.28/.29`; **NO_BEAD** for lockfile verify, GH `#258` Wasmtime advisories, GH `#279` data-dir policy, silent npm stubs | QuickJS + MCP (`src/mcp/`) + capability policy (`src/extensions.rs`) are real. **Gaps:** `PackageManager::resolve` (`src/package_manager.rs:516-596`) ignores `package-lock.json` `digest_sha256`; `wasmtime 47.0.4` has 5 RUSTSEC advisories & `WasmExtensionHandle::handle_slash` (`src/extensions.rs:11091`) is dead; `zod`/`ws`/`node:net`/`node-pty` shims silently fake validation/I/O; `npm:context-mode` (`#279`) writes outside extension root. |
| 9 | Session persistence: JSONL v3 tree, SQLite (`fsqlite 0.4.10`), `SessionStoreV2` sidecar, `pi doctor` | **PARTIAL / NO_BEAD** | **NO_BEAD** for `SessionStoreV2` live-save self-invalidation | JSONL v3 (`src/session.rs`), SQLite (`src/session_sqlite.rs`, `#250` fixed on `main`), and `pi doctor` (`src/doctor.rs`, `#267` fixed on `main`) work. **`SessionStoreV2` (`src/session_store_v2.rs`) is never written on live saves and `Session::save_inner` (`src/session.rs:5327`) invalidates migrated `.v2` indexes on the very first save.** |
| 10 | "Alien Artifact" math runtime controllers & claim-integrity evidence (`budget_summary.json`, swarm readiness, docs parity) | **WRONG_APPROACH / PARTIAL** | `bd-sog97*`, `bd-ri-phase1-full-refresh-rndeg*`, `bd-8pcrt`; **NO_BEAD** for `latency_proxy=0` bug, double-hashing, dead controller claims, `CHANGELOG`/`docs` drift | `CUSUM+Conformal` feeds `latency_proxy = 0.0` (`src/extensions/extension_manager_impl.rs:928-963`); `marshal` double-hashes (`src/extensions/protocol.rs:582-609`); `ExtensionDispatcher`/`VOI`/`IPS-WIS-DR`/`HostcallReactorMesh`/`io_uring` are unreached/inactive in prod despite `README.md:137,185,1881`; `budget_summary.json` stale (`2026-10-24`, `blocked`); swarm readiness `blocked` (25 issues); `CHANGELOG.md` + `docs/sdk.md` + `docs/rpc.md` + `README.md:2335` drifted. |

---

### 11.3 Direct Answers to the Five Reality-Check Questions

1. **What specifically IS working right now?**
   - Shipped `pi 0.8.0` runs cleanly for CLI help, model/provider discovery, `pi --explain-extension-policy`, `pi --explain-repair-policy`, print/RPC/interactive workflows across the 9 full agentic providers, JSONL v3 session trees, QuickJS extensions, MCP stdio/HTTP servers, LSP/DAP/eval/security_scan tools, and `install.sh` (73/73 regression checks green).
   - On `main` (`fb5190eee`), 7 open user-reported GitHub bugs (`#282, #271, #267, #264, #262, #257, #250`) and ~29 open/`in_progress`/`blocked` beads are already fixed in code.
2. **What is NOT working or not yet implemented?**
   - DSR Check 1 (`cargo fmt --check`) is red across 33 files; host DSR config on `trj` is broken; all 5 shipped `v0.8.0` binaries breach `<48 MiB` (up to `74.25 MiB`); 733 commits are unreleased; 12 of 36 built-in tools are unreachable by the model via `xdev`; `--wait-async` fails CLI parsing; print exit codes collide (`3 == 3`) and `--mode json` exits `0` on errors; `gitlab` and `cursor` providers lack tool use (and `cursor` drops conversation history); RPC `rewind` still holds locks across `.await` and discards context on summarizer failure; `PackageManager::resolve`, `install.sh`, and `self_update` skip cryptographic/lockfile verification; `wasmtime 47.0.4` + `fuzz/Cargo.lock` carry 8 RUSTSEC advisories; `SessionStoreV2` and 4+ math controllers do not execute in live sessions as `README.md` claims; `record_budget_overload_signal` computes `latency_proxy = 0.0`; and `budget_summary.json` / swarm claim readiness are red/blocked.
3. **What is blocking us from getting there?**
   - Unsynced `~/.config/dsr/` on `trj`, unformatted commits landing without DSR (`bd-0x31m`, `bd-gate1-fmt-chronically-red-aqxxe`), disabled RCH Windows worker `wsurf`, wedged Agent Mail server PID `632563` holding `.mailbox.activity.lock` for 59+ hours (with missing `50-agent-mail.py` hook on this Linux checkout), unprovisioned operator `minisign` keypair (`bd-yj126`), and split-brain tracker state (`beads.db` was 6 months stale until rebuilt in this audit; ~29 landed beads unclosed; 2 ghost/broken `in_progress` beads).
4. **If we implemented all open and in-progress beads, would we close the gap completely?**
   - **No.** Completing all 68 active beads would fix the perf refresh (`bd-sog97*`, `bd-ri-phase1-full-refresh-rndeg*`), minisign (`bd-yj126`), Windows CI (`bd-m7im4`), RPC `rewind` (`bd-rpc-rewind-atomicity-c349e`), and media caps (`bd-dowvm`), **but would completely miss**: the 12 invisible `xdev` tools, the broken `--wait-async` CLI pre-parser & print/JSON exit codes, startup `package-lock.json` `digest_sha256` verification, the `74.25 MiB` `dist`-vs-`release` binary size breach, the 5 Wasmtime RUSTSEC advisories + dead WASM slash routing + stale `fuzz/Cargo.lock`, the `gitlab`/`cursor` non-agentic provider gaps, the `latency_proxy = 0.0` bug + double-hashing + unreached controller/`SessionStoreV2` claims, silent `zod`/`ws`/`node:net` shims, SDK SemVer module sealing, and the 733-commit `CHANGELOG` / `v0.9.0` release gap.
5. **What goals from the vision are NOT covered by ANY existing bead (`NO_BEAD`)?**
   - Eight distinct `NO_BEAD` clusters (detailed in §11.4 G1–G8 below and converted into self-contained `br` beads in Phase 3a).

---

### 11.4 Bridge Plan (Revised In-Place — Ambition Round 3: Algorithmic, Formal & Systems-Optimal Architecture)

Every gap below unifies structural root-cause elimination with load-bearing, mathematically sound systems engineering (zero decorative math theatre; every optimization gated on incumbent-in-same-invocation proof per `AGENTS.md` Rule 0.5).

#### Gap G1 — Unify Build/Quality Authority, Fast Format Guard, Cross-Target Windows Check, & Pareto-Optimal `<48 MiB` Binary (`P1`, Complexity: `L`)
- **Current state:** Check 1 (`cargo fmt --check`) fails across 33 files (`check-1.log`); `~/.config/dsr/` on `trj` has `tool_name: pi` and zero checks; `.git/hooks/pre-commit` on `trj` lacks staged `rustfmt --check` and `50-agent-mail.py`; `.dsr/repos.yaml:76-77` lacks `--keep-going` (false-closed `bd-7ilwr`); `.dsr/repos.d/pi_agent_rust.yaml:65-70` specifies `--release` (`opt-level = "z"`) while `Cargo.toml:588-594` ships `[profile.dist]` (`opt-level = 3`, `74.25 MiB`: `.text = 48.78 MiB`, `.rodata = 16.60 MiB`, `.eh_frame* = 5.47 MiB` vs `<48 MiB` budget in `src/perf_build.rs:28`); `src/perf_build.rs:44-50` `CANONICAL_PIJS_PERF_FEATURES` lists `"wasm-host"` and omits `"ftui"`; RCH Windows worker `wsurf` is disabled.
- **Target architecture:**
  1. **Single Build-Contract Bijection:** Enforce exact equality across `Cargo.toml` (`[features].default` + `[profile.dist]`), `.dsr/repos.d/pi_agent_rust.yaml`, `src/perf_build.rs`, and host `~/.config/dsr/` via `scripts/check_dsr_contract_sync.py` (wired into `.dsr/repos.yaml` + `pi doctor --only swarm`). Measure the **exact shipped `dist` binary** in `src/perf_build.rs`.
  2. **Knapsack / Pareto Binary-Size Optimization (`74.25 MiB` → `<48 MiB`):** Compute per-crate $(\Delta\text{throughput}, \Delta\text{bytes})$ between `opt-level = "z"` and `opt-level = 3` (`cargo bloat --crates`). Set `[profile.dist] opt-level = "z"` globally across the 500+ cold CLI/schema/provider crates while retaining `[profile.dist.package.<hot>] opt-level = 3` strictly on high-$\Delta v/\Delta w$ hot kernels (`rquickjs-core`, `swc_ecma_parser`, `fsqlite-vdbe`, `ftui-render`), and reclaim the wasted `5.47 MiB` `.eh_frame*` sections under `panic = "abort"`.
  3. **Fail-Fast Hygiene & Cross-Target Windows Gate:** Add `<300 ms` staged-files `rustfmt --check` pre-commit hook stage (`bd-gate1-fmt-chronically-red-aqxxe` + `bd-0x31m`), restore `--keep-going` in `.dsr/repos.yaml:76-77`, and add Linux-hosted `cargo check --locked --bins --lib --target x86_64-pc-windows-msvc` to catch `#257`/`#271`-class Windows compile errors even while `wsurf` is offline.
  4. **Gate Green & Tracker Adjudication:** Format all 33 Rust files, sync `~/.config/dsr/`, run full `dsr quality --tool pi_agent_rust` to green, and batch-adjudicate all ~29 `LANDED_UNCLOSED` beads citing the DSR run receipt.
- **Verification:** `dsr repos validate --repo pi_agent_rust` exits `0`; `dsr quality --tool pi_agent_rust` exits `0` (`snapshot_before == snapshot_after`); shipped `dist` binary `< 50,331,648` bytes with zero regression on `pijs_workload` vs incumbent in the same invocation.
- **Dependencies:** Root blocker for G8 release cut and `LANDED_UNCLOSED` bead closures.

#### Gap G2 — Algebraic Single-Source Tool Catalog: Live `ToolRegistry::discoverable_index()` & `DebugTool` Config Fidelity (`P1`, Complexity: `M`)
- **Current state:** `src/app.rs:228-239` calls `xdev::prompt_index_for(enabled_tools, Some(config))` using `cli.enabled_tools(config)` (`src/cli.rs:3617-3633`), while `xdev::builtin_one_liner` (`src/xdev.rs:148-193`) returns `None` for 13 built-in tools, leaving `ToolRegistry::discoverable_index()` (`src/tools.rs:6801`) unused outside tests. Result: 12 real built-in tools (`manage_skill`, 5 memory tools, `browser`, `computer`, 4 media tools) are invisible to both `ToolRegistry::definitions()` and the `xdev` system-prompt index even when enabled in `settings.json`. Also `DebugTool::new(cwd, _config)` (`src/debug.rs:62`) ignores `_config`, and `jobs`/`hub` register as `Discoverable` (`src/xdev.rs:93`) despite `README.md:448` / `AGENTS.md:305` calling them Default-enabled.
- **Target architecture:**
  1. **Total Bijection Over Tool Metadata:** Unify `KNOWN_TOOL_NAMES`, default `LoadMode`, settings predicate, and one-line summary into one exhaustive `BUILTIN_TOOL_CATALOG` descriptor slice consumed by `ToolRegistry`, `cli.rs`, and `xdev.rs`; route `build_system_prompt` directly through `ToolRegistry::discoverable_index()`, deleting the partial `builtin_one_liner` match table.
  2. **Deterministic Promotion Semantics:** Tools explicitly named in `--tools` promote immediately to `LoadMode::Essential`; settings-enabled tools (`memory.backend = "local"`, `browser.enabled`, `computer.enabled`, `media.*`) register as `LoadMode::Discoverable` and appear in `discoverable_index()` automatically; reconcile `jobs`/`hub` tier across `src/xdev.rs:93` and docs.
  3. **DAP Config Enforcement:** Wire `DebugConfig` into `DebugTool::new` (`src/debug.rs:62`, `src/tools.rs:6430`) for adapter path overrides, timeouts, and allowlists.
- **Verification:** Exhaustive bijection unit test over `BUILTIN_TOOL_CATALOG` + E2E turn test proving all 12 tools appear in the `xdev` system-prompt index when configured and activate/execute cleanly in 1 turn.
- **Dependencies:** Independent; unblocks G8 docs inventory test.

#### Gap G3 — Introspective `Cli::command()` Pre-Parser & Total Headless Exit-Code Contract (`P1`, Complexity: `S–M`)
- **Current state:** `known_long_option()` (`src/cli.rs:67-157`) is a 90-line hand-maintained match table that missed `"wait-async"` (`src/cli.rs:550`), breaking `pi -p --wait-async "prompt"` (`bd-omp-wait-async-8e2bd`). In `src/main.rs`, `EXIT_CODE_COST_CAP` (`90`) and `EXIT_CODE_APPROVAL_UNAVAILABLE` (`103`) both equal `3`; `--mode json` returns `Ok(())` (exit `0`) on empty input (`10274-10278`) and `StopReason::Error` (`10394-10403`); explicit `--skill`/`--prompt-template`/`--theme` load errors at `2231` exit `1` instead of `EXIT_CODE_RESOURCE_LOAD_FAILED = 4`.
- **Target architecture:**
  1. **Zero-Drift Clap Introspection:** Replace `known_long_option()`'s manual string table with a `OnceLock` lookup synthesized from `<Cli as clap::CommandFactory>::command().get_arguments()` (extracting every long name, visible/hidden alias, and `ArgAction::takes_values()` arity automatically).
  2. **Injective Exit-Code Mapping Across Text & JSON Modes:** Define a single `CliExitCode` enum (`0` Ok, `1` TurnOrRuntimeError, `2` UsageError, `3` ApprovalUnavailable, `4` ResourceLoadFailed, `5` CostCapExceeded), flush terminal NDJSON error frames in `--mode json` before propagating non-zero exit status, and map explicit resource load errors at `src/main.rs:2231` to `4`.
- **Verification:** Subprocess regression `pi -p --wait-async "hello"`, property test iterating every `Cli::command()` flag/alias + positional prompt through `preprocess_extension_flags`, and subprocess exit-code matrix across `--mode text` and `--mode json`.
- **Dependencies:** Directly unblocks `bd-omp-wait-async-8e2bd`.

#### Gap G4 — Canonical Merkle Lockfile Verification, Minisign, & Zero-RUSTSEC Dependency Posture (`P1`, Complexity: `M`)
- **Current state:** `PackageManager::resolve()` (`src/package_manager.rs:516-596`) skips `package-lock.json` `digest_sha256` verification (`1935-1985`); `install.sh:1931-1995` and `src/self_update.rs:289-305` verify SHA-256 only (`bd-yj126`); `Cargo.lock:8813` selects `wasmtime 47.0.4` (5 RUSTSEC advisories, GH `#258`); `WasmExtensionHandle::handle_slash` (`src/extensions.rs:11091`) is dead; `fuzz/Cargo.lock:4034` is stale at `0.6.1` (3 RUSTSEC advisories).
- **Target architecture:**
  1. **Platform-Canonical Merkle Package Digest Gate:** Verify installed package trees against `package-lock.json` `digest_sha256` during `PackageManager::resolve()` using sorted `(normalized_rel_path, exec_bit, sha256(content))` tree hashing (fail closed on post-install tampering with remediation hint `pi update <pkg>`).
  2. **WASM & Fuzz Security Remediation:** Upgrade `wasmtime` to `>= 48.0.5`, wire `WasmExtensionHandle::handle_slash` into slash-command dispatch (`src/extensions.rs`), and refresh `fuzz/Cargo.lock` to zero RUSTSEC advisories.
  3. **End-to-End Ed25519/Minisign Verification:** Enforce `.minisig` verification against pinned `minisign.pub` in both `install.sh` and `src/self_update.rs` (`bd-yj126`).
- **Verification:** Planted 1-byte post-install package mutation fails closed at startup; WASM slash dispatch E2E test passes; zero RUSTSEC advisories in `Cargo.lock` and `fuzz/Cargo.lock`.
- **Dependencies:** Minisign step gated on operator keypair (`bd-yj126`); lockfile + Wasmtime + fuzz lock have zero external blockers.

#### Gap G5 — Kung-Robinson OCC RPC `rewind`, Unified `RecoverySupervisor`, Single-Bootstrap FTUI, ACP/Web Hardening, & Sealed SDK (`P1`, Complexity: `XL`)
- **Current state:** `src/rpc.rs:5174-5292` (`bd-rpc-rewind-atomicity-c349e`) holds `session` + `guard.session` locks across `summarize_span(...).await` and truncates context via `.unwrap_or_else` on summarizer failure; `interactive.rs`, `rpc.rs`, and `sdk/recovery.rs` maintain 3 divergent retry/failover engines and `/login` leaves `FailoverOptions` stale (`src/sdk/recovery.rs:397-400`); FTUI double-boots `Session`/`Config`/`AuthStorage`/`ModelRegistry`/`ToolRegistry` (`src/main.rs:2103-2537` + `src/sdk.rs:4394-4481`); 15 FTUI `KeyAction`s (`src/keybindings.rs:1228-1247`) and `src/terminal_images.rs` are dead; `src/acp.rs:787,2507` lacks `ExtensionRegion` and persistent session defaults; `pi web --bind lan` (`src/web_remote.rs`) permits cleartext HTTP bearer auth and `render_half_block_qr` (`898`) is a stub; `src/lib.rs:89-373` leaks 35 unhidden root `pub mod`s and `src/sdk.rs:3312-3386` skips `ProviderAdmissionGate`; `npm:context-mode` (`GH #279`) fails writing outside extension root.
- **Target architecture:**
  1. **Kung-Robinson 3-Phase OCC RPC `rewind`:** `(ReadPhase)` capture immutable message span + `mutation_epoch: u64` in `<10µs` and drop all session locks → `(SummarizePhase)` await `summarize_span` lock-free so stdin cancellation and extension callbacks run deadlock-free; if `Err`, abort without mutating context → `(ValidateWritePhase)` re-acquire lock, verify `session.mutation_epoch == read_epoch` (rejecting concurrent edits), and atomically install rewound projection + durable save. Prove linearizability under deterministic `LabRuntime` schedule interleavings.
  2. **Unified `RecoverySupervisor` & Single-Bootstrap FTUI:** Route FTUI, RPC, Print, and ACP through one `RecoverySupervisor` backed by live `Arc<Mutex<AuthStorage>>` (so `/login` and `/logout` update failover eligibility immediately); pass pre-loaded resources from `main.rs` into FTUI `SessionOptions` (1 boot); wire `terminal_images` into TUI attachment preview; bind or prune the 15 inert FTUI `KeyAction`s.
  3. **ACP, Web Remote, & Extension State Confinement:** Boot `ExtensionRegion` + standard session persistence in `src/acp.rs`; require TLS (`--tls-cert/--tls-key` or explicit `--allow-insecure-lan`) on `pi web --bind lan` and implement real Reed-Solomon half-block QR encoding in `src/web_remote.rs:898`; grant capability-scoped `~/.pi/agent/extension-data/<ext-id>/` storage (resolving `#279` safely).
  4. **SDK SemVer Sealing:** Mark all 35 internal root `pub mod`s `#[doc(hidden)]` in `src/lib.rs`, update `examples/basic_sdk.rs` to `pi::sdk::*`, and gate all 4 `AgentSessionHandle` mutators through `ensure_mutation_allowed`.
- **Verification:** `LabRuntime` concurrent rewind/callback/cancel/error matrix passes; FTUI boots auth/config/session once; `/login` unlocks fallback provider mid-session; `cargo doc --no-deps` documents only `pi::sdk`.
- **Dependencies:** Verified via G1 DSR gate.

#### Gap G6 — Agentic Provider Parity & Explicit Capability Introspection (`gitlab`, `cursor`, `copilot`, `openai-codex` OAuth) (`P2`, Complexity: `M–L`)
- **Current state:** `src/providers/gitlab.rs` is batch-only with zero tool support and `Usage::default()`; `src/providers/cursor.rs:1065-1070` sends only `latest_user_text` (dropping conversation history) and drops all tools; `src/providers/copilot.rs:382-403` drops `.with_reasoning`/`.with_input_types`/`.with_model_cost` on Chat Completions and freezes `github_token`; `src/auth.rs:2304-2359` skips OAuth refresh when `openai-codex` tokens omit `token_url`/`client_id`.
- **Target architecture:**
  1. Implement multi-turn history + tool schema/call/result translation + streaming/usage parsing in `src/providers/gitlab.rs` and `src/providers/cursor.rs`, AND add `ProviderCapabilities { multi_turn, tool_use, streaming, vision }` to `Provider` (`src/provider.rs`) surfaced in `pi --list-providers` and `pi doctor` so capability contracts are machine-verified.
  2. Forward reasoning/input_types/model_cost + live token refresh in `src/providers/copilot.rs`, and add default `openai-codex` OAuth endpoint metadata fallback in `src/auth.rs`.
- **Verification:** Deterministic VCR multi-turn tool-call suites for `gitlab`, `cursor`, and `copilot` + OAuth refresh unit tests.
- **Dependencies:** Feeds `bd-provider-live-validation-11-xme9d`.

#### Gap G7 — Prebaked QuickJS Bytecode (`<5 ms` Cold Load), Adaptive Conformal Overload Control, Honest Shims, & `SessionStoreV2` Append-Log (`P2`, Complexity: `L`)
- **Current state:** `ext_cold_load_simple_p95` fails perf gate (`11.9 ms` vs `5.0 ms`); `record_budget_overload_signal` (`src/extensions/extension_manager_impl.rs:928-963`) overwrites `cusum.last_observation_ms = Some(now_ms)` before computing `now_ms - prev`, feeding `latency_proxy = 0.0` on 100% of calls; `HostcallRewriteEngine::marshal` (`src/extensions/protocol.rs:582-609`) hashes payloads twice; `zod` (`src/extensions_js.rs:18423`), `node:net` (`15566`), `ws` (`18567`), and `node-pty` (`17859`) silently fake success; `ExtensionDispatcher`, `VOI`, `IPS/WIS/DR`, `IntentGraph`, `HostcallReactorMesh`, and `io_uring` have zero live production execution despite `README.md` claims; `Session::save_inner` (`src/session.rs:5327`) invalidates `.v2` sidecars on every live save.
- **Target architecture:**
  1. **Prebaked QuickJS Prelude Bytecode Image (`11.9 ms` → `<2.5 ms`):** Precompile the `pi` + `node:*` shim bundle once into an immutable QuickJS bytecode image (`JS_WriteObject` / `JS_ReadObject`) instead of re-evaluating JS shim modules per cold realm, solving `ext_cold_load_simple_p95 <= 5.0 ms` with incumbent-in-same-invocation benchmark proof.
  2. **Adaptive Conformal Inference (Gibbs-Candès ACI) + Page's CUSUM on True Service Latency & Single-Hash Marshaling:** Pass measured wall-clock hostcall service duration $L_t = t_{\text{done}} - t_{\text{start}}$ into `record_budget_overload_signal` (fixing the `0.0` bug) with $O(1)$ Adaptive Conformal step $\alpha_{t+1} = \alpha_t + \gamma(\alpha - \mathbf{1}\{L_t > \hat{Q}_{1-\alpha_t}\})$ for distribution-free coverage under non-stationary burst load, and compute payload hash once via `PrehashedPayload` in `HostcallRewriteEngine::marshal`.
  3. **Honest Runtime Shims:** Enforce real structural validation in built-in `zod` (throwing `ZodError` on schema violations) and make `node:net`, `ws`, and `node-pty` throw `ERR_PI_SHIM_UNSUPPORTED_OPERATION` on active I/O calls instead of emitting fake `'connect'`/`'open'` events.
  4. **Incremental `SessionStoreV2` Append-Log & Honest Controller Gating:** Wire `SessionStoreV2::append_frame` (CRC32C + SHA-256 hash chain) into `Session::save_inner` delta saves so $O(\text{index} + \text{tail})$ resume is genuinely live instead of self-invalidating on save #1, and gate unreached experimental controllers (`ExtensionDispatcher`, `VOI`, `IPS/WIS/DR`, `IntentGraph`, `io_uring` stub) behind `#[cfg(test)]` / `experimental-controllers` while aligning `README.md` + `math-reachability-runpack.json` with verified production paths.
- **Verification:** `pijs_workload` proves `ext_cold_load_simple_p95 < 5.0 ms` against live incumbent; ACI/CUSUM unit tests verify non-zero quantile tracking under regime shift; 1-hash-per-hostcall assertion; JS conformance tests for `zod` + fail-closed `ws`/`node:net`; 100-turn incremental `.v2` append/reload round-trip test.
- **Dependencies:** Directly unblocks `bd-sog97.5` and `bd-ri-phase1-full-refresh-rndeg`.

#### Gap G8 — Automated Doc-Code Bijections, Fresh Perf Evidence, & `v0.9.0` DSR Release (`P2`, Complexity: `M`)
- **Current state:** `733` commits since `v0.8.0` missing from `CHANGELOG.md`; `budget_summary.json` expired (`2026-10-24`, `check_readme_evidence_freshness.py` exit `1`); `README.md`, `docs/tui.md:90`, `src/autocomplete.rs:938`, `docs/sdk.md`, `docs/rpc.md`, `docs/rollout_defaults.md`, `docs/planning/FEATURE_PARITY.md`, and `/data/projects/AGENTS.md` (`bd-8pcrt`) have drifted; 7 fixed GitHub issues (`#282, #271, #267, #264, #262, #257, #250`) await release closure.
- **Target architecture:** Regenerate strict `budget_summary.json` via DSR perf lane (`bd-ri-phase1-full-refresh-rndeg`) after G1+G7, rebuild `CHANGELOG.md` `[Unreleased]` → `v0.9.0`, reconcile all drifted docs + `/data/projects/AGENTS.md` (`bd-8pcrt`), add automated drift tests (`tests/rpc_doc_inventory.rs`, `tests/cli_exit_codes_doc.rs`), publish `v0.9.0` (`<48 MiB`, 5 targets, `.sha256` + `.minisig`) via `dsr build` + `dsr release` + `dsr verify`, and close the 7 fixed GitHub issues citing `v0.9.0`.
- **Verification:** `python3 scripts/check_readme_evidence_freshness.py` exits `0`; `python3 scripts/report_swarm_claim_readiness.py --json` reports `0` blocking issues (or explicit dated waivers); `gh release view v0.9.0` + `dsr verify` exit `0`.
- **Dependencies:** Depends on G1–G7.

---

### 11.5 Execution Order & Critical-Path DAG (2026-11-12)

```
Track A (Immediate Parallel Unblockers — Zero External Blockers):
  G1a (fmt 33 files + DSR/perf_build sync + --keep-going + Windows check + <48 MiB profile)
  G2  (ToolRegistry::discoverable_index + BUILTIN_TOOL_CATALOG + DebugTool config)
  G3  (Cli::command() pre-parser fixing --wait-async + injective Print/JSON exit codes)
  G4a (PackageManager lockfile Merkle verify + wasmtime >=48.0.5 + WASM slash + fuzz lock)
  G5a (Kung-Robinson OCC RPC rewind + SDK #[doc(hidden)] & admission gate + /login live failover)
  G7a (Prebaked QuickJS prelude <5ms + ACI/CUSUM latency_proxy fix + 1-hash marshal + zod/ws shims + SessionStoreV2 append)
       │
       ▼
Track B (Authoritative Gate & Perf Regeneration):
  G1b (Full `dsr quality --tool pi_agent_rust` GREEN receipt → close ~29 LANDED_UNCLOSED beads)
       │
       ├──► G6 (GitLab/Cursor multi-turn + tools + ProviderCapabilities + Copilot/Codex OAuth)
       ├──► G5b (Single-bootstrap FTUI + ACP ExtensionRegion/sessions + Web Remote TLS/QR + #279 ext-data dir)
       └──► G8a (DSR perf refresh → green `budget_summary.json` + doc-code bijection tests + CHANGELOG 733-commit rebuild)
                 │
                 ▼
Track C (Operator Signing + Public Release Closure):
  G4b (`bd-yj126` minisign keypair + install.sh/self_update verify) ──► G8b (`dsr build` + `dsr release pi_agent_rust v0.9.0` + `dsr verify` + close 7 GH issues)
```

### 11.6 Phase 3a & Phase 5 Beads Created and Refined (2026-11-12, 5 Refinement Passes, 0 Cycles)

| Bead ID | Priority | Gap | Summary & Exclusive File Scope |
|---|---|---|---|
| `bd-dsr-authority-dist-size-budget-dk9av` | **P0** | G1a | Format 33 red files, add `--keep-going` + Check 8 + Linux-hosted `x86_64-pc-windows-msvc` check in `.dsr/repos.yaml`, sync host DSR config, and record bound 8/8 green DSR receipt |
| `bd-dist-binary-size-under-48mib-d7cij` | **P1** | G1b | Restore `<48 MiB` shipped `[profile.dist]` binary budget (from `74.25 MiB`) via per-crate Pareto optimization + `.eh_frame` stripping & unify `src/perf_build.rs` / `.dsr/repos.d/` contract |
| `bd-xdev-registry-tool-discovery-17sy3` | **P1** | G2 | Wire live `ToolRegistry::discoverable_index()` + exhaustive `BUILTIN_TOOL_CATALOG` so all 12 hidden tools reach the `xdev` system-prompt index & enforce `DebugConfig` in `DebugTool::new` |
| `bd-cli-clap-preparser-print-exit-codes-5dem0` | **P1** | G3 | Derive `known_long_option()` from `<Cli as CommandFactory>::command()` (fixing `--wait-async` for `bd-omp-wait-async-8e2bd` with subcommand safety) & unify injective Print/JSON exit codes |
| `bd-package-lock-merkle-verify-startup-qh50t` | **P1** | G4a | Enforce fail-closed startup `package-lock.json` `digest_sha256` Merkle verification (`<5 ms` streaming SHA-256) in `PackageManager::resolve()` |
| `bd-wasmtime-rustsec-slash-fuzz-lock-106bs` | **P1** | G4b | GH `#258`: Upgrade `wasmtime >= 48.0.5` (5 RUSTSEC advisories, keeping `wasm-host` optional), wire dead `WasmExtensionHandle::handle_slash`, & refresh stale `fuzz/Cargo.lock` |
| `bd-sdk-surface-sealing-recovery-acp-web-yrqff` | **P1** | G5a | Seal 35 root `pub mod`s in `src/lib.rs` with `#[doc(hidden)]`, enforce `ensure_mutation_allowed()` on 4 `AgentSessionHandle` mutators, & make `/login` refresh `FailoverOptions` live |
| `bd-acp-web-remote-extensions-tls-qr-ujsyt` | **P1** | G5b | Boot `ExtensionRegion` + persistent sessions in `src/acp.rs`, enforce LAN TLS on `pi web --bind lan`, & implement real ISO/IEC 18004 Reed-Solomon half-block QR rendering (`src/web_remote.rs:898`) |
| `bd-ftui-ext-datadir-images-keyactions-aah5b` | **P1** | G5c | GH `#279`: Add capability-confined `~/.pi/agent/extension-data/<ext-id>/` (`ctx.dataDir`) + FTUI startup error banner, wire `src/terminal_images.rs`, & resolve 15 inert FTUI `KeyAction`s |
| `bd-adjudicate-landed-beads-on-dsr-receipt-thtf4` | **P1** | G1c | Batch-verify and close ~29 already-landed beads (`bd-kgkrq`, `bd-mg6s5`, `bd-y84fr`, `bd-tools-known-names-missing-seven-mbo3g`, etc.) citing bound DSR `check-4.log` receipt |
| `bd-provider-gitlab-cursor-agentic-parity-t6lnl` | **P2** | G6 | Upgrade `gitlab` & `cursor` to multi-turn tool-calling providers, forward `copilot` reasoning/cost/live-auth, add `openai-codex` OAuth fallback, & expose `ProviderCapabilities` |
| `bd-ext-aci-overload-qbc-honest-shims-8h9jw` | **P2** | G7a-c | Fix `record_budget_overload_signal` `latency_proxy = 0.0` bug (ACI+CUSUM), single-hash `marshal`, prebake QuickJS prelude bytecode, enforce real `zod` + fail-closed `ws`/`net`/`node-pty` shims, & cfg-gate unreached controllers |
| `bd-session-store-v2-live-append-log-o2k94` | **P2** | G7d | Wire incremental `SessionStoreV2` `CRC32C` + hash-chain frame appends inside `Session::save_inner` (fixing save-#1 `.v2` invalidation) |
| `bd-v090-docs-changelog-dsr-release-cut-1cs36` | **P1** | G8 | Rebuild 733-commit `CHANGELOG.md`, reconcile drifted docs (`README.md`, `docs/rpc.md`, `docs/sdk.md`, `docs/tui.md`) with automated parity tests, publish `v0.9.0` via DSR, & close 7 fixed GH issues |
