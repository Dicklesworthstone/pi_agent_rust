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

## 7. Bridge plan (ordered by vision impact)

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
- **Gap F.** GH #207 shipped as the `current_time` essential-tier tool
  (`src/current_time.rs`, wired into registry/tiers/default list/goldens,
  unit-tested); GH #182 scoped into bd-oyckr.
