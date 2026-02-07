# Extension Corpus Refresh Checklist

Step-by-step procedure for refreshing the extension corpus: discovering new
extensions, validating them, running conformance + perf, and updating all
downstream artifacts. Executable by any engineer without extra context.

---

## Prerequisites

- Rust nightly toolchain (for `cargo test --features ext-conformance`)
- Bun 1.3+ (`~/.bun/bin/bun`) for the TS oracle
- `jq` for JSON manipulation
- Access to GitHub and npm registry for discovery sweeps

---

## Phase 1: Discovery (new candidates)

### 1.1 Upstream sync

```bash
# Pull latest pi-mono upstream examples
cd legacy_pi_mono_code/pi-mono
git pull origin main
```

Check for new extensions under:
- `packages/coding-agent/examples/extensions/`
- `.pi/extensions/`

### 1.2 GitHub discovery sweep

Search for new Pi extensions on GitHub:

```bash
# Keyword searches (adjust date range)
gh search repos "pi extension" --language=typescript --sort=updated
gh search repos "pi-agent extension" --sort=updated
gh search code "registerTool" "pi.registerTool" --language=typescript
```

Record new candidates in `docs/EXTENSION_CANDIDATES.md`.

### 1.3 npm registry sweep

```bash
# Search npm for Pi extension packages
npm search pi-extension
npm search @anthropic/pi
```

### 1.4 Dedup against existing corpus

Compare new candidates against `docs/extension-master-catalog.json` using
the canonical source key + content checksum strategy (see EXTENSIONS.md
section 1C.3).

Remove duplicates. Add genuinely new candidates to the candidate pool.

---

## Phase 2: Acquisition (download + organize)

### 2.1 Download new extensions

Place sources under the appropriate corpus directory:

| Source tier | Directory |
|---|---|
| `official-pi-mono` | `tests/ext_conformance/artifacts/plugins-official/` |
| `community` | `tests/ext_conformance/artifacts/community/` |
| `npm-registry` | `tests/ext_conformance/artifacts/npm/` |
| `third-party-github` | `tests/ext_conformance/artifacts/plugins-community/` |

### 2.2 Record provenance

For each new extension, record:
- Source URL / repo / npm package
- Version / commit hash
- License
- File count and total bytes

Update `docs/extension-artifact-provenance.json` with the new entries.

---

## Phase 3: TS Oracle Validation (ground truth)

### 3.1 Run the TS oracle on new extensions

```bash
cd tests/ext_conformance/ts_oracle

# Single extension
bun run load_extension.ts /path/to/new_extension.ts

# Batch (all new)
bash batch_load.sh /path/to/new_extensions_dir/
```

The oracle records: load success/failure, registered tools, commands, hooks,
flags, providers, shortcuts.

### 3.2 Update VALIDATED_MANIFEST.json

Merge oracle output into `tests/ext_conformance/VALIDATED_MANIFEST.json`.

Each entry needs:
- `id`: stable extension identifier
- `source_tier`: provenance tier
- `entry_path`: relative path to the extension source
- `expected_snapshot`: the oracle's registration output (tools, commands, etc.)

### 3.3 Verify manifest integrity

```bash
# Count entries
jq '.extensions | length' tests/ext_conformance/VALIDATED_MANIFEST.json

# Check for duplicate IDs
jq '[.extensions[].id] | group_by(.) | map(select(length > 1)) | length' \
  tests/ext_conformance/VALIDATED_MANIFEST.json
# Should output: 0
```

---

## Phase 4: Conformance Testing (Rust runtime)

### 4.1 Regenerate conformance test file

If the `conformance_test!` macro entries in
`tests/ext_conformance_generated.rs` need updating (new extensions added),
regenerate from the manifest.

### 4.2 Run all conformance tests

```bash
cargo test --test ext_conformance_generated --features ext-conformance -- --nocapture
```

### 4.3 Generate full conformance report

```bash
cargo test --test ext_conformance_generated conformance_full_report \
  --features ext-conformance -- --nocapture
```

This produces:
- `tests/ext_conformance/reports/conformance_events.jsonl`
- `tests/ext_conformance/reports/conformance_summary.json`
- `tests/ext_conformance/reports/CONFORMANCE_REPORT.md`

### 4.4 Update conformance baseline

```bash
# Review changes
diff <(jq . tests/ext_conformance/reports/conformance_baseline.json) \
     <(jq . /tmp/new_baseline.json)
```

Update `tests/ext_conformance/reports/conformance_baseline.json` with new
counts and failure classifications.

### 4.5 Classify any new failures

For each new failure, determine root cause and add to the appropriate
category in the baseline:

| Category | Description |
|---|---|
| `manifest_registration_mismatch` | Oracle and Rust register different items |
| `missing_npm_package` | Extension imports an npm package we don't stub |
| `multi_file_dependency` | Extension imports from sibling directories |
| `runtime_error` | JS throws during load/registration |
| `test_fixture` | Not a real extension (test infrastructure) |

---

## Phase 5: Performance Benchmarking

### 5.1 Run PR benchmark (quick check)

```bash
PI_BENCH_MODE=pr cargo test --test ext_bench_harness \
  --features ext-conformance -- --nocapture
```

### 5.2 Run nightly benchmark (full corpus)

```bash
PI_BENCH_MODE=nightly PI_BENCH_MAX=200 PI_BENCH_ITERATIONS=10 \
  cargo test --test ext_bench_harness --features ext-conformance -- --nocapture
```

This produces:
- `tests/perf/reports/ext_bench_baseline.json`
- `tests/perf/reports/BASELINE_REPORT.md`
- `tests/perf/reports/budget_summary.json`

### 5.3 Check budget compliance

```bash
cargo test --test perf_budgets --features ext-conformance -- --nocapture
```

All budgets in `tests/perf_budgets.rs` must pass. Key thresholds:

| Budget | Threshold |
|---|---|
| Cold load P95 (across extensions) | < 200ms |
| Warm load P95 | < 100ms |
| Event dispatch P99 | < 5ms |

### 5.4 Investigate regressions

If a budget fails, compare against the previous baseline:

```bash
# Side-by-side P95 comparison
jq '.scenarios.cold_load.p95_us' tests/perf/reports/ext_bench_baseline.json
```

Check for new extensions that are unusually slow (outliers in cold load).

---

## Phase 6: Catalog + Documentation Updates

### 6.1 Update extension catalog

Update `docs/extension-catalog.json` to include new extensions. Each entry
requires (per `docs/extension-catalog.schema.json`):

**Required fields:**
- `id`, `name`, `source_tier`, `source` (git/npm/url ref)
- `runtime_tier` (`legacy-js` / `multi-file` / `pkg-with-deps`)
- `interaction_tags`, `capabilities`, `io_pattern`, `complexity`
- `file_count`, `total_bytes`, `checksum.sha256`

**Optional fields (populate from test results):**
- `compatibility_notes.conformance_status` (`pass` / `fail`)
- `compatibility_notes.conformance_tier` (1-5)
- `compatibility_notes.failure_category` (if failed)
- `perf_budgets.cold_load_ms` (from benchmark baseline)

### 6.2 Validate catalog against schema

```bash
# Use ajv or similar JSON Schema validator
npx ajv validate -s docs/extension-catalog.schema.json \
  -d docs/extension-catalog.json
```

### 6.3 Update COMPATIBILITY_SUMMARY.md

Regenerate `tests/ext_conformance/reports/COMPATIBILITY_SUMMARY.md` with
updated numbers from the conformance and perf runs.

### 6.4 Update EXTENSIONS.md

Update the "Achieved coverage" table in EXTENSIONS.md section 1C.5 with
the new pass/fail counts.

### 6.5 Update README.md

If overall pass rate changed significantly, update the Extensions section
in README.md.

---

## Phase 7: Commit + Verify

### 7.1 Verify no regressions

```bash
# Regression check: no previously-passing extension should now fail
# Compare old baseline pass list against new results
```

The conformance baseline includes `regression_thresholds`:
- Tier 1 (simple): must remain 100%
- Tier 2 (multi-reg): must remain >= 95%
- Overall: must remain >= 80%
- Max 3 new failures per refresh cycle

### 7.2 Commit artifacts

Stage and commit the following files:

```bash
# Core artifacts
git add tests/ext_conformance/VALIDATED_MANIFEST.json
git add tests/ext_conformance_generated.rs
git add docs/extension-catalog.json

# Reports (tracked via .gitignore negation rules)
git add tests/ext_conformance/reports/conformance_baseline.json
git add tests/ext_conformance/reports/conformance_summary.json
git add tests/ext_conformance/reports/CONFORMANCE_REPORT.md
git add tests/ext_conformance/reports/COMPATIBILITY_SUMMARY.md

# Documentation
git add EXTENSIONS.md README.md

git commit -m "chore(extensions): refresh corpus (N new, M total, X% pass)"
```

### 7.3 Push

```bash
git push origin main && git push origin main:master
```

---

## Exit Criteria

A refresh is complete when ALL of the following are true:

- [ ] All new candidates have been through the TS oracle
- [ ] `VALIDATED_MANIFEST.json` is updated with new entries
- [ ] All conformance tests run (no test infrastructure failures)
- [ ] Conformance baseline updated with new counts
- [ ] No regression: previously-passing extensions still pass
- [ ] Performance benchmarks run; budgets pass (or regressions documented)
- [ ] `docs/extension-catalog.json` includes new entries with conformance status
- [ ] Catalog validates against `docs/extension-catalog.schema.json`
- [ ] `COMPATIBILITY_SUMMARY.md` reflects new numbers
- [ ] `EXTENSIONS.md` section 1C.5 coverage table updated
- [ ] All changes committed and pushed

---

## Artifacts Updated Per Refresh

| Artifact | Purpose | Location |
|---|---|---|
| Validated manifest | Ground truth registrations | `tests/ext_conformance/VALIDATED_MANIFEST.json` |
| Generated tests | Rust conformance test cases | `tests/ext_conformance_generated.rs` |
| Conformance baseline | Pass/fail counts + failure categories | `tests/ext_conformance/reports/conformance_baseline.json` |
| Conformance summary | Machine-readable summary | `tests/ext_conformance/reports/conformance_summary.json` |
| Conformance report | Human-readable per-extension results | `tests/ext_conformance/reports/CONFORMANCE_REPORT.md` |
| Compatibility summary | Combined conformance + perf overview | `tests/ext_conformance/reports/COMPATIBILITY_SUMMARY.md` |
| Perf baseline | Per-extension load time percentiles | `tests/perf/reports/ext_bench_baseline.json` |
| Budget summary | Budget pass/fail results | `tests/perf/reports/budget_summary.json` |
| Extension catalog | Enriched metadata (223+ entries) | `docs/extension-catalog.json` |
| Artifact provenance | Source tracking + licenses | `docs/extension-artifact-provenance.json` |
| EXTENSIONS.md | Architecture doc with coverage tables | `EXTENSIONS.md` |
| README.md | User-facing extension status | `README.md` |
