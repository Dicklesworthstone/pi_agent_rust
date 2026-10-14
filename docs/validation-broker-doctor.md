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
