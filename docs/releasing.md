# Releasing pi_agent_rust

This repo ships:
- A crates.io package: `pi_agent_rust` (Cargo `[package].name`)
- A library crate: `pi` (Cargo `[lib].name`)
- A binary: `pi` (Cargo `[[bin]].name`)

The Cargo source package also retains the internal `pi_legacy_capture`
conformance utility because integration tests execute it through
`CARGO_BIN_EXE_pi_legacy_capture`. It is gated by the non-default
`internal-legacy-capture` feature and is not a supported release artifact.
Ordinary `cargo install pi_agent_rust --locked` therefore installs only `pi`;
repository gates that cover the utility explicitly enable its internal feature.

## Versioning + tags (source of truth)
**Source of truth:** `Cargo.toml` `[package].version`.

- **Tag format:** `vX.Y.Z` (SemVer). Example: `v0.2.0`.
- **Pre-releases:** `vX.Y.Z-rc.1` (or similar). Example: `v0.2.0-rc.1`.
- **Coupling:** `pi_agent_rust` (crate), `pi` (lib), and `pi` (binary) are all built from the same package, so they share one version number.
- **Sibling repos:** `asupersync`, `rich_rust`, `charmed_rust`, `sqlmodel_rust` are versioned independently in their own repos.

### Publishing to crates.io
`.github/workflows/publish.yml` is a manual-dispatch, non-authoritative
diagnostic. It validates an annotated tag, the exact root package identity, a
clean frozen checkout, the release gate, and `cargo publish --dry-run --locked`.
It has no registry secret and never publishes anything.

Stable crates.io publication is owned only by `.github/workflows/release.yml`.
That workflow first creates or safely completes a verified GitHub draft, builds
and inspects the exact `.crate` without a secret, then passes the crate and a
source-bound checksum receipt to a fresh review-gated runner. The fresh runner
executes `cargo publish --locked --no-verify --registry crates-io`; its custom
Cargo credential provider releases the token only when Cargo identifies the
canonical crates.io registry and presents the exact verified crate
name/version/SHA-256. The workflow then requires crates.io to report the exact,
non-yanked version before it makes the GitHub release public. Pre-releases skip
crates.io entirely.

Release and publish workflows resolve the sibling-project crates from crates.io
under `Cargo.lock`; they do not build against arbitrary sibling repository
checkouts. Per-target build manifests therefore record selected locked crate
versions, registry sources, and checksums rather than unrelated repository HEADs.

### Publishing GitHub Releases binaries
`.github/workflows/release.yml` is triggered on tag pushes matching `v*` and will:
- run the full frozen-SHA format/check/clippy/test and release-evidence gates
- build `pi` for Linux/macOS/Windows and reject every native binary whose raw
  executable size is greater than or equal to 22 MiB (23,068,672 bytes)
- attach platform archives, per-target build manifests, and `SHA256SUMS` to a
  verified draft, preserving matching assets and adding only missing ones on a
  safe rerun
- mark the GitHub Release as a pre-release when the parsed SemVer has a
  pre-release component (for example, `-rc.1`)
- for stable versions, publish/reconcile the exact crate before making the
  verified GitHub draft public; an already-public exact release is accepted
  only when the exact non-yanked crate already exists

Release notes are extracted only from the exact `## [vX.Y.Z] ...` changelog
heading. Ensure that exact heading exists for the tag you are cutting.

### Required GitHub governance for the automated lane

Workflow YAML cannot make tag refs immutable or turn an auto-created
environment into a protected one. Before enabling the automated lane, an owner
must configure all of the following in repository settings:

- an environment named `release` with at least one required reviewer and
  self-review prevention; store `CARGO_REGISTRY_TOKEN` there and disable
  administrator bypass
- an active tag ruleset covering literal `refs/tags/v*`, with update and
  deletion forbidden and no bypass actors
- repository variable `RELEASE_GOVERNANCE_ACK` set exactly to
  `release-env-reviewers+immutable-v-tags-v1` only after those controls have
  been inspected

The workflow queries the observable environment and active ruleset shape and
fails closed when either is absent, unreadable, inactive, or malformed. GitHub
normally redacts ruleset `bypass_actors` from read-only callers; omission is
treated as unproven and fails closed rather than being confused with an empty
list. The environment API also does not independently prove the
administrator-bypass setting. Consequently, the automated lane must remain
disabled unless its workflow identity can read an explicit empty bypass list
and the owner has supplied the exact audit acknowledgement above. Do not add a
broad administrator token to a tag-triggered workflow merely to make this gate
green. The manual no-Actions lane also requires server-side tag immutability;
local Git ref checks are defense in depth, not a substitute for it.

**Current state:** ruleset `20418963` was created and read back on 2026-08-04
as active for `refs/tags/v*`, with update and deletion forbidden and no bypass
actors. The manual lane must still re-run the exact governance check below
before tagging and immediately before publication; a missing or changed control
is a hard stop. The automated lane remains disabled because the protected
`release` environment and acknowledgement described above are not configured.
Repeated local ref comparisons are never a substitute for the server-side rule.

## Distribution compatibility strategy (DROPIN-146)
Goal: keep packaging and invocation ergonomics compatible enough for frictionless migration from upstream Pi.

### Supported distribution paths
- **Installer path (`install.sh`)**: default channel for end users; installs GitHub release binary, verifies checksums, and manages migration state.
- **Release artifact path (GitHub Releases)**: direct binary download per OS/arch with `SHA256SUMS` verification.
- **Source path (`cargo build --release --locked`)**: deterministic fallback for constrained/air-gapped environments.

### Executable compatibility path
- Canonical command is `pi`.
- If TypeScript `pi` already exists, installer supports in-place migration and preserves old command as `legacy-pi`.
- If migration is declined (`--keep-existing-pi`), Rust Pi installs as `pi-rust` so both CLIs remain callable.
- Pinned rollout is supported by `install.sh --version vX.Y.Z`.

### Representative validation matrix
Run this matrix before declaring distribution parity complete for a release candidate:

1. Fresh Linux/macOS install (no prior `pi`):
   - `curl .../install.sh | bash`
   - `command -v pi && pi --version && pi --help >/dev/null`
2. Migration host with existing TypeScript `pi`:
   - `install.sh --adopt` (or interactive adopt path)
   - `pi --version` returns Rust build
   - `legacy-pi --version` still resolves to preserved TypeScript CLI
3. Keep-existing path:
   - `install.sh --keep-existing-pi`
   - `pi` remains TypeScript CLI, `pi-rust --version` resolves to Rust build
4. Pinned enterprise/CI rollout:
   - `install.sh --version vX.Y.Z`
   - binary checksum validation passes against release `SHA256SUMS`

## Perf-vs-size artifact policy (bd-3ar8v.5.5)

Release operations must keep benchmark evidence and shipping artifacts distinct.

- **Shipping/distribution artifacts**: built with Cargo `release` profile and published via
  `release.yml` + installer flows (`pi` binaries + `SHA256SUMS`).
- **Benchmark evidence artifacts**: produced by PERF-3X lanes (`scripts/perf/orchestrate.sh`,
  `scripts/bench_extension_workloads.sh`) using benchmark profile labeling (typically `perf`)
  with run-level provenance (`correlation_id`, build/profile metadata, allocator/PGO metadata).

Policy constraints:

1. Performance and certification claims must cite benchmark evidence artifacts, not release-only binaries.
2. Release binaries remain the deployment target and may be used to validate size/startup/install behavior.
3. Any release note claiming performance gains should include correlation-linked evidence references from benchmark artifact bundles.
4. If profile labels/provenance are missing or contradictory, treat the performance claim as invalid until regenerated.

## Swarm-scale claim readiness report (bd-2zcs5.27)

Before using swarm-scale, drop-in, extension, full-suite, or performance evidence in release-facing copy, generate the read-only readiness report:

```bash
python3 scripts/report_swarm_claim_readiness.py --self-test
python3 scripts/report_swarm_claim_readiness.py --json
```

The report emits schema `pi.swarm.claim_readiness_report.v1` and groups artifacts by `perf`, `full_suite`, `dropin`, `extension`, and `activity_ledger`. Its stable top-level machine fields are `overall_status`, `overall_ready`, `blocking_issue_count`, and `blocking_count`; `overall_ready` is the boolean alias for `overall_status == "ready"`, and `blocking_count` is an exact alias of `blocking_issue_count` for operator jq ergonomics. It distinguishes `release_facing` artifacts from `historical_snapshot` or `release_policy` records so old planning snapshots remain visible without automatically authorizing current claims.

```bash
python3 scripts/report_swarm_claim_readiness.py --json \
  | jq '{overall_status, overall_ready, blocking_issue_count, blocking_count}'
```

The same JSON also includes `stale_claims` with schema `pi.swarm.stale_claim_report.v1`. This section is report-only: it never reopens, reassigns, or edits Beads. It classifies `in_progress` beads from `.beads/issues.jsonl` using `--stale-claim-after-hours` and can treat fresher coordination evidence from `--stale-claim-activity-jsonl` rows as active owner evidence within `--stale-claim-activity-fresh-hours`. Each item names the bead ID, assignee, last update, evidence source, classification, and exact recommended operator action so operators can message the owner or manually reopen only after confirmation.

The JSON also includes `hostcall_queue_telemetry` with schema `pi.swarm.hostcall_queue_readiness.v1`. It reads hostcall queue evidence from `tests/perf/reports/stress_triage.json` and `docs/evidence/ext-stress-reactor-queue-coverage.json`, then reports stable counters for `s3fifo_fallback_transitions`, `s3fifo_fairness_rejected_total`, `s3fifo_lane_overflow_rejected_total`, `queue_overflow_rejected_total`, `safe_reclamation_fallback_transitions`, `bravo_transitions_total`, and `bravo_rollbacks_total`. Missing S3-FIFO or BRAVO telemetry is listed in `missing_required_fields` rather than treated as zero; non-zero fallback, fairness-rejection, lane-overflow, or BRAVO rollback totals make the section `fallback_heavy` so operators know not to present the run as contention-clean without more triage.

Use gate mode only when a release path must fail on stale or unsupported evidence:

```bash
python3 scripts/report_swarm_claim_readiness.py --gate
```

Gate mode exits non-zero only for release-facing blockers: missing artifacts, stale generated timestamps, no-data budget summaries, failed verdict fields, schema drift, or mismatched provenance across artifacts that are being used as one claim. Non-gate mode always exits 0 and is suitable for handoff notes, operator dashboards, and stale-evidence triage.

When the report blocks:
- Regenerate the exact artifact path listed when the claim is still intended to be release-facing.
- Split the claim by run when the report identifies multiple provenance values for one category.
- Soften or remove release-facing copy when the only available evidence is a historical snapshot.
- Do not use `docs/parity-certification.json` to override `docs/evidence/dropin-certification-verdict.json` or the report's drop-in blockers.

## When do we call it 1.0?
We call it `1.0.0` when:
- CI is green on Linux/macOS/Windows (`.github/workflows/ci.yml`)
- Required execution surfaces are parity-stable (interactive + print + JSON mode + RPC + SDK contract) with conformance evidence green
- Extension runtime surface and security policy are stable enough that we can commit to not breaking users without an intentional SemVer bump
- Drop-in certification artifacts report `CERTIFIED` for the clean release
  source commit, and the final release ref equals it or contains only
  allowlisted evidence-only descendants, before strict replacement claims are
  used

Until then, `0.x` releases may still change behavior to improve correctness/parity, and release messaging must not claim strict drop-in replacement.

## Cutting a release (patch/minor)
1) **Pick version** (SemVer):
   - patch: bugfixes / internal refactors
   - minor: new user-facing features
2) **Update version** in `Cargo.toml` (`[package].version`).
3) **Run quality gates locally**:
   - `cargo fmt --check`
   - `cargo check --locked --all-targets --features internal-legacy-capture`
   - `cargo clippy --locked --all-targets --features internal-legacy-capture -- -D warnings`
   - `cargo test --locked --all-targets --features internal-legacy-capture`
4) **Update changelog**:
   - `br changelog --since-tag vX.Y.Z` (or use `--since YYYY-MM-DD` if no prior tags)
   - paste the output into `CHANGELOG.md` under a new version heading
5) **Commit** (`git commit`).
6) **Tag according to the selected lane**:
   - automated: synchronize `main` and legacy `master`, create an annotated tag
     at their shared tip, then push it
   - manual/no-Actions: do not pre-create or push the tag here; the fail-closed
     lane below creates it locally only after the final source is frozen, uses
     it for the preserved raw build, and pushes it only after packaging passes
7) **Complete exactly one publication lane**:
   - automated: `Release (GitHub binaries)` completes the ordered draft → exact
     stable crate → public release flow after its external governance gate passes
   - manual/no-Actions: follow every fail-closed step below; do not dispatch,
     rerun, or otherwise invoke a workflow
   - optional `Publish validation (no publication)` is diagnostic only and is
     never evidence that publication occurred

## Manual DSR lane (no GitHub Actions)

Use this lane when the release is intentionally built and published from the
operator hosts. It does not dispatch, rerun, or otherwise invoke a GitHub
Actions workflow. Keep every pushed release-preparation, source, and evidence
commit marked with `[skip actions]`; the commit ultimately referenced by the
tag must contain that marker. Use an annotated tag with the marker as an
additional auditable signal.

Before opening the fail-fast session, freeze every release-source change in one
or more commits whose subjects end in `[skip actions]`, and leave the checkout
completely clean. Run the lane as one fail-fast Bash session (`set -euo
pipefail`); do not copy a later publication command in isolation. Start by
binding all operator state to the intended stable version and a fresh directory
outside the checkout. Replace the two explicit operator-supplied values before
running this block:

```bash
set -euo pipefail
umask 077
export RELEASE_VERSION="X.Y.Z"
export MANUAL_RELEASE_STATE_DIR="/path/outside/checkout/pi_agent_rust-vX.Y.Z-release-state"
[[ "$RELEASE_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]
export RELEASE_TAG="v${RELEASE_VERSION}"
test "$RELEASE_TAG" != "vX.Y.Z"
test ! -e "$MANUAL_RELEASE_STATE_DIR"
mkdir -m 700 "$MANUAL_RELEASE_STATE_DIR"
RELEASE_REPOSITORY="$(gh repo view --json nameWithOwner --jq .nameWithOwner)"
export RELEASE_REPOSITORY
test "$RELEASE_REPOSITORY" = "Dicklesworthstone/pi_agent_rust"
test -z "$(git status --porcelain=v2 --untracked-files=all)"
workflow_baseline="$MANUAL_RELEASE_STATE_DIR/github-actions-baseline.json"
workflow_baseline_proof="$MANUAL_RELEASE_STATE_DIR/github-actions-baseline.txt"
test ! -e "$workflow_baseline" && test ! -e "$workflow_baseline_proof"
gh api -H 'Accept: application/vnd.github+json' \
  "/repos/${RELEASE_REPOSITORY}/actions/runs?per_page=1" > "$workflow_baseline"
jq -e '
  (.total_count | type) == "number" and .total_count >= 0 and
  (.workflow_runs | type) == "array" and (.workflow_runs | length) <= 1 and
  all(.workflow_runs[]; (.id | type) == "number" and .id > 0)
' "$workflow_baseline" >/dev/null
WORKFLOW_BASELINE_ID="$(jq -r '.workflow_runs[0].id // "none"' \
  "$workflow_baseline")"
export WORKFLOW_BASELINE_ID
(set -C; printf 'latest_workflow_run_id=%s\n' "$WORKFLOW_BASELINE_ID" \
  > "$workflow_baseline_proof")
```

Before step 1, prove the active immutable tag ruleset again. The rule must
target `refs/tags/v*` (or all refs), have no exclusions, forbid update and
deletion, and expose an empty bypass-actor list. The command must pass against
the live repository; stop if the control recorded above has disappeared or
changed:

```bash
set -euo pipefail
ruleset_inventory="$MANUAL_RELEASE_STATE_DIR/tag-ruleset-inventory.json"
ruleset_details="$MANUAL_RELEASE_STATE_DIR/tag-ruleset-details.json"
test ! -e "$ruleset_inventory" && test ! -e "$ruleset_details"
gh api --paginate \
  -H 'Accept: application/vnd.github+json' \
  "/repos/${RELEASE_REPOSITORY}/rulesets?includes_parents=true&targets=tag&per_page=100" \
  | jq -s 'add' > "$ruleset_inventory"
jq -e 'type == "array" and length <= 100 and
  all(.[]; (.id | type) == "number")' "$ruleset_inventory" >/dev/null
while IFS= read -r ruleset_id; do
  gh api \
    -H 'Accept: application/vnd.github+json' \
    "/repos/${RELEASE_REPOSITORY}/rulesets/${ruleset_id}?includes_parents=true"
done < <(jq -r '.[].id' "$ruleset_inventory") | jq -s '.' > "$ruleset_details"
jq -e 'any(.[];
  .target == "tag" and .enforcement == "active" and
  ((.conditions.ref_name.include | index("refs/tags/v*")) != null or
   (.conditions.ref_name.include | index("~ALL")) != null) and
  .conditions.ref_name.exclude == [] and
  ([.rules[].type] | index("update")) != null and
  ([.rules[].type] | index("deletion")) != null and
  (.bypass_actors | type) == "array" and .bypass_actors == []
)' "$ruleset_details" >/dev/null
sha256sum "$ruleset_inventory" "$ruleset_details" \
  > "$MANUAL_RELEASE_STATE_DIR/tag-governance.sha256"
```

If the API omits `bypass_actors`, returns more than 100 tag-ruleset summaries,
changes shape, or cannot be read with the operator credential, stop. Absence of
proof is not proof of an empty bypass list.

1. Run the locked repository gates, including the internal capture target:

   ```bash
   set -euo pipefail
   cargo fmt --check
   cargo check --locked --all-targets --features internal-legacy-capture
   cargo clippy --locked --all-targets --features internal-legacy-capture -- -D warnings
   cargo test --locked --all-targets --features internal-legacy-capture
   ```

2. Bind the already-clean release source before generating tracked evidence.
   Fail unless the exact HEAD subject carries the required `[skip actions]`
   marker; this step deliberately performs no empty or implicit commit:

   ```bash
   set -euo pipefail
   source_commit="$(git rev-parse 'HEAD^{commit}')"
   source_subject="$(git show -s --format=%s "$source_commit")"
   case "$source_subject" in
     *'[skip actions]') ;;
     *) printf 'release-source HEAD lacks [skip actions]: %s\n' "$source_subject" >&2; exit 1 ;;
   esac
   test -z "$(git status --porcelain=v2 --untracked-files=all)"
   git diff --quiet "$source_commit" --
   git diff --cached --quiet "$source_commit" --
   ```

3. Generate source-bound conformance evidence explicitly. Do not copy forward
   a historical `CERTIFIED` verdict: unless the canonical full-certification
   pipeline has been rerun successfully against this exact source commit,
   regenerate an honest `NOT_CERTIFIED` verdict with an explicit blocker. This
   is a fail-closed release claim, not a waiver. Commit the generated evidence,
   then run the mandatory manual release gate. Ordinary test runs are read-only
   and do not freshen these tracked artifacts. The v0.x lane does not make a
   strict drop-in claim, while preflight and quality remain required:

   ```bash
   set -euo pipefail
   PI_GENERATE_CONFORMANCE_REPORT=1 \
     cargo test --locked --test conformance_report \
     generate_conformance_report -- --exact --nocapture
   RELEASE_TAG="$RELEASE_TAG" python3 - <<'PY'
   import json
   import os
   import re
   import subprocess
   from datetime import datetime, timezone
   from pathlib import Path

   commit = subprocess.run(
       ["git", "rev-parse", "HEAD^{commit}"],
       check=True,
       capture_output=True,
       text=True,
   ).stdout.strip()
   if re.fullmatch(r"[0-9a-f]{40}", commit) is None:
       raise SystemExit("release source is not bound to a full SHA-1 commit")
   tag = os.environ["RELEASE_TAG"]
   path = Path("docs/evidence/dropin-certification-verdict.json")
   if path.is_symlink() or not path.is_file():
       raise SystemExit("drop-in verdict must remain a regular tracked file")
   payload = {
       "schema": "pi.dropin.certification_verdict.v1",
       "git_commit": commit,
       "generated_at_utc": datetime.now(timezone.utc).replace(microsecond=0)
           .isoformat().replace("+00:00", "Z"),
       "overall_verdict": "NOT_CERTIFIED",
       "hard_gate_results": [],
       "blocking_reasons": [
           f"{tag} is not strict-drop-in certified: the canonical full-certification "
           f"pipeline was not regenerated and proven against source commit {commit}."
       ],
       "evidence_index": [],
       "source": {
           "generator": "manual-release-fail-closed",
           "certification_lane_artifact": "tests/full_suite_gate/certification_verdict.json",
           "lane_verdict": "not-run-for-this-source",
       },
   }
   path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
   PY
   git add \
     docs/evidence/dropin-certification-verdict.json \
     tests/ext_conformance/reports/CONFORMANCE_REPORT.md \
     tests/ext_conformance/reports/conformance_summary.json \
     tests/ext_conformance/reports/conformance_events.jsonl
   git commit -m "Record ${RELEASE_TAG} release evidence [skip actions]"

   RELEASE_GATE_REQUIRE_PREFLIGHT=1 \
   RELEASE_GATE_REQUIRE_QUALITY=1 \
   RELEASE_GATE_REQUIRE_DROPIN_CERTIFIED=0 \
   RELEASE_GATE_CARGO_RUNNER=local \
     ./scripts/release_gate.sh --no-rch --report
   ```

   The gate requires a clean repository at entry and revalidates the exact
   HEAD, canonical source-tree digest, index, index flags, symlink topology,
   untracked paths, and raw worktree bytes after every executable check. Push
   only after it passes, then synchronize the legacy compatibility ref:

   ```bash
   set -euo pipefail
   git push origin main
   git push origin main:master
   ```

4. From that final clean evidence commit, build and inspect the exact Cargo
   source package. Record its SHA-256 and byte size outside the checkout before
   running the dry-run, then prove the dry-run reproduced the same bytes. This
   proof must not predate the final source/evidence commit:

   ```bash
   set -euo pipefail
   cargo package --locked
   crate_path="${CARGO_TARGET_DIR:-target}/package/pi_agent_rust-${RELEASE_VERSION}.crate"
   test -f "$crate_path" && test ! -L "$crate_path"
   source_commit="$(git rev-parse 'HEAD^{commit}')"
   test "$(tar -xOf "$crate_path" \
     "pi_agent_rust-${RELEASE_VERSION}/.cargo_vcs_info.json" \
     | jq -er --arg commit "$source_commit" \
       'select(.git.sha1 == $commit and (.git.dirty // false) == false) | .git.sha1')" \
     = "$source_commit"
   package_sha256="$(sha256sum "$crate_path" | awk '{print $1}')"
   package_size="$(wc -c < "$crate_path" | tr -d '[:space:]')"
   proof_file="$MANUAL_RELEASE_STATE_DIR/pi_agent_rust-${RELEASE_VERSION}-crate.txt"
   test ! -e "$proof_file"
   umask 077
   (set -C; printf 'source_commit=%s\npackage_sha256=%s\npackage_size=%s\n' \
     "$source_commit" "$package_sha256" "$package_size" > "$proof_file")

   cargo publish --dry-run --locked
   test -f "$crate_path" && test ! -L "$crate_path"
   test "$(tar -xOf "$crate_path" \
     "pi_agent_rust-${RELEASE_VERSION}/.cargo_vcs_info.json" \
     | jq -er --arg commit "$source_commit" \
       'select(.git.sha1 == $commit and (.git.dirty // false) == false) | .git.sha1')" \
     = "$source_commit"
   dry_run_sha256="$(sha256sum "$crate_path" | awk '{print $1}')"
   dry_run_size="$(wc -c < "$crate_path" | tr -d '[:space:]')"
   test "$dry_run_sha256" = "$package_sha256"
   test "$dry_run_size" = "$package_size"
   printf 'dry_run_sha256=%s\ndry_run_size=%s\n' \
     "$dry_run_sha256" "$dry_run_size" >> "$proof_file"
   test -z "$(git status --porcelain=v2 --untracked-files=all)"
   printf 'release_tag=%s\n' "$RELEASE_TAG" >> "$proof_file"
   ```

   Stop if the checkout is dirty, the package metadata is not bound to the
   final commit, either equality check fails, or the receipt is not stored
   outside the checkout.

5. Freeze the clean source under a local annotated tag, then use only the
   audited private preservation wrapper for the five raw build legs. This
   preserved v0.2.0 lane is intentionally narrower than ordinary DSR: the
   launcher accepts one exact argument vector, rejects `--no-sync` and every
   resume/release/fallback/cleanup override, snapshots the frozen source into
   fresh per-run paths on the configured build hosts, runs DSR's native-host
   build mode, and produces raw executables only. Do not invoke the private
   `dsr` entrypoint directly, do not substitute canonical `dsr build`, and do
   not treat `--only-native` as proof that every target ran on matching CPU
   hardware: the audited configuration's Linux ARM64 leg is a cross-target
   build on its configured Linux host.

   The preserved lane and its audit are release inputs. Their fixed hashes
   below apply only to v0.2.0. If the path is absent, any hash or mode differs,
   or a later version is being cut, stop and perform a new preservation-lane
   audit; never silently fall back to another DSR invocation.

   ```bash
   set -euo pipefail
   test "$RELEASE_VERSION" = "0.2.0"
   test "$RELEASE_TAG" = "v0.2.0"
   test "$(git rev-parse --show-toplevel)" = "/data/projects/pi_agent_rust"
   source_commit="$(awk -F= '$1 == "source_commit" {print $2}' "$proof_file")"
   [[ "$source_commit" =~ ^[0-9a-f]{40}$ ]]
   test "$(git rev-parse 'HEAD^{commit}')" = "$source_commit"
   test "$(git rev-parse 'main^{commit}')" = "$source_commit"
   test -z "$(git status --porcelain=v2 --untracked-files=all)"

   git fetch --no-tags origin \
     refs/heads/main:refs/remotes/origin/main \
     refs/heads/master:refs/remotes/origin/master
   test "$(git rev-parse 'origin/main^{commit}')" = "$source_commit"
   test "$(git rev-parse 'origin/master^{commit}')" = "$source_commit"
   test -z "$(git tag --list "$RELEASE_TAG")"
   test -z "$(git ls-remote --tags origin \
     "refs/tags/$RELEASE_TAG" "refs/tags/$RELEASE_TAG^{}")"
   git tag -a "$RELEASE_TAG" \
     -m "$RELEASE_TAG manual DSR release [skip actions]" "$source_commit"
   test "$(git cat-file -t "refs/tags/$RELEASE_TAG")" = tag
   test "$(git rev-parse "refs/tags/$RELEASE_TAG^{commit}")" = "$source_commit"
   test "$(git tag --list --format='%(contents:subject)' "$RELEASE_TAG")" = \
     "$RELEASE_TAG manual DSR release [skip actions]"

   export PRESERVED_DSR_LANE="/data/tmp/dsr-preserve-pi-v0.2.0-d33f69b8-9756-4181-9de8-8b30671a9976"
   export PRESERVED_DSR_WRAPPER="$PRESERVED_DSR_LANE/preserved-pi-build"
   export PRESERVED_DSR_AUDIT="$PRESERVED_DSR_LANE/PRESERVATION_LANE_AUDIT.md"
   test -x "$PRESERVED_DSR_WRAPPER" && test ! -L "$PRESERVED_DSR_WRAPPER"
   test -f "$PRESERVED_DSR_AUDIT" && test ! -L "$PRESERVED_DSR_AUDIT"
   test "$(stat -c '%a' "$PRESERVED_DSR_WRAPPER")" = 700
   test "$(stat -c '%a' "$PRESERVED_DSR_AUDIT")" = 400
   test "$(sha256sum "$PRESERVED_DSR_WRAPPER" | awk '{print $1}')" = \
     7c1c3528229f89eadea62d72eb692b4a5f089e037e008c153544c35701f93f75
   test "$(sha256sum "$PRESERVED_DSR_AUDIT" | awk '{print $1}')" = \
     308b9ce092b34bac3224a91390452721475a9cb96a9ba9b4a164fcc2666662dc
   test "$(sha256sum "$PRESERVED_DSR_LANE/preservation-manifest.sha256" \
     | awk '{print $1}')" = \
     d040d967dbf63644a29d72068aa6ac35e5ff74a7e168cb5eda08a46ff828f32b
   (cd "$PRESERVED_DSR_LANE" && \
     sha256sum --check --status preservation-manifest.sha256)
   preserved_inputs="$MANUAL_RELEASE_STATE_DIR/preserved-lane-inputs.sha256"
   test ! -e "$preserved_inputs"
   (set -C; sha256sum \
     "$PRESERVED_DSR_WRAPPER" \
     "$PRESERVED_DSR_AUDIT" \
     "$PRESERVED_DSR_LANE/preservation-manifest.sha256" \
     > "$preserved_inputs")

   export DSR_BUILD_RUN_ID="$(uuidgen | tr '[:upper:]' '[:lower:]')"
   [[ "$DSR_BUILD_RUN_ID" =~ ^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$ ]]
   export PRESERVED_DSR_STATE_DIR="/data/tmp/pi-v0.2.0-dsr-state-$DSR_BUILD_RUN_ID"
   export RAW_RELEASE_DIR="/data/tmp/pi-v0.2.0-raw-assets-$DSR_BUILD_RUN_ID"
   build_receipt="$MANUAL_RELEASE_STATE_DIR/preserved-build-$DSR_BUILD_RUN_ID.json"
   test ! -e "$PRESERVED_DSR_STATE_DIR" && test ! -L "$PRESERVED_DSR_STATE_DIR"
   test ! -e "$RAW_RELEASE_DIR" && test ! -L "$RAW_RELEASE_DIR"
   test ! -e "$build_receipt"
   (
     set -C
     "$PRESERVED_DSR_WRAPPER" \
       --run-id "$DSR_BUILD_RUN_ID" \
       --state-dir "$PRESERVED_DSR_STATE_DIR" \
       --output-dir "$RAW_RELEASE_DIR" -- \
       build pi --version 0.2.0 \
       --targets linux/amd64,linux/arm64,darwin/amd64,darwin/arm64,windows/amd64 \
       --only-native --jobs 1 > "$build_receipt"
   )

   raw_manifest="$RAW_RELEASE_DIR/pi-v0.2.0-manifest.json"
   jq -e \
     --arg output "$RAW_RELEASE_DIR" \
     --arg manifest "$raw_manifest" '
     .command == "build" and .status == "success" and .exit_code == 0 and
     .details.tool == "pi" and .details.version == "0.2.0" and
     .details.total == 5 and .details.success == 5 and .details.failed == 0 and
     .details.output_dir == $output and .details.manifest == $manifest and
     .details.targets == [
       "linux/amd64", "linux/arm64", "darwin/amd64", "darwin/arm64",
       "windows/amd64"
     ]
   ' "$build_receipt" >/dev/null

   RAW_EXPECTED=(
     pi_linux_amd64
     pi_linux_arm64
     pi_darwin_amd64
     pi_darwin_arm64
     pi_windows_amd64.exe
     pi-v0.2.0-manifest.json
   )
   expected_raw="$(printf '%s\n' "${RAW_EXPECTED[@]}" | LC_ALL=C sort)"
   actual_raw="$(find "$RAW_RELEASE_DIR" -mindepth 1 -maxdepth 1 \
     -printf '%f\n' | LC_ALL=C sort)"
   test "$actual_raw" = "$expected_raw"
   for raw_name in "${RAW_EXPECTED[@]}"; do
     test -f "$RAW_RELEASE_DIR/$raw_name"
     test ! -L "$RAW_RELEASE_DIR/$raw_name"
     test -s "$RAW_RELEASE_DIR/$raw_name"
   done

   jq -e \
     --arg tag "$RELEASE_TAG" \
     --arg commit "$source_commit" \
     --arg run "$DSR_BUILD_RUN_ID" '
     .schema_version == "1.0.0" and .tool == "pi" and .version == $tag and
     .run_id == $run and .source.git_sha == $commit and
     .source.git_ref == $tag and (.source.dependencies | type) == "array" and
     .status == "success" and
     .summary == {total: 5, success: 5, failed: 0} and
     (.build_environments | length) == 5 and
     all(.build_environments[];
       .method == "native" and (.host | type) == "string" and
       (.host | length) > 0 and (.build_influence_env | type) == "object" and
       (.cargo_isolation | type) == "object") and
     ([.build_environments[].target] | sort) == [
       "darwin/amd64", "darwin/arm64", "linux/amd64", "linux/arm64",
       "windows/amd64"
     ] and
     (.artifacts | length) == 5 and
     ([.artifacts[] | {target, name}] | sort_by(.target)) == ([
       {target: "linux/amd64", name: "pi_linux_amd64"},
       {target: "linux/arm64", name: "pi_linux_arm64"},
       {target: "darwin/amd64", name: "pi_darwin_amd64"},
       {target: "darwin/arm64", name: "pi_darwin_arm64"},
       {target: "windows/amd64", name: "pi_windows_amd64.exe"}
     ] | sort_by(.target)) and
     all(.artifacts[];
       (.sha256 | test("^[0-9a-f]{64}$")) and
       (.size_bytes | type) == "number" and .size_bytes > 0 and
       .size_bytes < 23068672 and .archive_format == "binary" and
       .signed == false and .signature_file == "")
   ' "$raw_manifest" >/dev/null
   while IFS=$'\t' read -r raw_name expected_sha expected_size; do
     raw_path="$RAW_RELEASE_DIR/$raw_name"
     test "$(sha256sum "$raw_path" | awk '{print $1}')" = "$expected_sha"
     test "$(wc -c < "$raw_path" | tr -d '[:space:]')" = "$expected_size"
   done < <(jq -r '.artifacts[] | [.name, .sha256, .size_bytes] | @tsv' \
     "$raw_manifest")
   ```

   The aggregate manifest proves the source/tag binding, exact 5/5 target set,
   DSR native-host method, raw byte digests/sizes, build-influence environment
   receipts, per-run isolated source roots, and executable format/architecture
   checks. It does **not** contain `rustc -Vv` compiler identity and does not
   prove that each binary has already executed successfully on its target OS.
   Do not manufacture either claim in the public manifests; target-platform
   runtime smoke tests remain required in step 10.

6. Package the five retained raw binaries in a separate controller-side stage.
   This stage reads the frozen source blobs and the preserved aggregate
   manifest, but never runs DSR or Cargo. It uses the tagged commit timestamp as
   `SOURCE_DATE_EPOCH`, fixed archive member ordering/ownership/modes, USTAR+xz,
   ZIP deflate level 9, and stable sorted-key JSON serialization. For fixed
   source, raw binaries, aggregate manifest, and Python/compression runtime, its
   output bytes are deterministic.

   The public per-target schema is deliberately
   `pi.release.dsr_build_manifest.v1`, not the automated lane's
   `pi.release.build_manifest.v1`: the latter requires compiler identity that
   this preserved build receipt does not record. Each manual manifest instead
   binds its raw artifact and build-environment receipt to the aggregate DSR
   manifest, exact source blobs, locked registry dependency provenance, final
   archive, and archived binary.

   ```bash
   set -euo pipefail
   test "$(git rev-parse 'HEAD^{commit}')" = "$source_commit"
   test "$(git rev-parse "refs/tags/$RELEASE_TAG^{commit}")" = "$source_commit"
   test -z "$(git status --porcelain=v2 --untracked-files=all)"
   test -f "$raw_manifest" && test ! -L "$raw_manifest"
   export RELEASE_ARTIFACT_DIR="$MANUAL_RELEASE_STATE_DIR/artifacts"
   packaging_receipt="$MANUAL_RELEASE_STATE_DIR/deterministic-packaging.json"
   test ! -e "$RELEASE_ARTIFACT_DIR" && test ! -L "$RELEASE_ARTIFACT_DIR"
   test ! -e "$packaging_receipt"
   mkdir -m 700 "$RELEASE_ARTIFACT_DIR"
   (
     set -C
     RELEASE_ROOT="$(git rev-parse --show-toplevel)" \
     SOURCE_COMMIT="$source_commit" \
     RELEASE_TAG="$RELEASE_TAG" \
     RELEASE_VERSION="$RELEASE_VERSION" \
     RAW_RELEASE_DIR="$RAW_RELEASE_DIR" \
     RAW_MANIFEST="$raw_manifest" \
     DSR_BUILD_RUN_ID="$DSR_BUILD_RUN_ID" \
     RELEASE_ARTIFACT_DIR="$RELEASE_ARTIFACT_DIR" \
     PRESERVED_WRAPPER_SHA256="7c1c3528229f89eadea62d72eb692b4a5f089e037e008c153544c35701f93f75" \
     PRESERVED_AUDIT_SHA256="308b9ce092b34bac3224a91390452721475a9cb96a9ba9b4a164fcc2666662dc" \
     PRESERVATION_MANIFEST_SHA256="d040d967dbf63644a29d72068aa6ac35e5ff74a7e168cb5eda08a46ff828f32b" \
     python3 - > "$packaging_receipt" <<'PY'
   import hashlib
   import io
   import json
   import os
   import re
   import stat
   import struct
   import subprocess
   import tarfile
   import tomllib
   import zipfile
   from datetime import datetime, timezone
   from pathlib import Path

   def fail(message):
       raise SystemExit(message)

   def strict_object(pairs):
       result = {}
       for key, value in pairs:
           if key in result:
               fail(f"duplicate JSON key: {key!r}")
           result[key] = value
       return result

   def strict_json(path):
       try:
           return json.loads(
               path.read_text(encoding="utf-8"), object_pairs_hook=strict_object
           )
       except (OSError, UnicodeError, json.JSONDecodeError) as error:
           fail(f"invalid JSON {path}: {error}")

   def git(root, *arguments):
       process = subprocess.run(
           ["git", "-C", str(root), *arguments],
           check=False,
           capture_output=True,
           text=True,
       )
       if process.returncode != 0:
           fail(f"git {' '.join(arguments)} failed: {process.stderr.strip()}")
       return process.stdout.strip()

   def sha256_bytes(data):
       return hashlib.sha256(data).hexdigest()

   def digest(path):
       data = path.read_bytes()
       return {"name": path.name, "sha256": sha256_bytes(data), "size": len(data)}

   def exclusive_write(path, data, mode):
       with path.open("xb") as output:
           output.write(data)
       path.chmod(mode)

   def validate_binary(data, triple):
       if triple.endswith("linux-gnu"):
           if len(data) < 20 or data[:5] != b"\x7fELF\x02" or data[5] != 1:
               fail(f"{triple} is not a 64-bit little-endian ELF image")
           machine = 0x3E if triple.startswith("x86_64") else 0xB7
           if struct.unpack_from("<H", data, 18)[0] != machine:
               fail(f"{triple} ELF machine mismatch")
       elif triple.endswith("apple-darwin"):
           if len(data) < 8 or data[:4] != b"\xcf\xfa\xed\xfe":
               fail(f"{triple} is not a little-endian Mach-O 64 image")
           cpu = 0x01000007 if triple.startswith("x86_64") else 0x0100000C
           if struct.unpack_from("<I", data, 4)[0] != cpu:
               fail(f"{triple} Mach-O CPU mismatch")
       elif triple == "x86_64-pc-windows-msvc":
           if len(data) < 64 or data[:2] != b"MZ":
               fail("Windows binary has no DOS/PE header")
           offset = struct.unpack_from("<I", data, 0x3C)[0]
           if offset + 6 > len(data) or data[offset:offset + 4] != b"PE\0\0":
               fail("Windows binary has an invalid PE header")
           if struct.unpack_from("<H", data, offset + 4)[0] != 0x8664:
               fail("Windows binary is not x86_64")
       else:
           fail(f"unsupported target triple: {triple}")

   def verify_archive(path, archive_root, binary_name, binary_bytes, license_bytes,
                      readme_bytes, source_epoch, zip_timestamp):
       expected = {
           f"{archive_root}/{binary_name}": (binary_bytes, 0o755),
           f"{archive_root}/LICENSE": (license_bytes, 0o644),
           f"{archive_root}/README.md": (readme_bytes, 0o644),
       }
       if path.suffix == ".zip":
           with zipfile.ZipFile(path) as archive:
               infos = archive.infolist()
               names = [info.filename.rstrip("/") for info in infos]
               if len(names) != len(set(names)) or set(names) != set(expected):
                   fail(f"ZIP inventory differs: {path}")
               for info, name in zip(infos, names, strict=True):
                   mode = info.external_attr >> 16
                   if info.is_dir() or info.flag_bits & 0x1 or stat.S_ISLNK(mode):
                       fail(f"ZIP contains an unsafe entry: {info.filename!r}")
                   if info.date_time != zip_timestamp or mode & 0o777 != expected[name][1]:
                       fail(f"ZIP member metadata differs: {info.filename!r}")
                   if archive.read(info) != expected[name][0]:
                       fail(f"ZIP member bytes differ: {info.filename!r}")
           return
       with tarfile.open(path, mode="r:xz") as archive:
           members = archive.getmembers()
           names = [member.name.rstrip("/") for member in members]
           expected_names = {archive_root, *expected}
           if len(names) != len(set(names)) or set(names) != expected_names:
               fail(f"tar inventory differs: {path}")
           for member, name in zip(members, names, strict=True):
               if name == archive_root:
                   if not member.isdir() or member.mode != 0o755:
                       fail(f"archive root is not a directory: {path}")
               elif not member.isreg() or member.issym() or member.islnk():
                   fail(f"tar contains an unsafe entry: {member.name!r}")
               else:
                   extracted = archive.extractfile(member)
                   if extracted is None or extracted.read() != expected[name][0]:
                       fail(f"tar member bytes differ: {member.name!r}")
                   if member.mode != expected[name][1]:
                       fail(f"tar member mode differs: {member.name!r}")
               if member.uid != 0 or member.gid != 0 \
                       or member.uname != "" or member.gname != "" \
                       or member.mtime != source_epoch:
                   fail(f"tar member metadata differs: {member.name!r}")

   root = Path(os.environ["RELEASE_ROOT"])
   commit = os.environ["SOURCE_COMMIT"]
   tag = os.environ["RELEASE_TAG"]
   version = os.environ["RELEASE_VERSION"]
   run_id = os.environ["DSR_BUILD_RUN_ID"]
   raw_dir = Path(os.environ["RAW_RELEASE_DIR"])
   raw_manifest_path = Path(os.environ["RAW_MANIFEST"])
   output_dir = Path(os.environ["RELEASE_ARTIFACT_DIR"])
   if re.fullmatch(r"[0-9a-f]{40}", commit) is None:
       fail("source commit is not a full SHA-1")
   if re.fullmatch(
       r"[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-"
       r"[89ab][0-9a-f]{3}-[0-9a-f]{12}",
       run_id,
   ) is None:
       fail("DSR run ID has an unexpected shape")
   if git(root, "rev-parse", "HEAD^{commit}") != commit:
       fail("HEAD differs from frozen source")
   if git(root, "rev-parse", f"refs/tags/{tag}^{{commit}}") != commit:
       fail("annotated tag differs from frozen source")
   if git(root, "cat-file", "-t", f"refs/tags/{tag}") != "tag":
       fail("release tag is not annotated")
   if git(root, "status", "--porcelain=v2", "--untracked-files=all"):
       fail("release checkout is dirty")
   if not output_dir.is_dir() or output_dir.is_symlink() or any(output_dir.iterdir()):
       fail("public artifact directory must be a fresh empty plain directory")

   support_paths = {
       "cargo_toml": "Cargo.toml",
       "cargo_lock": "Cargo.lock",
       "rust_toolchain": "rust-toolchain.toml",
       "license": "LICENSE",
       "readme": "README.md",
       "install": "install.sh",
       "dropin_verdict": "docs/evidence/dropin-certification-verdict.json",
       "models_generated_ts":
           "legacy_pi_mono_code/pi-mono/packages/ai/src/models.generated.ts",
   }
   source_blobs = {}
   for label, relative in support_paths.items():
       path = root / relative
       if path.is_symlink() or not path.is_file():
           fail(f"frozen source input is missing/non-regular: {relative}")
       blob = git(root, "rev-parse", f"{commit}:{relative}")
       tree_fields = git(root, "ls-tree", commit, "--", relative).split(maxsplit=3)
       expected_mode = "100755" if relative == "install.sh" else "100644"
       if len(tree_fields) != 4 or tree_fields[0] != expected_mode \
               or tree_fields[1] != "blob" or tree_fields[2] != blob \
               or tree_fields[3] != relative:
           fail(f"frozen source mode/type differs: {relative}")
       if git(root, "hash-object", "--no-filters", "--", relative) != blob:
           fail(f"worktree bytes differ from frozen blob: {relative}")
       source_blobs[label] = blob

   cargo = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))
   if cargo["package"]["version"] != version or tag != f"v{version}":
       fail("Cargo version, release version, and tag differ")
   lock = tomllib.loads((root / "Cargo.lock").read_text(encoding="utf-8"))
   registry = "registry+https://github.com/rust-lang/crates.io-index"
   selected = []
   for package in lock["package"]:
       name = package["name"]
       if not (
           name in {"asupersync", "rich_rust"}
           or name.startswith("charmed-")
           or name.startswith("sqlmodel-")
       ):
           continue
       checksum = package.get("checksum")
       if package.get("source") != registry or not isinstance(checksum, str) \
               or re.fullmatch(r"[0-9a-f]{64}", checksum) is None:
           fail(f"invalid locked registry provenance for {name}")
       selected.append({
           "name": name,
           "version": package["version"],
           "source": registry,
           "checksum": checksum,
       })
   selected.sort(key=lambda item: (item["name"], item["version"]))
   identities = [(item["name"], item["version"]) for item in selected]
   required = {"asupersync", "rich_rust", "sqlmodel-core", "sqlmodel-sqlite"}
   if len(identities) != len(set(identities)) \
           or not required.issubset({name for name, _ in identities}):
       fail("locked release dependency selection is duplicate or incomplete")

   specs = {
       "linux/amd64": {
           "raw": "pi_linux_amd64", "asset": "pi-linux-amd64",
           "triple": "x86_64-unknown-linux-gnu", "runner_os": "Linux",
           "format": "tar.xz", "binary": "pi",
       },
       "linux/arm64": {
           "raw": "pi_linux_arm64", "asset": "pi-linux-arm64",
           "triple": "aarch64-unknown-linux-gnu", "runner_os": "Linux",
           "format": "tar.xz", "binary": "pi",
       },
       "darwin/amd64": {
           "raw": "pi_darwin_amd64", "asset": "pi-darwin-amd64",
           "triple": "x86_64-apple-darwin", "runner_os": "macOS",
           "format": "tar.xz", "binary": "pi",
       },
       "darwin/arm64": {
           "raw": "pi_darwin_arm64", "asset": "pi-darwin-arm64",
           "triple": "aarch64-apple-darwin", "runner_os": "macOS",
           "format": "tar.xz", "binary": "pi",
       },
       "windows/amd64": {
           "raw": "pi_windows_amd64.exe", "asset": "pi-windows-amd64",
           "triple": "x86_64-pc-windows-msvc", "runner_os": "Windows",
           "format": "zip", "binary": "pi.exe",
       },
   }
   expected_raw = {item["raw"] for item in specs.values()} | {
       f"pi-{tag}-manifest.json"
   }
   raw_entries = list(raw_dir.iterdir()) if raw_dir.is_dir() and not raw_dir.is_symlink() else []
   if len(raw_entries) != len(expected_raw) \
           or {entry.name for entry in raw_entries} != expected_raw:
       fail("raw DSR inventory is not exactly five binaries plus one manifest")
   if any(entry.is_symlink() or not entry.is_file() or entry.stat().st_size == 0
          for entry in raw_entries):
       fail("raw DSR inventory contains an invalid entry")

   if raw_manifest_path != raw_dir / f"pi-{tag}-manifest.json":
       fail("aggregate manifest path is outside the exact raw inventory")
   raw_manifest_bytes = raw_manifest_path.read_bytes()
   raw_manifest = strict_json(raw_manifest_path)
   expected_manifest_keys = {
       "schema_version", "tool", "version", "run_id", "source", "built_at",
       "duration_ms", "status", "summary", "build_environments", "artifacts",
   }
   if not isinstance(raw_manifest, dict) or set(raw_manifest) != expected_manifest_keys:
       fail("aggregate DSR manifest schema changed")
   if raw_manifest.get("schema_version") != "1.0.0" \
           or raw_manifest.get("tool") != "pi" \
           or raw_manifest.get("version") != tag \
           or raw_manifest.get("run_id") != run_id \
           or raw_manifest.get("status") != "success" \
           or raw_manifest.get("summary") != {"total": 5, "success": 5, "failed": 0} \
           or raw_manifest.get("source", {}).get("git_sha") != commit \
           or raw_manifest.get("source", {}).get("git_ref") != tag:
       fail("aggregate DSR manifest is not bound to this exact successful run")
   artifacts = raw_manifest.get("artifacts")
   environments = raw_manifest.get("build_environments")
   if not isinstance(artifacts, list) or len(artifacts) != 5 \
           or not isinstance(environments, list) or len(environments) != 5:
       fail("aggregate DSR manifest does not contain exact five-target receipts")
   artifacts_by_target = {item.get("target"): item for item in artifacts}
   environments_by_target = {item.get("target"): item for item in environments}
   if set(artifacts_by_target) != set(specs) or set(environments_by_target) != set(specs):
       fail("aggregate DSR manifest target set differs")
   if len(artifacts_by_target) != len(artifacts) \
           or len(environments_by_target) != len(environments):
       fail("aggregate DSR manifest contains duplicate targets")

   source_epoch = int(git(root, "show", "-s", "--format=%ct", commit))
   zip_time = datetime.fromtimestamp(source_epoch, tz=timezone.utc)
   if not 1980 <= zip_time.year <= 2107:
       fail("commit timestamp cannot be represented safely in ZIP")
   zip_timestamp = (
       zip_time.year, zip_time.month, zip_time.day,
       zip_time.hour, zip_time.minute, zip_time.second - zip_time.second % 2,
   )
   license_bytes = (root / "LICENSE").read_bytes()
   readme_bytes = (root / "README.md").read_bytes()
   aggregate_sha = sha256_bytes(raw_manifest_bytes)
   generated = []

   def tar_info(name, mode, size=0, directory=False):
       info = tarfile.TarInfo(name=name)
       info.type = tarfile.DIRTYPE if directory else tarfile.REGTYPE
       info.mode = mode
       info.uid = 0
       info.gid = 0
       info.uname = ""
       info.gname = ""
       info.mtime = source_epoch
       info.size = size
       return info

   def zip_info(name, mode):
       info = zipfile.ZipInfo(filename=name, date_time=zip_timestamp)
       info.create_system = 3
       info.compress_type = zipfile.ZIP_DEFLATED
       info.external_attr = (stat.S_IFREG | mode) << 16
       return info

   for dsr_target, spec in specs.items():
       raw_path = raw_dir / spec["raw"]
       raw_bytes = raw_path.read_bytes()
       raw_receipt = artifacts_by_target[dsr_target]
       environment = environments_by_target[dsr_target]
       if raw_receipt != {
           "name": spec["raw"],
           "target": dsr_target,
           "sha256": sha256_bytes(raw_bytes),
           "size_bytes": len(raw_bytes),
           "archive_format": "binary",
           "signed": False,
           "signature_file": "",
       }:
           fail(f"aggregate raw receipt differs for {dsr_target}")
       if len(raw_bytes) >= 22 * 1024 * 1024:
           fail(f"raw binary violates <22 MiB budget: {dsr_target}")
       if environment.get("target") != dsr_target \
               or environment.get("method") != "native" \
               or not isinstance(environment.get("host"), str) \
               or not environment["host"]:
           fail(f"invalid DSR build-environment receipt: {dsr_target}")
       validate_binary(raw_bytes, spec["triple"])

       archive_root = f"pi-{version}-{spec['triple']}"
       suffix = ".zip" if spec["format"] == "zip" else ".tar.xz"
       archive_path = output_dir / f"{spec['asset']}{suffix}"
       if archive_path.exists() or archive_path.is_symlink():
           fail(f"refusing to clobber {archive_path}")
       members = [
           (f"{archive_root}/{spec['binary']}", raw_bytes, 0o755),
           (f"{archive_root}/LICENSE", license_bytes, 0o644),
           (f"{archive_root}/README.md", readme_bytes, 0o644),
       ]
       with archive_path.open("xb") as output:
           if spec["format"] == "zip":
               with zipfile.ZipFile(
                   output, mode="w", compression=zipfile.ZIP_DEFLATED,
                   compresslevel=9, strict_timestamps=True,
               ) as archive:
                   for name, data, mode in members:
                       archive.writestr(
                           zip_info(name, mode), data,
                           compress_type=zipfile.ZIP_DEFLATED, compresslevel=9,
                       )
           else:
               with tarfile.open(
                   fileobj=output, mode="w:xz", format=tarfile.USTAR_FORMAT,
                   preset=9,
               ) as archive:
                   archive.addfile(tar_info(archive_root, 0o755, directory=True))
                   for name, data, mode in members:
                       archive.addfile(tar_info(name, mode, len(data)), io.BytesIO(data))
       archive_path.chmod(0o600)
       verify_archive(
           archive_path, archive_root, spec["binary"], raw_bytes,
           license_bytes, readme_bytes, source_epoch, zip_timestamp,
       )

       environment_bytes = json.dumps(
           environment, sort_keys=True, separators=(",", ":"), ensure_ascii=False
       ).encode("utf-8")
       manifest = {
           "schema": "pi.release.dsr_build_manifest.v1",
           "tag": tag,
           "version": version,
           "target": spec["triple"],
           "dsr_target": dsr_target,
           "asset": spec["asset"],
           "runner_os": spec["runner_os"],
           "pi_agent_rust": commit,
           "source_blobs": source_blobs,
           "selected_locked_registry_packages": selected,
           "raw_build": {
               "run_id": run_id,
               "aggregate_manifest": {
                   "name": raw_manifest_path.name,
                   "schema_version": "1.0.0",
                   "sha256": aggregate_sha,
               },
               "raw_binary": {
                   "name": spec["raw"],
                   "sha256": sha256_bytes(raw_bytes),
                   "size": len(raw_bytes),
               },
               "build_environment": {
                   "host": environment["host"],
                   "method": environment["method"],
                   "receipt_sha256": sha256_bytes(environment_bytes),
               },
               "preservation_lane": {
                   "wrapper_sha256": os.environ["PRESERVED_WRAPPER_SHA256"],
                   "audit_sha256": os.environ["PRESERVED_AUDIT_SHA256"],
                   "manifest_sha256": os.environ["PRESERVATION_MANIFEST_SHA256"],
               },
           },
           "packaging": {
               "source_date_epoch": source_epoch,
               "archive_root": archive_root,
               "format": spec["format"],
               "metadata_policy": "fixed-order-uid0-gid0-source-epoch-v1",
           },
           "archive": digest(archive_path),
           "binary": {
               "name": spec["binary"],
               "sha256": sha256_bytes(raw_bytes),
               "size": len(raw_bytes),
           },
       }
       manifest_path = output_dir / f"build-manifest-{spec['asset']}.json"
       manifest_bytes = (
           json.dumps(manifest, indent=2, sort_keys=True, ensure_ascii=False) + "\n"
       ).encode("utf-8")
       exclusive_write(manifest_path, manifest_bytes, 0o600)
       generated.extend([archive_path.name, manifest_path.name])

   install_path = output_dir / "install.sh"
   exclusive_write(install_path, (root / "install.sh").read_bytes(), 0o700)
   generated.append(install_path.name)
   if len(generated) != 11 or len(set(generated)) != 11:
       fail("packaging stage did not create exactly eleven pre-checksum assets")
   checksum_path = output_dir / "SHA256SUMS"
   checksum_lines = []
   for name in sorted(generated):
       checksum_lines.append(f"{digest(output_dir / name)['sha256']}  {name}\n")
   exclusive_write(checksum_path, "".join(checksum_lines).encode("utf-8"), 0o600)
   if len(checksum_lines) != 11:
       fail("SHA256SUMS must contain exactly eleven lines")

   expected_public = set(generated) | {"SHA256SUMS"}
   public_entries = list(output_dir.iterdir())
   if len(public_entries) != 12 \
           or {entry.name for entry in public_entries} != expected_public:
       fail("public release inventory is not exactly twelve assets")
   if any(entry.is_symlink() or not entry.is_file() or entry.stat().st_size == 0
          for entry in public_entries):
       fail("public release inventory contains an invalid entry")
   receipt = {
       "schema": "pi.release.deterministic_packaging_receipt.v1",
       "tag": tag,
       "source_commit": commit,
       "source_date_epoch": source_epoch,
       "raw_manifest_sha256": aggregate_sha,
       "assets": [digest(output_dir / name) for name in sorted(expected_public)],
   }
   print(json.dumps(receipt, indent=2, sort_keys=True))
   PY
   )

   EXPECTED_ASSETS=(
     pi-linux-amd64.tar.xz
     pi-linux-arm64.tar.xz
     pi-darwin-amd64.tar.xz
     pi-darwin-arm64.tar.xz
     pi-windows-amd64.zip
     install.sh
     SHA256SUMS
     build-manifest-pi-linux-amd64.json
     build-manifest-pi-linux-arm64.json
     build-manifest-pi-darwin-amd64.json
     build-manifest-pi-darwin-arm64.json
     build-manifest-pi-windows-amd64.json
   )
   expected_assets="$(printf '%s\n' "${EXPECTED_ASSETS[@]}" | LC_ALL=C sort)"
   actual_assets="$(find "$RELEASE_ARTIFACT_DIR" -mindepth 1 -maxdepth 1 \
     -printf '%f\n' | LC_ALL=C sort)"
   test "$actual_assets" = "$expected_assets"
   test "$(printf '%s\n' "$actual_assets" | wc -l | tr -d '[:space:]')" = 12
   for asset in "${EXPECTED_ASSETS[@]}"; do
     test -f "$RELEASE_ARTIFACT_DIR/$asset"
     test ! -L "$RELEASE_ARTIFACT_DIR/$asset"
     test -s "$RELEASE_ARTIFACT_DIR/$asset"
   done

   aggregate_sha256="$(sha256sum "$raw_manifest" | awk '{print $1}')"
   (
     cd "$RELEASE_ARTIFACT_DIR"
     test "$(wc -l < SHA256SUMS | tr -d '[:space:]')" = 11
     checksum_names="$(sed -E 's/^[0-9a-f]{64}  //' SHA256SUMS)"
     expected_checksum_names="$(printf '%s\n' "${EXPECTED_ASSETS[@]}" \
       | grep -v '^SHA256SUMS$' | LC_ALL=C sort)"
     test "$checksum_names" = "$expected_checksum_names"
     sha256sum --check --strict SHA256SUMS
     set -- build-manifest-pi-*.json
     test "$#" = 5
     for manifest in "$@"; do
       jq -e \
         --arg tag "$RELEASE_TAG" \
         --arg version "$RELEASE_VERSION" \
         --arg commit "$source_commit" \
         --arg run "$DSR_BUILD_RUN_ID" \
         --arg aggregate "$aggregate_sha256" '
         .schema == "pi.release.dsr_build_manifest.v1" and
         .tag == $tag and .version == $version and
         .pi_agent_rust == $commit and .raw_build.run_id == $run and
         .raw_build.aggregate_manifest.sha256 == $aggregate and
         .raw_build.aggregate_manifest.schema_version == "1.0.0" and
         .raw_build.build_environment.method == "native" and
         (.raw_build.build_environment.receipt_sha256 |
           test("^[0-9a-f]{64}$")) and
         (has("rustc") | not) and
         (.archive.sha256 | test("^[0-9a-f]{64}$")) and
         (.archive.size | type) == "number" and .archive.size > 0 and
         (.binary.sha256 | test("^[0-9a-f]{64}$")) and
         (.binary.size | type) == "number" and
         .binary.size > 0 and .binary.size < 23068672
       ' "$manifest" >/dev/null
     done
   )
   jq -e \
     --arg tag "$RELEASE_TAG" \
     --arg commit "$source_commit" \
     --arg aggregate "$aggregate_sha256" '
     .schema == "pi.release.deterministic_packaging_receipt.v1" and
     .tag == $tag and .source_commit == $commit and
     .raw_manifest_sha256 == $aggregate and
     (.assets | length) == 12 and
     ([.assets[].name] | length) == ([.assets[].name] | unique | length)
   ' "$packaging_receipt" >/dev/null
   receipt_assets="$(jq -r '.assets[].name' "$packaging_receipt" | LC_ALL=C sort)"
   test "$receipt_assets" = "$expected_assets"
   while IFS=$'\t' read -r asset expected_sha expected_size; do
     test "$(sha256sum "$RELEASE_ARTIFACT_DIR/$asset" | awk '{print $1}')" = \
       "$expected_sha"
     test "$(wc -c < "$RELEASE_ARTIFACT_DIR/$asset" | tr -d '[:space:]')" = \
       "$expected_size"
   done < <(jq -r '.assets[] | [.name, .sha256, .size] | @tsv' \
     "$packaging_receipt")
   printf 'raw_manifest_sha256=%s\npackaging_receipt_sha256=%s\n' \
     "$aggregate_sha256" \
     "$(sha256sum "$packaging_receipt" | awk '{print $1}')" >> "$proof_file"
   ```

   Finally, re-check the immutable-tag rule and remote branch tips, then push
   the already-created annotated tag. Do not recreate or move it after the raw
   build or packaging stage:

   ```bash
   set -euo pipefail
   test "$(git rev-parse 'HEAD^{commit}')" = "$source_commit"
   test "$(git cat-file -t "refs/tags/$RELEASE_TAG")" = tag
   test "$(git rev-parse "refs/tags/$RELEASE_TAG^{commit}")" = "$source_commit"
   test -z "$(git status --porcelain=v2 --untracked-files=all)"
   immutable_ruleset_id="$(jq -er 'first(.[] |
     select(.target == "tag" and .enforcement == "active" and
       ((.conditions.ref_name.include | index("refs/tags/v*")) != null or
        (.conditions.ref_name.include | index("~ALL")) != null) and
       .conditions.ref_name.exclude == [] and
       ([.rules[].type] | index("update")) != null and
       ([.rules[].type] | index("deletion")) != null and
       (.bypass_actors | type) == "array" and .bypass_actors == [])) | .id' \
     "$ruleset_details")"
   pretag_ruleset="$MANUAL_RELEASE_STATE_DIR/pre-tag-ruleset.json"
   test ! -e "$pretag_ruleset"
   gh api -H 'Accept: application/vnd.github+json' \
     "/repos/${RELEASE_REPOSITORY}/rulesets/${immutable_ruleset_id}?includes_parents=true" \
     > "$pretag_ruleset"
   jq -e '
     .target == "tag" and .enforcement == "active" and
     ((.conditions.ref_name.include | index("refs/tags/v*")) != null or
      (.conditions.ref_name.include | index("~ALL")) != null) and
     .conditions.ref_name.exclude == [] and
     ([.rules[].type] | index("update")) != null and
     ([.rules[].type] | index("deletion")) != null and
     (.bypass_actors | type) == "array" and .bypass_actors == []
   ' "$pretag_ruleset" >/dev/null
   git fetch --no-tags origin \
     refs/heads/main:refs/remotes/origin/main \
     refs/heads/master:refs/remotes/origin/master
   test "$(git rev-parse 'origin/main^{commit}')" = "$source_commit"
   test "$(git rev-parse 'origin/master^{commit}')" = "$source_commit"
   test -z "$(git ls-remote --tags origin \
     "refs/tags/$RELEASE_TAG" "refs/tags/$RELEASE_TAG^{}")"
   git push origin "refs/tags/$RELEASE_TAG:refs/tags/$RELEASE_TAG"
   remote_tag_commit="$(git ls-remote --tags origin \
     "refs/tags/$RELEASE_TAG^{}" | awk 'NR == 1 {print $1}')"
   test "$remote_tag_commit" = "$source_commit"
   posttag_workflows="$MANUAL_RELEASE_STATE_DIR/github-actions-after-tag.json"
   test ! -e "$posttag_workflows"
   gh api -H 'Accept: application/vnd.github+json' \
     "/repos/${RELEASE_REPOSITORY}/actions/runs?per_page=1" \
     > "$posttag_workflows"
   jq -e '
     (.total_count | type) == "number" and .total_count >= 0 and
     (.workflow_runs | type) == "array" and (.workflow_runs | length) <= 1 and
     all(.workflow_runs[]; (.id | type) == "number" and .id > 0)
   ' "$posttag_workflows" >/dev/null
   test "$(jq -r '.workflow_runs[0].id // "none"' "$posttag_workflows")" = \
     "$WORKFLOW_BASELINE_ID"
   ```

7. Create the GitHub draft with DSR, without dispatching any workflow, and bind
   it to exact metadata and exact bytes. The release body is a publication
   input: it must state the live `NOT_CERTIFIED` verdict and explicitly forbid
   strict drop-in wording. A historical `CERTIFIED` result must never be copied
   into this current release body. Canonical DSR is draft transport here, not
   the build or packaging authority. Give it a fresh state root so it cannot
   discover a stale alternate build manifest or mutate the exact 12-file public
   inventory with derived sidecars.

   ```bash
   set -euo pipefail
   expected_source_commit="$(awk -F= '$1 == "source_commit" {print $2}' "$proof_file")"
   expected_crate_sha256="$(awk -F= '$1 == "package_sha256" {print $2}' "$proof_file")"
   expected_crate_size="$(awk -F= '$1 == "package_size" {print $2}' "$proof_file")"
   [[ "$expected_source_commit" =~ ^[0-9a-f]{40}$ ]]
   [[ "$expected_crate_sha256" =~ ^[0-9a-f]{64}$ ]]
   [[ "$expected_crate_size" =~ ^[0-9]+$ ]]
   test "$(git rev-parse 'HEAD^{commit}')" = "$expected_source_commit"
   test -z "$(git status --porcelain=v2 --untracked-files=all)"

   verdict_source="$(jq -er '
     select(.schema == "pi.dropin.certification_verdict.v1" and
            .overall_verdict == "NOT_CERTIFIED" and
            (.git_commit | test("^[0-9a-f]{40}$")) and
            (.blocking_reasons | type) == "array" and
            (.blocking_reasons | length) > 0) | .git_commit
   ' docs/evidence/dropin-certification-verdict.json)"
   git merge-base --is-ancestor "$verdict_source" "$expected_source_commit"

   release_body="$MANUAL_RELEASE_STATE_DIR/RELEASE_BODY.md"
   test ! -e "$release_body"
   (set -C; printf '%s\n' \
     "# ${RELEASE_TAG}" \
     "" \
     "Manual DSR release of pi_agent_rust ${RELEASE_VERSION}." \
     "" \
     "### Drop-in certification status" \
     "" \
     "**NOT_CERTIFIED** — This release is not certified as a strict drop-in replacement and must not be described as one." \
     "" \
     "Evidence: https://github.com/Dicklesworthstone/pi_agent_rust/blob/${RELEASE_TAG}/docs/evidence/dropin-certification-verdict.json" \
     "" \
     "All downloadable artifacts are covered by the attached SHA256SUMS file." \
     > "$release_body")
   grep -Fx '**NOT_CERTIFIED** — This release is not certified as a strict drop-in replacement and must not be described as one.' \
     "$release_body" >/dev/null
   sha256sum "$release_body" > "$MANUAL_RELEASE_STATE_DIR/release-body.sha256"

   export DSR_RELEASE_STATE_DIR="$MANUAL_RELEASE_STATE_DIR/dsr-release-state"
   test ! -e "$DSR_RELEASE_STATE_DIR" && test ! -L "$DSR_RELEASE_STATE_DIR"
   mkdir -m 700 "$DSR_RELEASE_STATE_DIR"
   DSR_STATE_DIR="$DSR_RELEASE_STATE_DIR" \
     dsr release pi_agent_rust "$RELEASE_VERSION" --draft --no-dispatch \
       --artifacts "$RELEASE_ARTIFACT_DIR"

   draft_discovered="$MANUAL_RELEASE_STATE_DIR/github-draft-discovered.json"
   test ! -e "$draft_discovered"
   gh api -H 'Accept: application/vnd.github+json' \
     "/repos/${RELEASE_REPOSITORY}/releases/tags/${RELEASE_TAG}" \
     > "$draft_discovered"
   release_id="$(jq -er \
     --arg tag "$RELEASE_TAG" \
     'select((.id | type) == "number" and .id > 0 and
             .draft == true and .tag_name == $tag) | .id' \
     "$draft_discovered")"
   draft_payload="$MANUAL_RELEASE_STATE_DIR/github-draft-bind-payload.json"
   draft_bound="$MANUAL_RELEASE_STATE_DIR/github-draft-bound.json"
   test ! -e "$draft_payload" && test ! -e "$draft_bound"
   jq -n \
     --arg tag "$RELEASE_TAG" \
     --arg commit "$expected_source_commit" \
     --arg title "$RELEASE_TAG" \
     --rawfile body "$release_body" \
     '{tag_name: $tag, target_commitish: $commit, name: $title,
       body: $body, draft: true, prerelease: false}' \
     > "$draft_payload"
   gh api --method PATCH \
     -H 'Accept: application/vnd.github+json' \
     "/repos/${RELEASE_REPOSITORY}/releases/${release_id}" \
     --input "$draft_payload" > "$draft_bound"
   jq -e \
     --argjson id "$release_id" \
     --arg tag "$RELEASE_TAG" \
     --arg commit "$expected_source_commit" \
     --rawfile body "$release_body" \
     '.id == $id and .draft == true and .prerelease == false and
      .tag_name == $tag and .target_commitish == $commit and
      .name == $tag and .body == $body' \
     "$draft_bound" >/dev/null
   printf 'release_id=%s\nrelease_body_sha256=%s\n' \
     "$release_id" "$(sha256sum "$release_body" | awk '{print $1}')" \
     >> "$proof_file"
   ```

   Define one verifier and use it both immediately before and immediately after
   public publication. It binds the database ID, draft/public state, annotated
   tag target, title, body, prerelease flag, exact 12-name inventory, and every
   downloaded byte:

   ```bash
   set -euo pipefail
   EXPECTED_ASSETS=(
     pi-linux-amd64.tar.xz
     pi-linux-arm64.tar.xz
     pi-darwin-amd64.tar.xz
     pi-darwin-arm64.tar.xz
     pi-windows-amd64.zip
     install.sh
     SHA256SUMS
     build-manifest-pi-linux-amd64.json
     build-manifest-pi-linux-arm64.json
     build-manifest-pi-darwin-amd64.json
     build-manifest-pi-darwin-arm64.json
     build-manifest-pi-windows-amd64.json
   )
   expected_assets="$(printf '%s\n' "${EXPECTED_ASSETS[@]}" | LC_ALL=C sort)"
   local_assets="$(find "$RELEASE_ARTIFACT_DIR" -mindepth 1 -maxdepth 1 \
     -printf '%f\n' | LC_ALL=C sort)"
   test "$local_assets" = "$expected_assets"
   test "$(printf '%s\n' "$local_assets" | wc -l | tr -d '[:space:]')" = 12
   for asset in "${EXPECTED_ASSETS[@]}"; do
     test -f "$RELEASE_ARTIFACT_DIR/$asset"
     test ! -L "$RELEASE_ARTIFACT_DIR/$asset"
     test -s "$RELEASE_ARTIFACT_DIR/$asset"
   done

   verify_exact_release() {
     local expected_draft="$1"
     local label="$2"
     local metadata="$MANUAL_RELEASE_STATE_DIR/github-release-${label}.json"
     local download_dir="$MANUAL_RELEASE_STATE_DIR/github-assets-${label}"
     test "$expected_draft" = true || test "$expected_draft" = false
     test ! -e "$metadata" && test ! -e "$download_dir"
     gh api -H 'Accept: application/vnd.github+json' \
       "/repos/${RELEASE_REPOSITORY}/releases/tags/${RELEASE_TAG}" \
       > "$metadata"
     jq -e \
       --argjson id "$release_id" \
       --argjson draft "$expected_draft" \
       --arg tag "$RELEASE_TAG" \
       --arg commit "$expected_source_commit" \
       --rawfile body "$release_body" \
       '.id == $id and .draft == $draft and .prerelease == false and
        .tag_name == $tag and .target_commitish == $commit and
        .name == $tag and .body == $body and
        (.assets | type) == "array" and (.assets | length) == 12 and
        ([.assets[].name] | length) == ([.assets[].name] | unique | length)' \
       "$metadata" >/dev/null
     local remote_assets
     remote_assets="$(jq -r '.assets[].name' "$metadata" | LC_ALL=C sort)"
     test "$remote_assets" = "$expected_assets"
     mkdir -m 700 "$download_dir"
     gh release download "$RELEASE_TAG" --dir "$download_dir"
     for asset in "${EXPECTED_ASSETS[@]}"; do
       cmp "$RELEASE_ARTIFACT_DIR/$asset" "$download_dir/$asset"
     done
     local remote_tag_object remote_tag_commit
     remote_tag_object="$(git ls-remote --tags origin \
       "refs/tags/$RELEASE_TAG" | awk 'NR == 1 {print $1}')"
     remote_tag_commit="$(git ls-remote --tags origin \
       "refs/tags/$RELEASE_TAG^{}" | awk 'NR == 1 {print $1}')"
     [[ "$remote_tag_object" =~ ^[0-9a-f]{40}$ ]]
     test "$remote_tag_object" != "$remote_tag_commit"
     test "$remote_tag_commit" = "$expected_source_commit"
   }

   verify_exact_release true draft-after-bind
   ```

8. On the clean publisher checkout at the exact tagged commit, materialize and
   preserve the checksum-gated Cargo credential provider from the frozen
   release workflow. Do not substitute `cargo:token`. The v0.2.0 reviewed
   workflow and extracted provider hashes below are intentional fail-closed
   pins; a later workflow change requires an explicit review and documentation
   update before this manual lane can publish.

   ```bash
   set -euo pipefail
   test "$(git rev-parse 'HEAD^{commit}')" = "$expected_source_commit"
   test -z "$(git status --porcelain=v2 --untracked-files=all)"
   frozen_workflow="$MANUAL_RELEASE_STATE_DIR/frozen-release-workflow.yml"
   provider="$MANUAL_RELEASE_STATE_DIR/pi-crates-credential-provider.py"
   provider_proof="$MANUAL_RELEASE_STATE_DIR/credential-provider.sha256"
   test ! -e "$frozen_workflow" && test ! -e "$provider" && test ! -e "$provider_proof"
   (set -C; git show \
     "$expected_source_commit:.github/workflows/release.yml" > "$frozen_workflow")
   test "$(sha256sum "$frozen_workflow" | awk '{print $1}')" = \
     20b9d3f8d431ca1ed99e977509f4e4a135146917d1b3ec12997c4a1979ec9aef
   FROZEN_WORKFLOW="$frozen_workflow" PROVIDER_PATH="$provider" python3 - <<'PY'
   import os
   from pathlib import Path

   workflow = Path(os.environ["FROZEN_WORKFLOW"]).read_text(encoding="utf-8")
   start = "          source = r'''"
   end = "          '''\n          Path(os.environ[\"PROVIDER_PATH\"]).write_text(source, encoding=\"utf-8\")"
   if workflow.count(start) != 1 or workflow.count(end) != 1:
       raise SystemExit("frozen workflow does not contain one auditable provider source block")
   raw = workflow.split(start, 1)[1].split(end, 1)[0]
   lines = raw.splitlines(keepends=True)
   if not lines or lines[0] != "#!/usr/bin/env python3\n":
       raise SystemExit("credential provider block has an unexpected header")
   source = lines[0]
   for line in lines[1:]:
       if line.startswith("          "):
           source += line[10:]
       elif line == "\n":
           source += line
       else:
           raise SystemExit("credential provider block has unexpected YAML indentation")
   compile(source, os.environ["PROVIDER_PATH"], "exec")
   with Path(os.environ["PROVIDER_PATH"]).open("x", encoding="utf-8") as output:
       output.write(source)
   PY
   chmod 700 "$provider"
   test -f "$provider" && test ! -L "$provider"
   provider_sha256="$(sha256sum "$provider" | awk '{print $1}')"
   test "$provider_sha256" = \
     3aee4bc78904238aecba0ee6f973caae69027efaf28d5b1d649ddf9ef4aaf903
   (set -C; sha256sum "$frozen_workflow" "$provider" > "$provider_proof")
   ```

   Adversarially self-test allow and deny behavior before any real token is
   read. A successful exact publish request must create the exact receipt;
   wrong checksum, registry, identity, or extra fields must be rejected without
   creating one.

   ```bash
   set -euo pipefail
   PROVIDER_PATH="$provider" \
   SELF_TEST_DIR="$MANUAL_RELEASE_STATE_DIR" \
   PACKAGE_VERSION="$RELEASE_VERSION" \
   CRATE_SHA256="$expected_crate_sha256" python3 - <<'PY'
   import json
   import os
   import subprocess
   from pathlib import Path

   provider = os.environ["PROVIDER_PATH"]
   root = Path(os.environ["SELF_TEST_DIR"])
   official = {"index-url": "sparse+https://index.crates.io/", "name": "crates-io"}
   publish = {
       "v": 1, "kind": "get", "operation": "publish",
       "name": "pi_agent_rust", "vers": os.environ["PACKAGE_VERSION"],
       "cksum": os.environ["CRATE_SHA256"], "registry": official, "args": [],
   }

   def invoke(label, request):
       receipt = root / f"provider-self-test-{label}.json"
       if receipt.exists():
           raise SystemExit(f"self-test path already exists: {receipt}")
       env = {
           **os.environ,
           "PI_CRATES_IO_RELEASE_TOKEN": "self-test-token",
           "PI_EXPECTED_CRATE_NAME": "pi_agent_rust",
           "PI_EXPECTED_CRATE_VERSION": os.environ["PACKAGE_VERSION"],
           "PI_EXPECTED_CRATE_SHA256": os.environ["CRATE_SHA256"],
           "PI_CREDENTIAL_RECEIPT": str(receipt),
       }
       process = subprocess.run(
           [provider, "--cargo-plugin"],
           input=json.dumps(request, separators=(",", ":")) + "\n",
           capture_output=True, text=True, env=env, timeout=10, check=False,
       )
       lines = process.stdout.splitlines()
       if process.returncode != 0 or len(lines) != 2 or json.loads(lines[0]) != {"v": [1]}:
           raise SystemExit(f"credential-provider protocol failure: {label}")
       return json.loads(lines[1]), receipt

   read = {"v": 1, "kind": "get", "operation": "read", "registry": official, "args": []}
   response, receipt = invoke("read", read)
   if response.get("Ok", {}).get("token") != "self-test-token" or receipt.exists():
       raise SystemExit("read allow self-test failed")
   response, receipt = invoke("exact-publish", publish)
   if response.get("Ok") != {
       "kind": "get", "token": "self-test-token", "cache": "never",
       "operation_independent": False,
   } or not receipt.is_file():
       raise SystemExit("exact publish allow self-test failed")
   expected_receipt = {
       "schema": "pi.release.cargo_credential_receipt.v1",
       "name": "pi_agent_rust", "version": os.environ["PACKAGE_VERSION"],
       "crate_sha256": os.environ["CRATE_SHA256"],
       "registry_name": "crates-io", "registry_index_url": official["index-url"],
   }
   if json.loads(receipt.read_text(encoding="utf-8")) != expected_receipt:
       raise SystemExit("exact publish receipt differs")
   denials = {
       "wrong-checksum": {**publish, "cksum": "0" * 64},
       "wrong-name": {**publish, "name": "other"},
       "wrong-version": {**publish, "vers": "999.0.0"},
       "wrong-registry": {**publish, "registry": {**official, "name": "other"}},
       "extra-field": {**publish, "unexpected": True},
   }
   for label, request in denials.items():
       response, receipt = invoke(label, request)
       if "Err" not in response or receipt.exists():
           raise SystemExit(f"credential-provider deny self-test failed: {label}")
   PY
   test "$(sha256sum "$provider" | awk '{print $1}')" = "$provider_sha256"
   ```

   Recreate the package on this isolated publisher path and match the source
   proof before reading the token. Then force both Cargo credential settings to
   the reviewed provider at command-line precedence. The provider releases the
   token only when Cargo itself presents the exact crate name, version, registry,
   and SHA-256 in a publish operation.

   ```bash
   set -euo pipefail
   manifest_abs="$(realpath Cargo.toml)"
   publisher_cargo_home="$MANUAL_RELEASE_STATE_DIR/publisher-cargo-home"
   publisher_cwd="$MANUAL_RELEASE_STATE_DIR/publisher-cwd"
   test ! -e "$publisher_cargo_home" && test ! -e "$publisher_cwd"
   mkdir -m 700 "$publisher_cargo_home" "$publisher_cwd"
   (
     cd "$publisher_cwd"
     env -u CARGO_REGISTRY_TOKEN -u CARGO_REGISTRIES_CRATES_IO_TOKEN \
       CARGO_HOME="$publisher_cargo_home" \
       cargo publish --manifest-path "$manifest_abs" --dry-run --locked \
         --registry crates-io
   )
   publisher_crate="${CARGO_TARGET_DIR:-target}/package/pi_agent_rust-${RELEASE_VERSION}.crate"
   test -f "$publisher_crate" && test ! -L "$publisher_crate"
   test "$(sha256sum "$publisher_crate" | awk '{print $1}')" = "$expected_crate_sha256"
   test "$(wc -c < "$publisher_crate" | tr -d '[:space:]')" = "$expected_crate_size"
   test -z "$(git status --porcelain=v2 --untracked-files=all)"
   test "$(sha256sum "$provider" | awk '{print $1}')" = "$provider_sha256"

   registry_credential_config="$(PROVIDER_PATH="$provider" python3 - <<'PY'
   import json, os
   print("registry.credential-provider=" + json.dumps(os.environ["PROVIDER_PATH"]))
   PY
   )"
   named_credential_config="$(PROVIDER_PATH="$provider" python3 - <<'PY'
   import json, os
   print("registries.crates-io.credential-provider=" + json.dumps(os.environ["PROVIDER_PATH"]))
   PY
   )"
   actual_registry_provider="$(
     cd "$publisher_cwd"
     env -u PI_CRATES_IO_RELEASE_TOKEN -u CARGO_REGISTRY_TOKEN \
       -u CARGO_REGISTRIES_CRATES_IO_TOKEN CARGO_HOME="$publisher_cargo_home" \
       cargo -Z unstable-options config get registry.credential-provider \
         --format=json-value \
         --config 'registry.credential-provider="/bin/false"' \
         --config "$registry_credential_config" \
         --config 'registries.crates-io.credential-provider="/bin/false"' \
         --config "$named_credential_config"
   )"
   actual_named_provider="$(
     cd "$publisher_cwd"
     env -u PI_CRATES_IO_RELEASE_TOKEN -u CARGO_REGISTRY_TOKEN \
       -u CARGO_REGISTRIES_CRATES_IO_TOKEN CARGO_HOME="$publisher_cargo_home" \
       cargo -Z unstable-options config get registries.crates-io.credential-provider \
         --format=json-value \
         --config 'registry.credential-provider="/bin/false"' \
         --config "$registry_credential_config" \
         --config 'registries.crates-io.credential-provider="/bin/false"' \
         --config "$named_credential_config"
   )"
   test "$(jq -er '.' <<<"$actual_registry_provider")" = "$provider"
   test "$(jq -er '.' <<<"$actual_named_provider")" = "$provider"

   actual_receipt="$MANUAL_RELEASE_STATE_DIR/pi-crates-credential-receipt.json"
   test ! -e "$actual_receipt"
   test "$(sha256sum "$publisher_crate" | awk '{print $1}')" = "$expected_crate_sha256"
   IFS= read -r -s -p 'crates.io token: ' PI_CRATES_IO_RELEASE_TOKEN
   printf '\n'
   export PI_CRATES_IO_RELEASE_TOKEN
   trap 'unset PI_CRATES_IO_RELEASE_TOKEN' EXIT
   set +e
   (
     cd "$publisher_cwd"
     env -u CARGO_REGISTRY_TOKEN -u CARGO_REGISTRIES_CRATES_IO_TOKEN \
       CARGO_HOME="$publisher_cargo_home" \
       PI_EXPECTED_CRATE_NAME=pi_agent_rust \
       PI_EXPECTED_CRATE_VERSION="$RELEASE_VERSION" \
       PI_EXPECTED_CRATE_SHA256="$expected_crate_sha256" \
       PI_CREDENTIAL_RECEIPT="$actual_receipt" \
       cargo publish --manifest-path "$manifest_abs" --locked --no-verify \
         --registry crates-io \
         --config "$registry_credential_config" \
         --config "$named_credential_config"
   )
   cargo_status=$?
   set -e
   unset PI_CRATES_IO_RELEASE_TOKEN
   trap - EXIT
   test -f "$actual_receipt" && test ! -L "$actual_receipt"
   jq -e \
     --arg version "$RELEASE_VERSION" \
     --arg sha "$expected_crate_sha256" '
     .schema == "pi.release.cargo_credential_receipt.v1" and
     .name == "pi_agent_rust" and .version == $version and
     .crate_sha256 == $sha and .registry_name == "crates-io" and
     (.registry_index_url == "sparse+https://index.crates.io/" or
      .registry_index_url == "https://github.com/rust-lang/crates.io-index")
   ' "$actual_receipt" >/dev/null
   printf 'cargo_publish_exit=%s\ncredential_receipt_sha256=%s\n' \
     "$cargo_status" "$(sha256sum "$actual_receipt" | awk '{print $1}')" \
     >> "$proof_file"
   ```

   A nonzero Cargo exit is not success, but it can be an ambiguous network
   result. In every case, the following exact crates.io reconciliation is the
   authority. It must observe the non-yanked canonical name/version and exact
   checksum before GitHub publication; otherwise stop.

   ```bash
   set -euo pipefail
   PACKAGE_VERSION="$RELEASE_VERSION" \
   CRATE_SHA256="$expected_crate_sha256" python3 - <<'PY'
   import json
   import os
   import re
   import time
   import urllib.error
   import urllib.parse
   import urllib.request

   endpoint = (
       "https://crates.io/api/v1/crates/pi_agent_rust/"
       + urllib.parse.quote(os.environ["PACKAGE_VERSION"], safe="")
   )
   version = None
   for attempt in range(60):
       request = urllib.request.Request(
           endpoint,
           headers={"Accept": "application/json", "User-Agent": "pi-agent-rust-manual-release"},
       )
       try:
           with urllib.request.urlopen(request, timeout=30) as response:
               body = response.read(1024 * 1024 + 1)
       except urllib.error.HTTPError as exc:
           if exc.code != 404:
               raise
       else:
           if len(body) > 1024 * 1024:
               raise SystemExit("crates.io response exceeds 1 MiB")
           payload = json.loads(body)
           version = payload.get("version") if isinstance(payload, dict) else None
           if version is not None:
               break
       if attempt != 59:
           time.sleep(5)
   if not isinstance(version, dict) or version.get("crate") != "pi_agent_rust" \
       or version.get("num") != os.environ["PACKAGE_VERSION"] \
       or version.get("yanked") is not False \
       or version.get("checksum") != os.environ["CRATE_SHA256"] \
       or re.fullmatch(r"[0-9a-f]{64}", version.get("checksum", "")) is None:
       raise SystemExit("crates.io did not reconcile to the exact verified crate")
   PY
   ```

9. Make GitHub public last. Immediately before the PATCH, re-check the immutable
   tag rule, tag object/target, exact draft ID/state/title/body/prerelease, all
   12 names and bytes, and the crates.io checksum. PATCH by the recorded release
   database ID, then repeat the exact release verifier immediately afterward.

   ```bash
   set -euo pipefail
   prepublic_ruleset="$MANUAL_RELEASE_STATE_DIR/pre-public-ruleset.json"
   test ! -e "$prepublic_ruleset"
   gh api -H 'Accept: application/vnd.github+json' \
     "/repos/${RELEASE_REPOSITORY}/rulesets/${immutable_ruleset_id}?includes_parents=true" \
     > "$prepublic_ruleset"
   jq -e '
     .target == "tag" and .enforcement == "active" and
     ((.conditions.ref_name.include | index("refs/tags/v*")) != null or
      (.conditions.ref_name.include | index("~ALL")) != null) and
     .conditions.ref_name.exclude == [] and
     ([.rules[].type] | index("update")) != null and
     ([.rules[].type] | index("deletion")) != null and
     (.bypass_actors | type) == "array" and .bypass_actors == []
   ' "$prepublic_ruleset" >/dev/null

   verify_exact_release true immediately-before-publication
   registry_checksum="$(curl -fsS \
     "https://crates.io/api/v1/crates/pi_agent_rust/${RELEASE_VERSION}" \
     | jq -er --arg version "$RELEASE_VERSION" '
       select(.version.crate == "pi_agent_rust" and
              .version.num == $version and .version.yanked == false and
              (.version.checksum | test("^[0-9a-f]{64}$"))) |
       .version.checksum')"
   test "$registry_checksum" = "$expected_crate_sha256"

   public_payload="$MANUAL_RELEASE_STATE_DIR/github-public-payload.json"
   public_response="$MANUAL_RELEASE_STATE_DIR/github-public-response.json"
   test ! -e "$public_payload" && test ! -e "$public_response"
   jq -n \
     --arg tag "$RELEASE_TAG" \
     --arg commit "$expected_source_commit" \
     --arg title "$RELEASE_TAG" \
     --rawfile body "$release_body" \
     '{tag_name: $tag, target_commitish: $commit, name: $title,
       body: $body, draft: false, prerelease: false}' \
     > "$public_payload"
   gh api --method PATCH \
     -H 'Accept: application/vnd.github+json' \
     "/repos/${RELEASE_REPOSITORY}/releases/${release_id}" \
     --input "$public_payload" > "$public_response"
   jq -e \
     --argjson id "$release_id" \
     --arg tag "$RELEASE_TAG" \
     --arg commit "$expected_source_commit" \
     --rawfile body "$release_body" \
     '.id == $id and .draft == false and .prerelease == false and
      .tag_name == $tag and .target_commitish == $commit and
      .name == $tag and .body == $body' \
     "$public_response" >/dev/null
   verify_exact_release false immediately-after-publication
   test "$(curl -fsS \
     "https://crates.io/api/v1/crates/pi_agent_rust/${RELEASE_VERSION}" \
     | jq -er '.version.checksum')" = "$expected_crate_sha256"
   ```

   All provider code, its frozen-workflow source, hashes, self-test receipts,
   publication receipt, release metadata snapshots, and downloaded assets stay
   preserved under `MANUAL_RELEASE_STATE_DIR`. The manual lane cannot make the
   crates.io query and GitHub PATCH atomic, so the immutable server-side tag rule
   is a hard precondition. Any missing field, unreadable bypass list, changed
   hash, duplicate/extra asset, metadata drift, mismatched byte, or unexpected
   public state is a stop condition.

10. Smoke-test the binaries and installer on their target platforms, confirm
    crates.io still serves the exact non-yanked version/checksum, and prove the
    latest GitHub Actions run ID is unchanged from the pre-release baseline.
    Do not dispatch or rerun a workflow to obtain that evidence. Preserve the
    platform smoke receipts under `MANUAL_RELEASE_STATE_DIR`; a packaging
    format/architecture check is not a substitute for executing the binaries.

    ```bash
    set -euo pipefail
    postrelease_workflows="$MANUAL_RELEASE_STATE_DIR/github-actions-after-release.json"
    test ! -e "$postrelease_workflows"
    gh api -H 'Accept: application/vnd.github+json' \
      "/repos/${RELEASE_REPOSITORY}/actions/runs?per_page=1" \
      > "$postrelease_workflows"
    jq -e '
      (.total_count | type) == "number" and .total_count >= 0 and
      (.workflow_runs | type) == "array" and (.workflow_runs | length) <= 1 and
      all(.workflow_runs[]; (.id | type) == "number" and .id > 0)
    ' "$postrelease_workflows" >/dev/null
    test "$(jq -r '.workflow_runs[0].id // "none"' "$postrelease_workflows")" = \
      "$WORKFLOW_BASELINE_ID"
    curl -fsS \
      "https://crates.io/api/v1/crates/pi_agent_rust/${RELEASE_VERSION}" \
      | jq -e \
        --arg version "$RELEASE_VERSION" \
        --arg checksum "$expected_crate_sha256" '
        .version.crate == "pi_agent_rust" and
        .version.num == $version and .version.yanked == false and
        .version.checksum == $checksum
      ' >/dev/null
    ```

## Pre-release flow (rc)
Use an annotated pre-release tag to exercise the configured automated release
lane without publishing to crates.io:
- `git tag -a vX.Y.Z-rc.1 -m "vX.Y.Z-rc.1 release" && git push origin vX.Y.Z-rc.1`

`release.yml` skips crates.io and publishes a GitHub pre-release only after its
governance and artifact gates pass. `publish.yml` does not run on tag push; it
is an optional manual dry-run diagnostic. For the no-Actions DSR lane, keep the
tagged commit message marked `[skip actions]` and do not dispatch either
workflow.

## Merge-Gate DoD Policy
Feature-surface pull requests must satisfy the Definition-of-Done evidence checklist before merge:
- Unit evidence link(s)
- E2E evidence link(s)
- Extension evidence link(s)
- Reproduction commands for pass/fail validation paths

CI enforces this via `.github/workflows/ci.yml` using `.github/pull_request_template.md` as the
canonical checklist format.

### Migration Guidance for Existing Feature Branches
For branches opened before this gate was introduced:
1. Rebase onto latest `main`.
2. Replace the PR body with `.github/pull_request_template.md`.
3. Backfill links to current evidence artifacts.
4. Include exact rerun commands used to validate fixes for the most recent failing path.
5. Re-run CI and merge only after the DoD evidence guard passes.

## Pre-release checklist
- CI is green on `main` (Linux/macOS/Windows).
- Local gates are green:
  - `cargo fmt --check`
  - `cargo check --locked --all-targets --features internal-legacy-capture`
  - `cargo clippy --locked --all-targets --features internal-legacy-capture -- -D warnings`
  - `cargo test --locked --all-targets --features internal-legacy-capture`
- Feature PRs merged since the previous tag satisfy the DoD evidence checklist (unit + e2e + extension + repro commands).
- `CHANGELOG.md` updated for the version you’re tagging.
- Benchmarks run if this release is performance-sensitive (see the
  [benchmark guide](planning/BENCHMARKS.md)).
- Distribution compatibility matrix (above) passes for all required paths.

## Post-release checklist
- GitHub Release exists and includes expected artifacts for each platform.
- `SHA256SUMS` matches downloaded artifacts.
- Crates.io publish succeeded (if configured) and the version matches the tag.
- Smoke test install paths (download binary + run `pi --version`).
