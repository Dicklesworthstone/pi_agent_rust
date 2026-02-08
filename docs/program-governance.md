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
| Bead backlog grooming | Primary maintainer | `bv --robot-triage` |

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
| Roadmap review and bead reprioritization | Primary maintainer | `bv --robot-triage` |

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

## Incident Response

### Conformance Regression on Main

1. Triage within 24 hours via `bv --robot-triage`.
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
