# Perf Budgets Recipe — DSR hidden contracts

This document is the operator-facing reference for every hidden contract
of the DSR perf recipe that produces
`tests/perf/reports/budget_summary.json`. It exists because
`bd-ri-phase1-recipe-audit` (the gate before the run) found that
several silent failure modes were *almost* failing the run without
producing a useful error.

If any of these contracts is violated, the resulting evidence is
invalid per AGENTS.md "DSR-Only" rule and the bead cannot be closed.

## 1. The authoritative tool

```
/Users/jemanuel/projects/doodlestein_self_releaser/dsr
```

On the release-operator host this is reachable as `dsr` via a symlink in
`~/.local/bin`. Do not assume it: `which dsr` returning non-zero on a
fresh shell is a recipe-audit failure, and the absolute path above always
works.

```bash
DSR=/Users/jemanuel/projects/doodlestein_self_releaser/dsr
"$DSR" quality --tool pi_agent_rust --dry-run \
  --work-dir /Users/jemanuel/projects/pi_agent_rust
```

## 2. The DSR quality recipe (7 checks, registered in `~/.config/dsr/repos.yaml`)

For `pi_agent_rust`, the recipe is, as reported by
`dsr quality --tool pi_agent_rust --dry-run` on 2026-09-21:

1. `cargo fmt --check`
2. `RCH_REQUIRE_REMOTE=1 RCH_BUILD_TIMEOUT_SEC=3600 CARGO_BUILD_JOBS=2 rch exec -- cargo check --locked --all-targets`
3. `RCH_REQUIRE_REMOTE=1 RCH_BUILD_TIMEOUT_SEC=3600 CARGO_BUILD_JOBS=2 rch exec -- cargo clippy --locked --all-targets -- -D warnings`
4. `RCH_REQUIRE_REMOTE=1 RCH_BUILD_TIMEOUT_SEC=3600 RCH_TEST_TIMEOUT_SEC=7200 CARGO_BUILD_JOBS=2 rch exec -- env TMPDIR=/tmp CARGO_INCREMENTAL=0 CARGO_PROFILE_TEST_DEBUG=0 PI_PROVIDER_REPLAY_GIT_COMMIT="$(git rev-parse HEAD)" cargo test --locked --all-targets --no-fail-fast`
5. `bash tests/installer_regression.sh`
6. `python3 scripts/check_module_reachability.py`
7. `python3 scripts/check_fixture_read_patience.py`

`scripts/check_readme_evidence_freshness.py` is **not** in this recipe,
so nothing in the gate notices when README evidence claims drift away
from `tests/perf/reports/budget_summary.json`. That is how the README
spent a month advertising four passing budgets against an artifact whose
rows were all `NO_DATA` (`bd-readme-evidence-table-diverged-ba8bd`).

To wire it in, the line for `~/.config/dsr/repos.yaml` is:

```yaml
      - python3 scripts/check_readme_evidence_freshness.py --structural-only
```

**Use `--structural-only` in the gate, and only there.** The default mode
also enforces a 14-day age limit on every cited artifact. In a per-commit
gate that is a time bomb: nobody refreshes `budget_summary.json` on a
fortnightly cadence, so the check would turn red on the calendar, with no
commit to blame and nothing the committer could do about it — and a gate
that reddens by itself is one everybody learns to ignore. This project
has enough of those already.

`--structural-only` keeps every check that compares the README against
what the artifacts currently say: per-budget statuses in the evidence
table, labelled aggregates anywhere in the README, the claim bindings,
missing artifacts, and the release-authorization contract. All of those
are properties of the commit, and stay true until somebody edits one side.

The full check, age limits included, belongs where it already is: the
pre-release list in `docs/releasing.md`, where a stale artifact genuinely
should block a release.

Tracked as `bd-readme-freshness-into-recipe-5sgos`. The registry lives
outside this repository, so adding the line is an operator action on the
release host, not a change anybody can land here.

## 3. Hidden contract: build scratch on the Data volume

The `CARGO_TARGET_DIR=/tmp/pi-agent-rust-dsr/{check,test}` hardcode this
section was written about is **gone** from the registered recipe; the
checks quoted in section 2 are verbatim from
`~/.config/dsr/repos.yaml` as of 2026-09-21. What remains is `TMPDIR=/tmp`
on the test check, and rch's own target directories on the worker.

The underlying hazard has not gone away: `/tmp` on macOS is
`/private/tmp` on the Data volume, and a 19-budget perf run can produce
100GB+ of churn in a single day. So before a perf run, either point the
scratch at `$RCH_TARGET_BASE` (or the external-NVMe equivalent), or
accept the pollution as a known cost and clean up afterwards with
`sbh check --need 20G` or equivalent.

The preflight script (`scripts/perf/preflight_dsr_recipe.sh`)
warns on this contract and surfaces a `DSR_TMP_TARGET_DIR_USED`
finding in the runpack.

## 4. Hidden contract: `rch` worker fleet

DSR routes cargo invocations through `rch` (per the AGENTS.md
"RCH" section). For the run to be valid:

- `rch diagnose` must show at least one healthy worker.
- The worker must support the host target triple
  (the recipes are x86_64-linux-gnu and aarch64-darwin).
- If the fleet is unreachable, plain `rch exec` falls back to local cargo
  silently; the resulting evidence is **invalid per AGENTS.md**.

Observed 2026-09-01: running the recipe from a git worktree under
`/data/tmp/...` made every worker refuse with "Project path normalization
failed", and `rch exec` then compiled `cargo check --all-targets` locally on a
load-50 swarm host while DSR kept reporting the check as running. Two
mitigations are now baked into the recipe (`.dsr/repos.yaml` and the host
registry):

- Every cargo check is prefixed with `RCH_REQUIRE_REMOTE=1`, rch's
  fail-closed proof mode: no eligible worker means the check fails with an
  rch refusal instead of a local compile. (`RCH_FORCE_REMOTE`, which older
  scripts use, still fails open.)
- `RCH_BUILD_TIMEOUT_SEC=3600` / `RCH_TEST_TIMEOUT_SEC=7200` raise rch's
  300 s / 1800 s defaults, which a cold all-targets check or test of this
  crate exceeds.

Run the recipe from the primary checkout path (`/data/projects/pi_agent_rust`
on the swarm host), not from a temporary worktree, and do not edit the tree
while it runs: DSR snapshots `HEAD`, the porcelain status, and `Cargo.lock`
before and after and marks the run `invalidated-moving-source` on any change.

The preflight runs `rch diagnose` and refuses to proceed with
`RCH_QUIET=1` or empty fleet.

## 5. Hidden contract: `rchignore`

The file `.rchignore` at the repo root excludes `_tmp*`, `codex*`,
`artifacts/`, `ubuntu*/`, `legacy_pi_mono_code/pi-mono/node_modules/`,
etc. from being shipped to the remote worker. The recipe assumes
this file is present and well-formed.

The preflight verifies `.rchignore` exists and contains the
required `legacy_pi_mono_code/pi-mono/node_modules/` exclusion
(it is the source of the pi-mono baseline comparison).

## 6. Hidden contract: perf evidence cache

`tests/perf/reports/budget_summary.json` may consume cached
evidence from `$CARGO_TARGET_DIR/perf/evidence_cache` (default)
with schema `pi.perf.evidence_cache.v1` and TTL controlled by
`PI_PERF_EVIDENCE_CACHE_TTL_HOURS` (default 168h).

For a *strict* run (the one that backs README claims), the
operator must:

- `unset PERF_EVIDENCE_DIR PERF_EVIDENCE_DIRS PI_PERF_POST_GENERATION`
  before the run, OR explicitly set `PI_PERF_STRICT=1`.
- Ensure the cache does not have a `pass` entry with
  `correlation_id` that the harness would accept as fresh.

## 7. Hidden contract: env_fingerprint.json

The `tests/perf/reports/env_fingerprint.json` artifact
(schema `pi.perf.host_topology_fingerprint.v1`) records
cgroup v2 CPU quota, cpuset size, NUMA topology, memory limits.
For the run to be reproducible:

- The host must have cgroup v2 enabled (or the harness emits
  a caveat that the fingerprint is incomplete).
- The fingerprint must be re-recorded on every run (the
  orchestrator does this automatically).
- The fingerprint's `budget_profile` must be in
  `["full", "constrained", "minimal"]`; `unknown` is a recipe-
  audit failure.

## 8. Hidden contract: closeout-evidence-registry freshness

The `docs/contracts/closeout-evidence-registry.json` indexes 65
closeout-gate artifacts. The `check_closeout_gate_freshness.py`
script (referenced in `scripts/`) enforces a freshness window.
A blocked budget_summary with a stale registry is a recipe-audit
failure; the operator must re-run any stale gate before the
budget summary is allowed to flip `claim_readiness`.

## 9. Hidden contract: preflight_budget_inputs.py

The `scripts/perf/preflight_budget_inputs.py` script (already
present, well-documented, schema
`pi.perf.budget_preflight.v1`) lists missing budget inputs
and expected artifact paths. The recipe requires this script
to exit 0 before the orchestrator (`scripts/perf/orchestrate.sh`)
is allowed to proceed.

The preflight in this bead wraps this script and treats any
non-zero exit as a recipe-audit failure.

## 10. Hidden contract: phase1_matrix_validation.json

The phase-1 matrix validation
(`tests/perf/reports/phase1_matrix_validation.json`,
schema `pi.perf.phase1_matrix_validation.v1`) requires:

- Five scale points: 100k, 500k, 1M, 2M, 5M tokens.
- Per-scale weighted-contribution attribution
  (the formula in README L1264-1278).
- 95% confidence intervals via the `n_eff` formula.
- A non-null `correlation_id` matching the orchestrator's
  `CI_CORRELATION_ID` env var.

A missing or stale matrix validation is a recipe-audit
failure.

## 11. How to run the preflight

```bash
bash scripts/perf/preflight_dsr_recipe.sh \
  --dsr /Users/jemanuel/projects/doodlestein_self_releaser/dsr \
  --work-dir /Users/jemanuel/projects/pi_agent_rust \
  --out docs/evidence/ri-phase1-recipe-audit-runpack.json
```

Exit 0 = ready to run DSR.
Exit 1 = at least one contract violated; see runpack for
which one.

## 12. How to run DSR end-to-end

```bash
DSR=/Users/jemanuel/projects/doodlestein_self_releaser/dsr
"$DSR" quality --tool pi_agent_rust \
  --work-dir /Users/jemanuel/projects/pi_agent_rust
```

The output is the orchestrator run; the budget_summary is
written by the `perf_budgets` step. Verify with:

```bash
jq '.claim_readiness' tests/perf/reports/budget_summary.json
```

## 13. Anti-patterns to refuse

- Do not run DSR without the preflight green. The preflight
  exists because past runs have produced invalid evidence.
- Do not let DSR fall back to local cargo silently.
  Set `RCH_REQUIRE_REMOTE=1` to fail closed.
- Do not accept a budget_summary that has any FAIL or NO_DATA
  in the ci_enforced set.
- Do not let `/tmp/pi-agent-rust-dsr` accumulate; clean it up
  with `sbh check` after the run.
- Do not cite the README's "5-7ms startup" / "23-32MB binary" /
  "~4.9MB RSS" numbers until the budget_summary's
  `performance_claims_authorized` is `true`.
