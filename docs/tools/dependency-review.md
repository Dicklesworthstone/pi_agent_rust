# Compare saved dependency audits

`security_scan` exposes `compare_dependencies` alongside `audit_dependencies`.
The comparison reads two saved native dependency-audit SARIF files; it does not
query OSV, run a package manager, modify lockfiles, or rescan source code.

```json
{"op":"compare_dependencies","baseline":"before.sarif","current":"after.sarif"}
{"op":"compare_dependencies","baseline":"before.sarif","current":"after.sarif","offset":20,"limit":20}
```

Create each input with `audit_dependencies` and its explicit `sarifOut` option.
Use the same selected lockfile paths on both audits. The current audit must not
predate the baseline. Names must be root-level `.sarif` filenames, as with audit
exports; reports in other directories must be copied deliberately by the host.
Both files are opened relative to one pinned workspace descriptor. Symlinks,
non-regular files, path escapes and files larger than 16 MiB are refused. The
reader requires the supported Unix descriptor backend; unsupported platforms
fail explicitly. I/O capability and owner cancellation are checked around the
blocking worker. Filesystem calls themselves are not hard-real-time operations.

## What a change means

Findings match by exact ecosystem, package name, version and advisory ID, using
recomputed `pi/dependency/v1` fingerprints. Different versions and advisory
aliases are not merged. States are:

| State | Meaning |
|---|---|
| `persistent` | The exact finding appears in both reports. |
| `newly_observed` | The baseline queried this exact package/version completely without this match; the current report contains it. |
| `new_package` | The current finding's exact package/version was absent from the baseline's eligible inventory. |
| `no_longer_observed` | The current query for this exact package/version completed without the former match. |
| `package_not_in_current_inventory` | The exact package/version is no longer eligible/present; removal, upgrade or exclusion is not proof of a fix. |
| `unknown` | The report lacking this finding did not complete that package/version's query. `reason` identifies which side was incomplete. |

**No state certifies a vulnerability as fixed or a project as clean.** OSV data
can change between queries, exclusions can change, and package presence is not
installed-code reachability. The audit observation time is a start timestamp,
not a frozen advisory-database revision. These local reports are not signed or
authenticated attestations; schema and identity validation do not prove their
origin or truth. Compare reports from the same intended project/service.

`queryComplete` is true only when both audits completed every eligible query.
Otherwise `isError` is true while all available comparisons remain accessible.
A failure for another package does not invalidate a completed package's local
comparison. Metadata gaps remain distinct from query gaps. Withdrawn and
SARIF-suppressed findings stay in the counts and pages; they are not treated as
absent. Persistent entries separately report advisory-metadata and installation-
location changes. Exclusion and lockfile-digest changes are also explicit.

## Paging and input validation

The default page contains 20 findings; `limit` accepts 1–50. `counts` and `total`
always describe the entire comparison, including findings outside the page.
Ordering is deterministic by ecosystem/name/version/advisory. Follow
`nextOffset` until it is null. Both input SHA-256 hashes are returned on every
page: restart paging if either changes. A page exceeding the shared tool-output
budget is refused with a request to reduce `limit`; it is not silently cut off.

Only native dependency SARIF v2.1.0 with audit/inventory v1 schemas is accepted.
The importer verifies package-index links, unique identities, hashes, lockfile
scope, timestamps, per-query booleans, aggregate completion, metadata availability
and invocation success. Source-scan SARIF, multi-run reports, duplicate known
JSON fields and contradictory records fail rather than producing empty baselines.
Unneeded advisory prose and arbitrary SARIF metadata are not echoed into model
context. Existing source `compare` and `disposition` operations are unchanged.

## Validation status

Tests were authored for Cargo/npm changes, version separation, partial queries,
withdrawn/suppressed records, metadata gaps, scope/time refusals, tampered and
duplicate identities, bounded paging, confinement and I/O denial. A public-tool
loopback test produces actual audit exports through the existing HTTP client,
then compares them after its fixture server exits, including an outage artifact.
The fixtures do not test live OSV accuracy.

`dsr quality --tool pi_agent_rust` was attempted in the implementation environment
and returned command not found (exit 127). Rust compilation, formatting, Clippy
and all Rust tests remain unexecuted. No DSR, release, or Bead closure is claimed.
