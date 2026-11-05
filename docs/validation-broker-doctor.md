# Doctor reports in the validation broker

`pi doctor` checks ordinary installation health. Its default consumer report is
not evidence that a host is ready for development swarms or heavyweight builds.
The validation broker does not change that consumer report or invent failures
for an otherwise healthy installation.

The public Rust API `pi::validation_broker::normalize_doctor_json` accepts native
Doctor JSON as well as the existing `checks` / `preflight.checks` input formats.
Capture the specialized report explicitly:

```sh
pi doctor --only swarm --format json
```

Retain the report even when the command exits with status 1. A nonzero exit is a
diagnostic outcome, not permission to discard its failures. The broker CLI's
`--inputs` argument still takes a complete normalized broker input snapshot;
this API addition does not make raw doctor output a complete CLI input file.

## Native report interpretation

The adapter retains each finding's category, title, severity and optional detail.
It checks the report's summary counters and overall status against the findings.
Missing fields, unknown severities, contradictory counters, and malformed native
reports make the doctor source degraded. If `findings` is present, malformed
native data never falls back to a separate success-only `checks` array.

A native report needs exactly one swarm-category finding for each schema:

- `pi.doctor.swarm_resource_preflight.v1`
- `pi.doctor.swarm_admission.v1`

Missing or duplicate required evidence, a non-ready resource preflight,
unavailable resource sources, or a missing/failed resource plan degrades the
source. Nonempty resource `critical_failures`, any finding with severity `fail`,
and live-admission actions `deny` or `backpressure` block admission. Backpressure
remains a stop signal even when the corresponding doctor finding is a warning.
Ordinary informational and warning findings do not become failures by themselves.
The existing legacy check formats retain their original strict status semantics.

## Storage and resource-plan evidence

Empty error lists and `plan_error: null` are not enough to establish readiness.
The native resource finding must include `recommended_budgets` with positive
integer agent, tool, extension-lane, queue-depth, and RSS budgets. Its remote
verification fanout must be an unsigned integer; zero remains valid for a
disabled remote lane.

The `tmpfs_headroom` payload must contain exactly one `CARGO_TARGET_DIR` row and
one `TMPDIR` row, a positive integer `warn_available_kb`, and an absolute
`expected_root`. Each row must report `exists`, `under_expected_root`, and
`ready` as true, a null `problem`, and an unsigned `available_kb` at least as
large as the reported minimum. Measurements are compared in KiB without a
lossy float or byte conversion. Unknown measurements, missing or duplicate
rows, and malformed fields cannot authorize new work or cached-result reuse.
An explicit storage hazard or a below-minimum measurement remains blocking
even if the report incorrectly carries a `pass` severity and empty failure list.

Reported POSIX paths are compared by components, including on non-POSIX broker
hosts. A shared string prefix is not containment; relative paths, parent
traversal and NUL bytes are rejected. Redundant separators and `.` components
are accepted. This is a consistency check against the report's declared root
and minimum, not a second host probe or an independent storage policy. It does
not resolve remote symlinks, authenticate the report, prove freshness, or bind
the report to a particular future command's paths. Callers still own those
provenance and request-equivalence checks. Consumer reports are unchanged.

## Reuse cannot override failed authority

Required RCH local-fallback denial, degraded authoritative sources, and doctor
failures are evaluated before choosing a reusable validation result. A damaged
later slot-store record cannot make an earlier valid cached record trustworthy.
An explicitly degraded or unavailable source remains blocking even when its
explanatory reason list is empty; the same rule applies to degraded slot stores.

Healthy, equivalent cached results can still be reused when RCH capacity is busy,
because reuse does not launch another build. Agent Mail availability remains an
advisory input to confidence rather than a new consumer-installation requirement.
No doctor diagnostic severity, resource threshold, or consumer category selection
is changed by this adapter.

Normalization and planning do not execute checks or repair files. Callers remain
responsible for capturing fresh inputs with correct provenance and for retaining
all required diagnostic findings. Structural validation is not cryptographic
artifact authentication, a hostile-input sandbox, native quality acceptance, or
permission to skip DSR and the repository's required validation.

## Workspace and executable identity for reusable results

On Unix, a newly acquired lease captures a bounded digest of workspace file
contents, directory names, permission modes and ownership, plus the immediate
executable's canonical path, bytes, mode and ownership. Completion checks those
inputs again before marking the lease reusable. A later admission request must
match both the declared request metadata and the captured inputs. Old leases
without input identity remain valid ownership records but cannot supply reusable
results. Unavailable inputs likewise preserve the active owner's slot and require
fresh validation instead of enabling duplicate work.

Capture uses directory handles and relative nofollow opens, verifies file and
directory identity while reading, and refuses symlinks, special files, unreadable
inputs, non-UTF-8 input names and observed concurrent changes. Limits are 100,000
workspace entries, 512 MiB of workspace bytes, 128 directory levels and 256 MiB
for the executable. Only the workspace's root `.git` entry and the exact declared
target/tmp subtrees are excluded. In-workspace output paths must be under a
recognized output root such as `target`, `tmp`, `build`, `dist`, `out` or
`.pi-validation`; ordinary source trees cannot be excluded. Nested directories
merely named `target` are still inputs unless explicitly declared as an allowed
output path. Store files and generated logs should live in excluded output
directories or outside the workspace.

Executable resolution checks effective execute access and honors an explicit
request `PATH`. When the request inherits `PATH`, completion also checks the
current inherited value; switching tools during a validation run cannot be hidden
by replaying the previous search path. The request's environment fingerprint
continues to cover the explicitly supplied environment only. Capture does not
attest shell interpreters, dynamically loaded libraries, remote workers, external
dependencies, ACLs, extended attributes or other inherited environment values.
Callers must supply the relevant environment and retain the actual DSR evidence.

The scans bracket a validation run; they do not freeze a filesystem transaction
or prove that a transient change was never made and restored between captures.
Freshness therefore remains one condition for advisory reuse, alongside the
authoritative gate result and provenance. Windows and other non-Unix platforms
currently fail closed for input capture pending an equivalent qualified native
implementation; their lease ownership and admission safeguards still operate.

Planning scans inputs only when a temporally eligible reusable result has matching
declared metadata and the required authorities permit reuse. Known authority
failures and stores containing only unrelated, active, terminal or expired leases
do not trigger a workspace scan. A valid cached result retains its existing
precedence over an equivalent active lease. This avoids unnecessary reads without
introducing a metadata-only digest cache or claiming a measured performance gain.

Regression tests live beside the adapter in `src/validation_broker/doctor.rs`.
They cover native serialization, scope separation, structured stop signals,
malformed reports, corrupt JSONL slot stores, and guarded cache reuse.
Workspace, executable, persisted timestamp, legacy ownership and scan-scope
regressions live in `tests/validation_broker_store.rs`, including subprocess
completion after a changed inherited `PATH`.
`tests/doctor_broker_native.rs` additionally passes real CLI JSON from isolated
consumer and explicitly requested swarm runs through the public normalizer.
