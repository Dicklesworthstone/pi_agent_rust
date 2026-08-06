# Extension Platform Program Governance

This document defines ownership, decision authority, quality gates, and
maintenance cadence for the Pi extension platform.

---

## Ownership

| Area | Owner | Backup |
|------|-------|--------|
| Runtime (PiJS, QuickJS, hostcalls) | Primary maintainer | AI agent review (Claude/Codex) |
| Extension API surface | Primary maintainer | AI agent review |
| Capability policy (safe/balanced/permissive) | Primary maintainer | Security review required |
| CI/CD pipelines | Primary maintainer | Self-healing via gate promotion |
| Conformance corpus (223 extensions) | Automated via conformance harness | Manual triage for new failures |
| Documentation | Primary maintainer + agents | Automated staleness checks |
| Sibling crates (asupersync, rich_rust, charmed, sqlmodel) | Independently versioned | Cross-repo coordination via bead dependencies |

### Decision Authority

- **Breaking changes** to extension API: Primary maintainer approval required.
  Documented in CHANGELOG.md before release.
- **Capability policy changes** (adding/removing capabilities): Requires
  security review and conformance regression check.
- **New official extensions**: Must pass conformance at Tier 1-2 level with
  clean preflight report.
- **Dependency additions**: Must pass `cargo audit`, no `unsafe` in new deps.

---

## Quality Gates

### CI Gates (Enforced on Every PR)

| Gate | Threshold | Enforcement |
|------|-----------|-------------|
| `cargo fmt` | Zero diff | `.github/workflows/ci.yml` |
| `cargo clippy -D warnings` | Zero warnings | `.github/workflows/ci.yml` |
| Unit tests | 100% pass | `.github/workflows/ci.yml` |
| VCR/fixture tests | 100% pass | `.github/workflows/ci.yml` |
| No-mock dependency guard | Zero violations | `.github/workflows/ci.yml` |
| Suite classification guard | All files classified | `.github/workflows/ci.yml` |
| Traceability matrix guard | All classified tests traced | `.github/workflows/ci.yml` |
| VCR leak guard | No cassettes in wrong suite | `.github/workflows/ci.yml` |
| PR Definition-of-Done evidence guard | Required for feature-surface PRs | `.github/workflows/ci.yml` |

### Conformance Gates (Nightly + Release)

| Gate | Threshold | Source |
|------|-----------|--------|
| Extension corpus pass rate | >= 80% (current: 91.9%) | `conformance_summary.json` |
| Scenario conformance | >= 90% (current: 96.0%) | `scenario_conformance.json` |
| Node API matrix | 100% critical pass (current: 13/13) | `runtime_api_matrix.json` |
| Maximum failures | <= 36 extensions | `conformance_summary.json` |
| Maximum N/A | <= 170 extensions | `conformance_summary.json` |
| Policy negative tests | 100% pass | `conformance_report.json` |

### Performance Gates

| Budget | Threshold | Source |
|--------|-----------|--------|
| Cold extension load | < 200ms p95 | `budget_summary.json` |
| Warm extension load | < 50ms p95 | `budget_summary.json` |
| Event dispatch latency | < 10ms p95 | `budget_summary.json` |
| Binary size | < 50MB | CI artifact check |

See [BENCHMARKS.md](../BENCHMARKS.md) for full budget definitions.

### Release Gates (1.0 Criteria)

Per [releasing.md](releasing.md):
- CI green on Linux/macOS/Windows
- Core CLI modes stable (print + interactive + RPC)
- Extension runtime surface and security policy stable
- Conformance gates met at release thresholds

### Definition of Done for Feature Changes

Feature-surface PRs (runtime/provider/tooling behavior changes) are not mergeable until:

1. PR body includes checked evidence for unit, e2e, and extension validation.
2. PR body links directly to structured artifacts/logs for those runs.
3. PR body includes reproduction commands for both passing validation and the most recent failing path.
4. PR body contains no unresolved checklist placeholders.

The canonical checklist source is `.github/pull_request_template.md`, and CI enforces it in
the Linux PR lane.

#### Migration Guidance for Existing Feature Branches

1. Rebase on latest `main`.
2. Replace PR body with `.github/pull_request_template.md`.
3. Backfill evidence links from latest CI/local runs.
4. Include explicit failing-path artifact links plus exact rerun commands.
5. Re-run CI and merge only after DoD guard passes.

---

## Release Cadence

| Channel | Frequency | Scope |
|---------|-----------|-------|
| Patch (`0.x.Y`) | As needed | Bug fixes, conformance improvements |
| Minor (`0.X.0`) | Monthly | New features, API additions |
| Major (`X.0.0`) | When 1.0 criteria met | Stability commitment |
| Pre-release (`-rc.N`) | Before major/minor | Validation window |

### Versioning Rules

- **SemVer** with tag format `vX.Y.Z` (source of truth: `Cargo.toml`).
- Pre-1.0: breaking changes allowed with changelog documentation.
- Post-1.0: breaking changes require major version bump and deprecation window.
- Sibling crates versioned independently.

See [releasing.md](releasing.md) for the full release process.

---

## Maintenance Cadence

### Weekly

| Task | Owner | Verification |
|------|-------|-------------|
| Dependency audit (`cargo audit`) | Automated | CI gate |
| Conformance regression review | Agent triage | Nightly CI reports |
| Bead backlog grooming | Primary maintainer | `bv --robot-plan` + `br ready --json` |

### Weekly QA Burndown (bd-1f42, as of 2026-02-10)

Snapshot:
- Scope: `bd-1f42*` QA epic and sub-beads
- Total: 51 beads (`8 closed`, `7 in_progress`, `36 open`)
- Priority mix: `open P0=19`, `open P1=17`, `in_progress P0=5`, `in_progress P1=2`
- Slip check: no `bd-1f42*` bead is past due as of `2026-02-10`.
- Near-term risk window: `2026-02-13` to `2026-02-16` (baseline-audit and core-unit milestones).

Top blocker graph pressure (open blockers):
- `bd-1f42.6.5` blocked by 8 prerequisites
- `bd-1f42.4` blocked by 7 prerequisites
- `bd-1f42.6` blocked by 7 prerequisites
- `bd-1f42.2` blocked by 6 prerequisites
- `bd-1f42.7.3` blocked by 6 prerequisites

#### Blocker RCA + Unblock Actions

| Blocked target | Root cause analysis | Accountable owner | Unblock action (next checkpoint) |
|----------------|---------------------|-------------------|----------------------------------|
| `bd-1f42.3` (E2E harness track) | Critical-path concentration on runner core (`bd-1f42.3.1`) and versioned logging contract (`bd-1f42.3.6`) keeps scenario/replay beads from starting. | `TopazForest` | Close `bd-1f42.3.1`, then land `bd-1f42.3.6` logging contract to unlock downstream scenario/replay work. |
| `bd-1f42.4` (208-extension matrix track) | Fixture corpus + executor + gate/reporting work is still serial (`bd-1f42.4.2`/`.4.3`/`.4.4`), so downstream compatibility and dossier beads remain blocked. | `OrangeBarn` | Prioritize `bd-1f42.4.3` sharded executor completion, then wire `bd-1f42.4.4` CI must-pass gate immediately after. |
| `bd-1f42.6.5` (final full-suite gate) | Final release-blocking gate depends on unfinished deliverables across CI, unit, e2e, extension, and reliability tracks. | `PearlRaven` (coordination owner for `bd-1f42.6.*`) | Keep prerequisite checklist explicit and sequence completion `bd-1f42.6.1` -> `bd-1f42.6.8` -> `bd-1f42.6.5` with owner check-ins each burndown cycle. |
| `bd-1f42.7.3` (final certification) | Certification is evidence-gated: governance cadence (`bd-1f42.7.2`) plus runbook and CI evidence-bundle dependencies are still open. | `BrightValley` | Maintain weekly burndown updates, close `bd-1f42.7.4` runbook, then assemble certification once `bd-1f42.6.8` and `bd-1f42.4.5` are complete. |

Next burndown checkpoint: `2026-02-17` (UTC), with updated blocker counts and owner-level action status.

### Weekly Provider Parity Rollup (bd-3uqg, as of 2026-02-12)

Snapshot:
- Scope: `bd-3uqg*` provider-parity epic and sub-beads.
- Ready front: only `bd-3uqg` is currently unblocked/open; most downstream tasks are intentionally dependency-gated.
- Highest-impact current blocker: `bd-3uqg.3.8.4` (native parity consolidation), which unlocks `bd-3uqg.8`, `bd-3uqg.8.2`, and `bd-3uqg.9.1.2`.

Execution lanes and ownership:

| Lane | Active beads | Current owner/assignee | Handoff target |
|------|--------------|------------------------|----------------|
| Native implementation | `bd-3uqg.3` | `RusticPrairie` | `bd-3uqg.3.8` verification |
| Native verification | `bd-3uqg.3.8` | `AmberHill` | `bd-3uqg.3.8.4` consolidated report |
| Native parity consolidation | `bd-3uqg.3.8.4` | `IvoryPuma` | Test/docs expansion (`bd-3uqg.8*`, `bd-3uqg.9.1.2`) |
| Provider onboarding docs | `bd-3uqg.9` | `TealFox` | Docs matrix and playbook children |
| Provider support matrix | `bd-3uqg.9.1` | `QuietCove` | `bd-3uqg.9.1.2` and `bd-3uqg.9.1.3` |
| Rollup orchestration | `bd-3uqg`, `bd-3uqg.10`, `bd-3uqg.10.1` | `CopperCreek` | `bd-3uqg.10.2` gate execution |

Sequencing gates (ordered):
1. **Gate A - Native parity evidence complete**
Criteria: `bd-3uqg.3.8.4` includes provider-level pass/fail matrix with links to unit/e2e logs and explicit deviation notes.
2. **Gate B - Core provider test harnesses**
Criteria: `bd-3uqg.8.2`, `bd-3uqg.8.3`, and `bd-3uqg.8.4` land with deterministic fixtures, event-parity assertions, and CI artifact hooks.
3. **Gate C - Provider docs evidence closure**
Criteria: `bd-3uqg.9.1.2`, `bd-3uqg.9.1.3`, `bd-3uqg.9.2`, `bd-3uqg.9.3`, `bd-3uqg.9.4`, and `bd-3uqg.9.5` reference concrete test artifacts rather than narrative-only claims.
4. **Gate D - Rollup certification path**
Criteria: `bd-3uqg.10.2` runs mandatory quality gates, then `bd-3uqg.10.3` performs final parity audit, and `bd-3uqg.10.4` publishes completion handoff + remaining-gap beads.

Anti-stall checkpoints:
- Every active `bd-3uqg*` assignee posts a concise progress update in Agent Mail thread `br-3uqg` at least once per 24 hours while `in_progress`.
- Any bead blocked for more than 12 hours must post a blocker note with exact blocking bead IDs, affected files, and required unblock action.
- Rollup owner re-runs `bv --robot-plan` and `bv --robot-priority` at each checkpoint, then validates proposed bead IDs with `br ready --json` / `br show <id>` to skip tombstones before reassigning effort.
- No new provider-family onboarding starts until Gate A evidence exists, to prevent parallel drift and report mismatch.

Next provider rollup checkpoint: `2026-02-13` (UTC), focused on `bd-3uqg.3.8.4` completion and Gate A evidence audit.

### Monthly

| Task | Owner | Verification |
|------|-------|-------------|
| Performance budget review | Primary maintainer | `budget_summary.json` trends |
| Extension corpus update | Automated discovery | `extension-inclusion-list.json` |
| Documentation staleness check | Traceability tests | `traceability_staleness.rs` |
| CI gate threshold review | Primary maintainer | Gate promotion workflow |

### Quarterly

| Task | Owner | Verification |
|------|-------|-------------|
| Full conformance campaign (223 extensions) | Automated | `conformance.yml` nightly |
| Security review of capability policies | Primary maintainer | Threat model doc |
| Dependency major version updates | Primary maintainer | `cargo update` + full test |
| Roadmap review and bead reprioritization | Primary maintainer | `bv --robot-plan` + `bv --robot-priority` |

---

## Extension Governance

### Tiers and Vetting

| Tier | Count | Vetting | Conformance |
|------|------:|---------|-------------|
| Official | 60 | Full review, MIT license verified | 100% pass required |
| Community | 58 | Automated conformance check | 90%+ pass target |
| npm registry | 66 | License + provenance check | Best-effort |
| Third-party | 23 | Provenance verification | Best-effort |
| Agent ecosystem | 1+ | Same as community | Same as community |

### Adding New Extensions

1. Extension passes `pi doctor` with PASS verdict.
2. License is permissive (MIT, Apache-2.0, BSD).
3. Provenance is pinnable (git commit hash or npm version).
4. No per-extension patches required (unmodified compatibility).
5. Added to `docs/extension-inclusion-list.json` with tier assignment.

### Removing Extensions

- Extensions are removed from the corpus when:
  - License changes to incompatible terms.
  - Source becomes unavailable (unpinnable provenance).
  - Extension requires per-extension patches to function.
- Removal is documented in CHANGELOG.md.

---

## Deprecation Policy

### Pre-1.0 (Current)

- Breaking changes are allowed with CHANGELOG documentation.
- Deprecated APIs emit runtime warnings for at least one minor release.
- CLI flag changes are documented in `--help` output.

### Post-1.0 (Future)

- Deprecated APIs: warning for 2 minor releases, removal in next major.
- CLI flag removal: 1 minor release deprecation window.
- Extension API changes: 2 minor release deprecation window.
- Capability policy changes: announced in release notes.

---

## Incapacity Playbook

This playbook covers maintainer unavailability, account compromise, or a
release-blocking emergency where the primary maintainer cannot operate the
repository directly. It is intentionally procedural and does not store secrets,
private contact details, recovery phrases, or personal account material in this
repository.

### Activation Criteria

Use this path only when at least one condition is true:

1. The primary maintainer is unreachable for 72 hours and a Critical/High
   security fix, installer breakage, or release-blocking regression must ship.
2. The primary maintainer explicitly delegates emergency control in writing.
3. The GitHub or crates.io account appears compromised and normal release
   authority must be suspended while credentials are rotated.

### Repo Ownership Escalation

| Step | Action | Evidence |
|------|--------|----------|
| 1 | Open an emergency tracking bead with label `operations` and link the triggering incident. | `br show <id> --json` |
| 2 | Confirm the activating condition with the out-of-band maintainer contact roster stored outside this repo. | Emergency log entry, not committed |
| 3 | Have the designated emergency repo admin accept temporary operational ownership. | GitHub organization or repo audit log |
| 4 | Freeze non-emergency merges until CI, release tags, and installer distribution are verified. | Branch ruleset/audit-log snapshot |
| 5 | Record every action in the emergency bead and in release notes if a user-visible build ships. | Bead comments, CHANGELOG entry |

The private roster must identify at least two emergency contacts: one GitHub
repository administrator and one release/secrets operator. If either role is
vacant, the weekly governance review must file a P1 operations bead.

### Secrets Recovery and Rotation

No secret value belongs in this repo. The emergency operator should recover or
rotate credentials from the private secrets manager/escrow only after the
activation criteria are met.

| Secret or authority | Recovery/rotation action | Verification |
|---------------------|--------------------------|--------------|
| GitHub repository/admin access | Rotate compromised credentials, review SSH/GPG keys, revoke unknown tokens, and confirm branch rulesets still protect `main`. | GitHub security log + ruleset snapshot |
| `CARGO_REGISTRY_TOKEN` | Revoke the old crates.io token, issue a least-privilege replacement, and update the Actions secret. | `Publish` workflow dry-run or pre-release tag |
| Release workflow authority | Verify `.github/workflows/release.yml` permissions still use GitHub-scoped release rights only. | Workflow run log |
| Installer artifact integrity | Regenerate release assets, `SHA256SUMS`, and Sigstore/checksum evidence before advising users to install. | GitHub Release artifact list + checksum proof |
| Local signing or recovery material | Rotate outside Git, then document only the rotation event and operator identity. | Private escrow audit log |

After rotation, invalidate all emergency session tokens, remove temporary
administrators, and capture a final audit-log snapshot before returning to
normal governance.

### Emergency Release Process

1. Rebase on latest `main` and keep the patch scoped to the emergency.
2. Run the mandatory local gates from [AGENTS.md](../AGENTS.md), including
   `cargo check --all-targets`, `cargo clippy --all-targets -- -D warnings`,
   `cargo fmt --check`, and `./scripts/reconcile_beads_ledger.sh`.
3. Cut a pre-release tag first when time allows: `vX.Y.Z-rc.N`.
4. Verify the `Publish` and `Release (GitHub binaries)` workflows in
   [releasing.md](releasing.md), including installer regression and checksum
   artifacts.
5. Promote to the final SemVer tag only after the release artifact path and
   installer path both pass smoke tests.
6. Document the reason for emergency authority in `CHANGELOG.md` and the
   emergency bead.

If a Critical/High vulnerability is actively exploitable, the emergency repo
admin may bypass the normal monthly cadence, but may not bypass branch
protection, code review evidence, or release artifact verification.

### Branch-Protection Authority

Only the primary maintainer or the designated emergency repo admin may change
branch protection, required checks, release workflow permissions, or tag
protection. During an emergency, any such change requires:

1. A linked emergency bead.
2. A second human or agent review recorded in the bead.
3. A before/after snapshot of the GitHub ruleset or workflow permission diff.
4. Immediate restoration of the stricter rule after the emergency release.

Branch protection must never be relaxed to make unrelated work mergeable during
an incapacity event.

### Tabletop Dry Run

Run this review quarterly or after any release-process change:

1. Pick a hypothetical Critical installer regression.
2. Identify the emergency repo admin and release/secrets operator from the
   private roster without exposing their contact details in Git.
3. Walk through credential rotation for `CARGO_REGISTRY_TOKEN` as a dry run:
   do not rotate unless there is a real incident, but verify the owner and
   storage location are known.
4. Trigger or inspect a non-publishing release rehearsal, preferably a
   pre-release tag or workflow dry run.
5. Confirm the installer path verifies `SHA256SUMS` and published install
   commands never pass the installer's testing-only `--no-verify` flag. This is
   distinct from the credential-scoped Cargo upload described in the release
   runbook.
6. File beads for any missing role, stale secret owner, failing workflow, or
   ambiguous branch-protection authority.

The tabletop outcome is pass only when every role is named in private escrow,
the release workflow can be rehearsed without publishing a final release, and
no unresolved Critical/High operations gap remains untracked.

---

## Incident Response

### Conformance Regression on Main

1. Triage within 24 hours via `bv --robot-plan` + `br ready --json`, and confirm candidate IDs with `br show <id>` before claiming.
2. If regression is in runtime: fix and cut patch release.
3. If regression is in test harness: fix harness, re-run campaign.
4. If regression is in extension: update exception list with justification.

### Security Vulnerability

1. Assess severity (capability escape, sandbox bypass, data leak).
2. Critical/High: fix within 48 hours, cut patch release.
3. Medium/Low: fix in next scheduled release.
4. Document in CHANGELOG.md and release notes.

### Performance Regression

1. Compare against baseline in `budget_summary.json`.
2. If budget exceeded by >20%: investigate and fix before release.
3. If budget exceeded by <20%: document and adjust threshold if justified.

---

## Roadmap

### Current Program (bd-k5q5)

| Epic | Status | Goal |
|------|--------|------|
| Conformance evidence (bd-k5q5.2) | In progress | 223 scenarios green or documented |
| Node/Bun compatibility (bd-k5q5.3) | Closed | 18+ Node modules shimmed |
| Capability policy (bd-k5q5.4) | Closed | Safe/balanced/permissive profiles |
| CI gates (bd-k5q5.5) | In progress | Regression prevention |
| Documentation (bd-k5q5.6) | In progress | Architecture + operator docs |
| Verification program (bd-k5q5.7) | In progress | Unit + E2E + diagnostics |

### 1.0 Milestones

1. All CI gates green on Linux/macOS/Windows.
2. Extension conformance >= 90% with documented exceptions.
3. Performance budgets met consistently over 30 days.
4. Public documentation complete (compatibility matrix, playbook, governance).
5. Security review of capability policies complete.

---

## Related Documents

| Document | Path | Purpose |
|----------|------|---------|
| Release process | [docs/releasing.md](releasing.md) | Versioning, tagging, publishing |
| Testing policy | [docs/testing-policy.md](testing-policy.md) | Suite classification, enforcement |
| Compatibility matrix | [docs/ext-compat.md](ext-compat.md) | Node/Bun API support |
| Operator playbook | [docs/conformance-operator-playbook.md](conformance-operator-playbook.md) | Running conformance tests |
| Troubleshooting | [docs/extension-troubleshooting.md](extension-troubleshooting.md) | Common failure patterns |
| Benchmarks | [BENCHMARKS.md](../BENCHMARKS.md) | Performance budgets |
| Architecture | [docs/extension-architecture.md](extension-architecture.md) | Runtime design |
| Threat model | [docs/extension-runtime-threat-model.md](extension-runtime-threat-model.md) | Security analysis |
| Traceability matrix | [docs/traceability_matrix.json](traceability_matrix.json) | Requirement-to-test mapping |
