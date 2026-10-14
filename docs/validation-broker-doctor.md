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

Regression tests live beside the adapter in `src/validation_broker/doctor.rs`.
They cover native serialization, scope separation, structured stop signals,
malformed reports, corrupt JSONL slot stores, and guarded cache reuse.
`tests/doctor_broker_native.rs` additionally passes real CLI JSON from isolated
consumer and explicitly requested swarm runs through the public normalizer.
