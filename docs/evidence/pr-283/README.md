# PR #283: selective recovery and validation qualification

Date: November 5, 2026.

This record separates executed module diagnostics from full-application and DSR
qualification. It is not merge approval for PR #283 and is not a release-quality
receipt. The contribution was inspected at
`9bc06d639ece500177452568b0be64f1c33ba50a`; the initial main baseline was
`630fdcaae50537efa77e33c2cfeb73738072f7b8`.

During the review, the open PR advanced to
`87f1075847940940eb33f94472cbe8c3ab1a3bdb`. Its two follow-up commits replace the
unavailable `Result::is_err_or` call and change resource-governor arithmetic.
The compiler failure recorded below is historical evidence for `9bc06d6`, not
a claim that the latest PR still contains that particular call. The independent
recovery fix already corrects it. The resource-governor change was not imported
or qualified here, and the full latest PR was not merged or qualified.

The selected improvements landed through GitHub in three incremental main
commits:

- [987f685f71c41bf88c1dd813ac5b97f79578c895](https://github.com/Dicklesworthstone/pi_agent_rust/commit/987f685f71c41bf88c1dd813ac5b97f79578c895): validation timestamp ordering and mutation atomicity.
- [c6bb0f8deee39d43ef940a9a44ef9ebfe2747ba7](https://github.com/Dicklesworthstone/pi_agent_rust/commit/c6bb0f8deee39d43ef940a9a44ef9ebfe2747ba7): opt-in recovery engine and fail-closed integration.
- [667c9ba8b92aefdace99d9d3bc7472007031a6e5](https://github.com/Dicklesworthstone/pi_agent_rust/commit/667c9ba8b92aefdace99d9d3bc7472007031a6e5): validation input identity and reuse correctness.

## Review and integration scope

The review covered the current main implementation, the donor changes, repository
instructions and recovery/validation contracts, and these planning records:
`docs/planning/PLAN_TO_PORT_PI_TO_RUST.md`,
`docs/planning/PLAN_TO_COMPLETE_PORT.md`, and
`docs/planning/REALITY_CHECK_2026-09-01.md`. Relevant issue history included
`bd-cv653.3.13` (existing file-mutation undo/redo), `bd-gusp4.1` through
`bd-gusp4.11` (broker contract, store, admission, CLI, fault corpus, Doctor,
E2E, stress budgets, and closeout), and `bd-63x3v.6.13.5` (freshness reuse).
The fresh native-provider outcome issue
`bd-provider-live-validation-11-xme9d` remains open. No issue was closed by this
work.

The selected work extends the existing recorder and broker, retaining the
contributor's useful bounded snapshot/checksum/journal design. Review and focused
regressions addressed the following defects and contracts:

- Donor compiler incompatibilities on the pinned Rust/dependency versions.
- Private journal ownership, independent session leases, lock/snapshot/parent
  identity, symlink/hardlink defenses, and preserved corrupt evidence.
- Descriptor-relative atomic snapshot publication and workspace-directory
  synchronization before recording a completed restore.
- Pending edits and partial rollback represented as explicit recovery gaps.
- Interrupted restore refusing journal reopen for manual reconciliation, and
  stale redo history discarded rather than replayed through uncertainty.
- Refusal to perform an edit if its durable pre-write intent cannot be saved.
- Descriptor-relative snapshot reads that cannot persist outside file contents
  after an ancestor is substituted between capture and commit.
- Rejected validation mutations leaving state unchanged, ordered persisted
  timestamps, and exact age/expiry boundaries.
- Actual workspace and immediate executable identity, inherited `PATH` changes,
  and eligibility/ownership filtering before expensive reuse scans.

The recovery integration is an opt-in Unix SDK engine with fail-closed write
callsites. The existing default in-memory lifecycle remains the default. Automatic
CLI/FTUI recovery activation and the full donor PR are outside this integration.
Source review found that the donor's classic startup creates a durable recorder
once while session installation, `/new`, and fork can retain it; explicitly
supplied FTUI recorders can similarly be cloned across session changes. Dynamic
SDK root changes do not rebind a recorder, and a second path with the same session
header can contend with a still-held prior lease. These are source-level findings;
native lifecycle tests have not executed. SDK documentation therefore requires a
fixed root set while attached and a new recorder/session identity after roots
change; revoked roots must not retain old undo authority.

The executed diagnostics below establish specific Linux filesystem and broker
behaviors. They do not establish physical-power-loss durability, coordination
with noncooperating writers, atomic transactions spanning multiple workspace
files, full application lifecycle correctness, or native Windows/macOS
qualification. Validation fingerprints cover the immediate executable and
workspace scope; they do not attest remote workers, transitive toolchains, or
external dependencies, and before/after captures do not freeze a running gate's
workspace.

The journal lease serializes access to one journal. It does not serialize
independent writers to workspace files. For an existing file, the content check
and the later rename or unlink are separate operations, so a last-check race
with another writer remains possible even without `force`. The descriptor and
identity protections do not provide a multi-file workspace transaction.

## Executed validation timestamp regressions

The actual `validation_broker.rs` implementation and its production child module
were compiled as a small Rust library. The existing
`tests/validation_broker_store.rs` integration suite, extended with the new
regressions, exercised the real broker methods and actual persisted JSONL stores.

| Implementation | Tests passed | Tests failed | Exit |
| --- | ---: | ---: | ---: |
| Unmodified main broker, with the new regression cases | 18 | 4 | 101 |
| Timestamp and mutation-atomicity fix | 22 | 0 | 0 |

The failures on main were:

- `rejected_lease_mutations_preserve_state_and_cannot_backdate_evidence`: a rejected mutation changed the lease.
- `invalid_lease_timestamp_order_degrades_persisted_history`: invalid temporal ordering remained usable after JSONL reload.
- `persisted_reusable_evidence_obeys_exact_freshness_boundaries`: future, fractional-age, or expiry boundaries were admitted incorrectly.
- `stale_detection_requires_expiry_and_explicit_reason`: the lease remained unexpired at its exact deadline.

The corrected suite passed all 22 cases, including the existing fault corpus,
stress-budget, admission, source-normalization, and store tests. The corrected
source and test file were frozen during execution:

| File | SHA-256 |
| --- | --- |
| `src/validation_broker.rs` | `42d3d59d93e4656ae897b8606553f1159e7be9e435e3e5671eff49bf22a9e75d` |
| `tests/validation_broker_store.rs` | `ccf43a64934607e485ee90a3910a7c4a1418a09833f140c3615760dff965b5d2` |

The targeted `rustfmt --edition 2024 --check` invocation passed. Diagnostic
Clippy also passed with `-D warnings`, first with the harness default lint set
and then with the repository's exact `[lints.rust]`, `[lints.clippy]`, and
`clippy.toml` configuration. Its target scope remains the small broker library
and actual store integration test, rather than the full application inventory.

## Executed validation input-identity regressions

The final implementation and actual store integration suite were independently
copied into an immutable harness: **31 tests passed, zero failed, and none were
ignored**. This suite includes the earlier timestamp regressions; those counts
are separate execution records, not disjoint test totals. Exact repository-lint
Clippy with `-D warnings`, targeted pinned `rustfmt`, copied-source integrity, and
all 63 resolved registry dependency-version checks passed.

The tests exercise persisted reuse after source/manifest/lockfile edits, renames,
permission changes, executable-byte changes, executable changes outside the
workspace, inherited `PATH` changes in subprocesses, symlink substitution,
oversized sparse inputs, and unavailable or legacy identity. They also check
that active ownership survives unavailable identity, cheap admission authority
and eligibility checks precede expensive scans, and source directories cannot be
declared as excluded output directories. These cases establish correctness of
the tested broker transitions and input captures; they are not large-workspace
latency benchmarks or execution of the validation command itself.

| Executed validation source/test | SHA-256 |
| --- | --- |
| `src/validation_broker.rs` | `227b6b332b899984d9a8125245517c0e080c1ceb6154c2efd10a8ada0c70b4ef` |
| `src/validation_broker/freshness.rs` | `7d2ab54c657ab444b3f8bb02bb38af556f180b831d7ce6e8e4e5ac163bb8ad44` |
| `tests/validation_broker_store.rs` | `74aeb8da216579b643bf0ef386e88afff5c59a14bb5e8da6f25675e9276eae9a` |

The two private freshness unit tests, the full Doctor unit-test inventory, and
the CLI/E2E binaries were not executed by this non-test library/integration
harness. Non-Unix fail-closed test branches were source-reviewed and remain
outside the Linux execution claim. Later fixture annotations and native-only
fixture changes are recorded separately from these executed bytes.

## Recovery compiler evidence

The original donor's `undo.rs`, `undo/journal.rs`, and `file_identity.rs` were
frozen byte-for-byte and compiled with the repository's pinned toolchain and
dependency versions. The corrected harness reached the actual donor code and
failed with exit 101:

- `Result::is_err_or` is unavailable on the pinned toolchain (`journal.rs:180`).
- The locked SHA-256 output type does not implement `LowerHex`; three
  `format!("{:x}", digest)` uses fail to compile. The locked `hybrid-array`
  source has no such implementation, so a dependency feature union does not
  resolve this failure.

| Original donor file | SHA-256 |
| --- | --- |
| `src/undo.rs` | `5535f2bd183f8709df96575c0f6083b594cab9c647491e3b6a0cd6dbda6e2348` |
| `src/undo/journal.rs` | `62c5d63fc53224ca839ed1fa722fc3c8a499b94bf383f8488454808f537f29aa` |
| `src/file_identity.rs` | `ef3b3be538b4a75eb043d3a3af79186c5688efb3d42265751a5f7f90f88a9011` |

An earlier harness setup attempt used `#[path]` in a way that changed child-module
resolution. Its `E0583` failures were harness defects and are excluded from the
product findings above. The working harness uses ordinary Rust module layout.

## Executed recovery regressions

The final functional candidate, before scanner annotation cleanup, was copied
into an immutable ordinary-layout Rust crate.
All **60 Linux module tests executed under the normal parallel runner: 59 passed,
one failed, and none were ignored**. The failure was the existing
`snapshot_rejects_symlinks_and_sockets_without_following_them` test: this execution
environment refuses `AF_UNIX` bind with `EPERM`, before the snapshot assertion.
The exact original main test was independently compiled and executed and failed
at the same bind. Its assertion remains intact.

| Final recovery test group | Passed | Failed |
| --- | ---: | ---: |
| Journal, including the child-process helper | 29 | 0 |
| Core undo | 25 | 1 |
| File identity | 5 | 0 |

The passing cases read actual persisted state after reopening and after child
processes exited without recorder destructors. They cover committed/undone
history, interrupted edit/restore, partial rollback, cross-process lease
exclusion/release, separate sessions, journal/lock/parent replacement, corrupt
checksums/blobs/paths, symlink/hardlink protection, bounds/counter saturation,
failed durable intent, and injected directory-sync failure. The late
ancestor-substitution regressions also pass: reads remain tied to the captured
directory and outside contents never enter the persisted post-edit blob.

| Executed recovery source | SHA-256 |
| --- | --- |
| `src/undo.rs` | `fb270019523df8941bcfbb16eb0738eca7b1567084df186cf14e7ae30c08071a` |
| `src/undo/journal.rs` | `b510e13314ddabe649d39c36e9fbce8d96f2399a3cb3caf9c5db7f0f6e7c3307` |
| `src/file_identity.rs` | `74cc117c0ee61beb3a4ea801a6b084a318dce6731430322a312f90efda4013a9` |

Exact repository-lint Clippy with `-D warnings` and targeted pinned `rustfmt`
both passed. Post-run verification confirmed unchanged copied source hashes and
that all 58 resolved registry package versions occur in the source lockfile.

Earlier failures are retained in the evidence. Explicit private test directories
were necessary because the locked `tempfile` crate creates directories with
`0777` filtered by umask; this environment produced `0755`. Production ownership
and private-mode checks were preserved. A later parallel run observed one
transient `WouldBlock` while reopening after drop; the same compiled binary's
serial run passed that case. This is consistent with a concurrent test child
temporarily inheriting a close-on-exec lease before exec. Expected-success reopen
fixtures now retry only `WouldBlock`, bounded to two seconds; deliberate busy
owner assertions and production lock behavior are unchanged. The final parallel
run above includes that fixture correction.

## Static analysis and final annotated source

The unchanged repository command
`python3 scripts/check_ubs_staged_delta.py --print-ubs-output` passed for the final
recovery index, final validation index, and combined integration index. The
integration scan covered 12 Rust files and 3,790 changed lines. Its whole-file
scanner reported **5 critical, 2,020 warning, and 1,576 informational findings**;
the delta gate reported no parsed warning/critical finding on a changed line and one
informational finding. This is a changed-line gate pass, not a clean whole-file
scan.

Initial failing scans are retained. Their default text output samples locations,
so the first reported locations were not treated as an exhaustive list. Review
included all 13 synchronous journal-test lock sites and the remaining fixture
clone/literal-join sites. Precise line-local comments explain reviewed false
positives: poisoned synchronous test locks must fail tests; basename joins remain
within identity-checked parents and journal root checks; temporary-name retries
are bounded; fixed test cases require owned inputs and failure diagnostics.
Two non-Unix CLI fixture assertions became equivalent explicit test errors.
No scanner implementation, gate script, rule configuration, or production
behavior was changed to obtain the delta result.

The five displayed integrated critical samples were source-reviewed as unchanged
baseline sites: a thinking-level test comparison, the configured SDK RPC
executable and Windows private launcher, and two synthetic redaction fixtures.
The raw output and its sampling limit remain part of the evidence.

After removing explicit UBS comments from both the executed and final copies,
normalizing both with the pinned `rustfmt`, and comparing exact output bytes,
the executed recovery modules, broker Doctor module, and store tests matched.
The full comparison is in `annotation-equivalence.json`; the annotation diff is
retained. The broker/freshness/file-identity production bytes are unchanged.
The native-only CLI fixture delta is separately retained and is not claimed as
executed on another platform.

| Final annotated file | SHA-256 |
| --- | --- |
| `src/undo.rs` | `fee91b030f0a68aac7a8125ffd497b8da2c9b3d6ef5a7a4ece476d2e693fb69a` |
| `src/undo/journal.rs` | `c3b77a7f98fa4b7e24db70ab26925d08a21463687598e932bbc7520f1d82a95e` |
| `src/validation_broker/doctor.rs` | `81dc709d01e53ab42e0ac939511738328cb1545670aa8576bcf747ab56a0d70f` |
| `tests/validation_broker_store.rs` | `4882f1c7a49b5f3a4b38cf9a6bc7ac828c2eab43bd88a3e2d48bd5fa33d2bfe6` |

## Toolchain and diagnostic scope

The checked-in `rust-toolchain.toml` selects `nightly-2026-08-31`. The tools used
reported:

- `rustc 1.100.0-nightly (908501772 2026-08-30)`.
- `cargo 1.100.0-nightly (e8cb624d5 2026-08-22)`.
- `rustfmt 1.10.0-nightly (9085017724 2026-08-30)`.

Each final diagnostic manifest pins its direct dependencies to versions in the
main `Cargo.lock`, and its lockfile is seeded from that file. An explicit
comparison found no resolved registry dependency version absent from the source
lockfile. The initial source lockfile SHA-256 was
`265dc436906f1ac109edf4ab9878a40d66e0e742b95daf14ea29899d757e59e5`.

The recovery manifest uses `base64=0.23.1`, `dirs=7.0.0`, `fs4=1.1.0`,
`rustix=1.1.5` (fs/process), `serde=1.0.229` (derive/rc),
`serde_json=1.0.151` (raw_value), `sha2=0.11.0`, `tempfile=3.27.0`,
`tracing=0.1.44`, and `uuid=1.27.0` (v4). The validation manifest uses the
applicable shared dependencies plus `chrono=0.4.45` (serde) and
`thiserror=2.0.21`. Dependency feature unions are deliberately smaller than those
of the full application.

The harnesses preserve the production module files without rewriting their
logic. Two explicit boundaries keep them small:

- Recovery includes a minimal `Config` adapter containing the exact production
  `global_dir`, `env_lookup`, and `global_dir_from_env` bodies. This is not the
  complete production configuration type. The recovery tests use explicit
  journal paths; full configuration/session/UI integration is outside this
  diagnostic scope.
- Validation includes only the `Validation`, `Io`, and `Json` error variants
  needed by the broker, with the same messages, boxing, constructor, and
  conversions as the production error type. The broker is compiled as a
  non-test library; the separate integration target runs its actual public APIs.
  The production doctor's unrelated unit-test dependencies are not substituted
  or claimed as executed.

These Linux module diagnostics do not establish full Pi compilation, CLI/FTUI
integration, native provider behavior, Windows/macOS portability, physical power
loss behavior, or DSR acceptance.

## Authoritative DSR attempts

The unchanged repository recipe was actually invoked through DSR, with no check
removed and no `--skip-checks`:

```sh
RCH_CANONICAL_PROJECT_ROOT=/workspace/scratch/b95c142b8b6d \
DSR_REPOS_FILE=.dsr/repos.yaml dsr --json --non-interactive quality \
  --tool pi_agent_rust --work-dir /workspace/scratch/b95c142b8b6d/pi_agent_rust
```

DSR source revision was `07586ae05668290353a210a421b254c4c362ff43` (reported
version 0.2.3). The installed RCH release was 2.1.35, verified against its release
SHA-256 sidecar. The baseline receipt recorded identical clean source snapshots
before and after the run, aggregate exit 1, **1 of 7 checks passed**, and a
742,532 ms duration.

| Check | Actual baseline outcome |
| --- | --- |
| Formatting | Exit 1; existing formatting differences across main |
| Rust all-target compiler check | Exit 103 before Cargo execution; RCH required remote execution and refused the noncanonical checkout path |
| Rust all-target Clippy | Same fail-closed remote-routing refusal |
| Rust all-target tests, including integration targets | Same fail-closed remote-routing refusal; the tests did not execute |
| Installer regression script | 71 passed, 2 failed, 1 skipped; missing sparse-checkout bundled skill and completion-generation timeout test failure |
| Module reachability | Exit 1; `web_remote` had no non-test consumer (153 declared, 149 reachable, 3 allowlisted, 1 unreachable) |
| Fixture read patience | Exit 0 |

The final run used the complete checkout and the supported
`RCH_CANONICAL_PROJECT_ROOT` setting shown above. It actually executed all seven
recipe commands at `667c9ba8b92aefdace99d9d3bc7472007031a6e5` and exited 1:
**2 of 7 checks passed, 5 failed, in 204,389 ms**. The
[final receipt](final-dsr-receipt.json) records identical clean source snapshots
before and after, including the unchanged Cargo lock hash. The separate
before/after source records also show an empty `git status --porcelain`.

| Check | Final observed outcome at `667c9ba8` |
| --- | --- |
| Formatting | Exit 1; differences in 55 files. One newly introduced hashline-edit error-conversion formatting hunk was identified and corrected afterward; the remaining displayed differences are outside the selected source changes. |
| Rust all-target compiler check | Exit 103 before Cargo; RCH daemon unavailable and remote-required mode refused local fallback. |
| Rust all-target Clippy | Exit 103 for the same fail-closed daemon/routing condition; Cargo Clippy did not execute. |
| Rust all-target tests, including integration targets | Exit 103 for the same condition; the full application/integration inventory did not execute. |
| Installer regression script | Exit 0: **73 passed, 0 failed, 1 skipped**. Both previously failing installer cases passed. |
| Module reachability | Exit 1: the same `web_remote` finding; 153 declared, 149 reachable, 3 allowlisted, 1 unreachable. |
| Fixture read patience | Exit 0. |

`RCH_REQUIRE_REMOTE=1` was preserved. RCH's independently queried worker inventory
contained zero configured workers. The actual Rust-lane refusal was an
unavailable daemon; no remote job or local Cargo execution is inferred from
those command attempts.

An earlier final-run entry attempt returned configuration error 4 with zero
checks executed because the verified local `yq` binary lacked execute permission.
Its official release SHA-256 was rechecked, execute permission restored, and the
unchanged YAML recipe parsed successfully before the seven-check run above.
That infrastructure entry failure and its repair are separate from the final
qualification results; no check or gate policy was weakened.

After the run, the one introduced formatting hunk in `src/tools.rs` was corrected
without changing the expression. The
[format-equivalence record](final-tools-format-equivalence.json) confirms that
the complete pre/post file normalizes to byte-identical output under the pinned
formatter. No full DSR rerun is claimed for that formatting-only cleanup. The
unchanged staged UBS command subsequently passed for that one-file, four-line
formatting delta with zero parsed changed-line findings; its whole-file totals
remained 3 critical, 955 warning, and 862 informational findings. The completed
retry log is retained; an earlier attempt returned no result and is not counted.
Later unrelated extension and compaction changes on main were preserved; they
are outside this DSR receipt, which remains explicitly scoped to `667c9ba8`.

## Native provider qualification attempt

The checked-in native-provider entrypoint was invoked against its ordinary
default binary path, then repeated at the same clean integrated source
`667c9ba8b92aefdace99d9d3bc7472007031a6e5` used for the final DSR run:

```sh
python3 scripts/perf/run_provider_live_e2e.py \
  --out /workspace/scratch/b95c142b8b6d/qualification-tools/logs/final-provider-live-preflight.json
```

It exited 1 with `verdict=binary_missing`; all 11 providers were explicitly
recorded as `skipped_no_binary`. No unrelated prebuilt binary was substituted.
This is a recorded blocked qualification attempt, not passing native-provider
evidence. The final [preflight JSON](provider-live-preflight.json) and
[source-head record](native-provider-source-head.txt) are retained.

## Commands and retained diagnostics

The actual module commands used the pinned toolchain directory on `PATH`, a
dedicated Cargo home/target directory, and two build jobs. Relevant invocations:

```sh
cargo test --offline --manifest-path donor-harness/Cargo.toml --no-run
cargo test --offline --manifest-path validation-main-harness/Cargo.toml \
  --test validation_broker_store
cargo test --offline --manifest-path validation-harness/Cargo.toml \
  --test validation_broker_store
cargo clippy --offline --manifest-path validation-harness/Cargo.toml \
  --test validation_broker_store -- -D warnings
cargo test --offline --manifest-path recovery-final-independent-harness/Cargo.toml --lib
cargo clippy --offline --manifest-path recovery-final-independent-harness/Cargo.toml \
  --lib --tests -- -D warnings
cargo test --offline --manifest-path validation-final-independent-harness/Cargo.toml \
  --test validation_broker_store
cargo clippy --offline --manifest-path validation-final-independent-harness/Cargo.toml \
  --test validation_broker_store -- -D warnings
```

The curated logs, manifests, source inventories, comparisons, and receipts are
adjacent to this README. `evidence-inventory.json` lists the frozen artifact set
and SHA-256 values. Original run logs were retained at
`/workspace/scratch/b95c142b8b6d/qualification-tools/logs`; the large whole-project
formatting output is represented by its receipt hash and precise attribution
above rather than duplicated here.

The companion [prepare_core_diagnostics.py](prepare_core_diagnostics.py) recreates an equivalent small
harness from a checkout without changing that checkout. Keep the checkout fixed
during execution and retain the generated source-hash inventory. The script
does not turn those module results into a replacement for the DSR gate.

```sh
# From the repository root, with the pinned nightly toolchain on PATH.
# Choose new output directories; existing directories are refused.
python3 docs/evidence/pr-283/prepare_core_diagnostics.py prepare . \
  /tmp/pi-undo-diagnostic --surface undo
cargo test --offline --manifest-path /tmp/pi-undo-diagnostic/Cargo.toml --lib
cargo clippy --offline --manifest-path /tmp/pi-undo-diagnostic/Cargo.toml \
  --lib --tests -- -D warnings
python3 docs/evidence/pr-283/prepare_core_diagnostics.py verify /tmp/pi-undo-diagnostic

python3 docs/evidence/pr-283/prepare_core_diagnostics.py prepare . \
  /tmp/pi-validation-diagnostic --surface validation
cargo test --offline --manifest-path /tmp/pi-validation-diagnostic/Cargo.toml \
  --test validation_broker_store
python3 docs/evidence/pr-283/prepare_core_diagnostics.py verify /tmp/pi-validation-diagnostic
```

The helper copies production module/test bytes and records their hashes. Its
verification checks the copied source lock and toolchain hashes before trusting
those provenance anchors, then checks source integrity and dependency-version
membership. Integration test names are restricted to simple basenames. Validation
corpus paths remain links to the fixed checkout. Offline commands require the
relevant dependencies to be present in Cargo's cache. `verify` does not execute
Cargo and, by itself, is not a compilation or test result.
