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
`.github/workflows/publish.yml` is triggered on tag pushes matching `v*` and will:
1) validate the tag is SemVer
2) verify `Cargo.toml` version matches the tag version
3) run `cargo publish --dry-run --locked`
4) publish to crates.io **only** when:
   - the parsed SemVer has no pre-release component
   - `CARGO_REGISTRY_TOKEN` is configured

Release and publish workflows resolve the sibling-project crates from crates.io
under `Cargo.lock`; they do not build against arbitrary sibling repository
checkouts. Per-target build manifests therefore record selected locked crate
versions, registry sources, and checksums rather than unrelated repository HEADs.

### Publishing GitHub Releases binaries
`.github/workflows/release.yml` is triggered on tag pushes matching `v*` and will:
- build `pi` for Linux/macOS/Windows (release profile)
- attach platform archives, per-target build manifests, and `SHA256SUMS` to a GitHub Release
- mark the GitHub Release as a pre-release when the parsed SemVer has a
  pre-release component (for example, `-rc.1`)

Release notes are extracted from `CHANGELOG.md` on a best-effort basis; ensure the changelog contains a `##` heading with the version string for the tag you are cutting.

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
6) **Tag**:
   - `git tag vX.Y.Z`
   - `git push origin vX.Y.Z`
7) **Verify** GitHub Actions:
   - `Publish` workflow (crates.io publish) behaves as expected
   - `Release (GitHub binaries)` workflow creates a GitHub Release with binaries + `SHA256SUMS`

## Manual DSR lane (no GitHub Actions)

Use this lane when the release is intentionally built and published from the
operator hosts. It does not dispatch, rerun, or otherwise invoke a GitHub
Actions workflow. Keep every pushed release-preparation, source, and evidence
commit marked with `[skip actions]`; the commit ultimately referenced by the
tag must contain that marker. Use an annotated tag with the marker as an
additional auditable signal.

1. Run the locked repository gates, including the internal capture target:

   ```bash
   cargo fmt --check
   cargo check --locked --all-targets --features internal-legacy-capture
   cargo clippy --locked --all-targets --features internal-legacy-capture -- -D warnings
   cargo test --locked --all-targets --features internal-legacy-capture
   cargo package --locked
   cargo publish --dry-run --locked
   ```

2. Commit the clean release source before generating tracked evidence. Every
   commit message in this lane must end with `[skip actions]`:

   ```bash
   git commit -m "Prepare vX.Y.Z release source [skip actions]"
   ```

3. Generate source-bound conformance evidence explicitly, commit it, then run
   the mandatory manual release gate. Ordinary test runs are read-only and do
   not freshen these tracked artifacts. The v0.x lane does not make a strict
   drop-in claim, while preflight and quality remain required:

   ```bash
   PI_GENERATE_CONFORMANCE_REPORT=1 \
     cargo test --locked --test conformance_report \
     generate_conformance_report -- --exact --nocapture
   git commit -m "Record vX.Y.Z release evidence [skip actions]"

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
   git push origin main
   git push origin main:master
   ```

4. Validate DSR and all configured native build hosts, then inspect the build
   plan before executing it:

   ```bash
   dsr doctor
   dsr repos validate
   dsr health all
   dsr build pi_agent_rust --version X.Y.Z --dry-run
   ```

   DSR source synchronization mirrors into its configured destinations. Never
   sync into a shared or dirty checkout. Prepare dedicated host checkouts at
   the exact release commit, verify each checkout's `HEAD`, and then build with
   `--no-sync`:

   ```bash
   dsr build pi_agent_rust --version X.Y.Z --no-sync -o /path/to/release-artifacts
   ```

5. Verify the artifacts and `SHA256SUMS` locally. Create and push the annotated
   tag only after the artifacts came from the exact clean release commit:

   ```bash
   git tag -a vX.Y.Z -m "vX.Y.Z manual DSR release [skip actions]"
   git push origin vX.Y.Z
   ```

6. Upload a draft without dispatching any workflow, inspect the draft and its
   complete five-platform asset set, then publish it and publish the crate
   manually:

   ```bash
   dsr release pi_agent_rust X.Y.Z --draft --no-dispatch \
     --artifacts /path/to/release-artifacts
   gh release edit vX.Y.Z --draft=false
   cargo publish --locked
   ```

7. Re-download the published assets, verify their checksums, smoke-test the
   binaries and installer on their target platforms, and confirm crates.io
   serves version `X.Y.Z`. Also confirm that no workflow run newer than the
   recorded pre-release baseline was created.

## Pre-release flow (rc)
Use a pre-release tag to exercise CI/publish validation without publishing to crates.io:
- `git tag vX.Y.Z-rc.1 && git push origin vX.Y.Z-rc.1`

This should run the `Publish` workflow planning step and skip the crates publish step.

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
