# Native security review

`security_scan` retains its existing local source operations: `plan`, `run`,
`disposition`, and `compare`. The source-rule engine is now in
`src/security_scan/source.rs`; its implementation and tests are retained intact.

## Inspect locked dependencies without a network request

```json
{"op":"dependency_plan"}
{"op":"dependency_plan","paths":["Cargo.lock","web/package-lock.json"]}
```

Empty dependency paths inspect **root `Cargo.lock` and `package-lock.json` only**.
Nested workspaces must supply explicit lockfile paths. A dependency path is a
lockfile, not a directory. No manifest resolver, package manager, installation,
source execution, or vulnerability-service request runs during this operation.

The result contains exact public-registry package/version pairs, all their
lockfile locations, lockfile SHA-256 digests, and explicit exclusions. Identical
queries are deduplicated across files and nested npm installations without
losing their locations. Multiple locked versions remain distinct. Development
and optional packages present in a supported lockfile are included.

Supported inputs are Cargo lockfile versions 1–4 (including older files without
an explicit version) and npm `package-lock.json`/`npm-shrinkwrap.json` versions
2–3. npm v1 and unknown versions fail rather than yielding an empty inventory.
The root npm project entry is not treated as its own dependency. npm aliases
use their recorded package name; scoped and nested packages retain identity.

Only recorded public crates.io sources and credential-free HTTPS
`registry.npmjs.org` tarball origins qualify. Local/workspace, linked, git,
private-registry, missing-origin, and unresolved-version entries are exclusions,
not inferred public package names. Exclusions do not include source URLs or
credentials. **An inventory is not a security verdict or an installed-code or
reachability analysis.** A missing lockfile or parse failure is an error.

Reads are bounded to 32 lockfiles, 4 MiB per file, 16 MiB combined, 20,000 entries,
and 10,000 unique public package/version pairs. Descriptor-relative reads reject
symlinks and parent traversal; the currently implemented filesystem backend
requires supported Unix descriptor APIs. Other platforms fail explicitly rather
than quietly weakening that confinement. Filesystem calls are not preemptible
hard real-time operations. This is not a sandbox for other security tool ops.

The model-facing offline preview includes at most 50 packages, two locations
per package and 20 exclusions, with explicit total counts and truncation flags.
The public SDK `dependencies::inventory` function retains the complete bounded
offline inventory. Audit SARIF also retains that full inventory. Large lockfiles
therefore do not become an unbounded model-facing message.

## Validation

The implementation adds pure parser/identity/limit tests and real temporary-file
confinement tests. Existing source scanner tests are retained unchanged. The
required quality entry point is `dsr quality --tool pi_agent_rust`; no direct
Cargo or GitHub Actions lane substitutes for it. No Rust test or compile pass
is claimed until that gate has run.

## Audit public dependency versions against OSV

```json
{"op":"audit_dependencies"}
{"op":"audit_dependencies","paths":["Cargo.lock","web/package-lock.json"],"sarifOut":"dependency-audit.sarif","timeoutMs":60000}
```

This operation derives the same inventory, then uses the native HTTP client to
query `https://api.osv.dev/v1/querybatch`. **Eligible public-registry package
names, ecosystems and exact versions leave the machine.** Subsequent requests
send matched advisory IDs to retrieve their details. Source code, lockfile
contents, lockfile paths, resolved source URLs and excluded private/local/git
package identities are not sent. There is no package-manager subprocess,
installation, manifest resolution, automatic remediation or authentication key.

`dependency_plan` is the offline preview. `audit_dependencies` is an explicit
network operation under the host's normal tool-approval policy. A trusted SDK
host may configure an OSV-compatible service with
`SecurityScanTool::with_osv_base_url`; HTTPS is required except for literal
loopback HTTP fixture endpoints. This is not a model-facing endpoint argument.
`with_osv_client` retains a host-supplied HTTP/VCR configuration. Custom services
receive the package coordinates and must be trusted accordingly.

### Completion is explicit

Results distinguish package lookup completion from advisory-detail completion:

- `complete`: every eligible package/version query finished, including all pages,
  and the selected metadata requests completed.
- `complete_with_metadata_gaps`: queries finished and matched IDs remain visible,
  but some advisory details are unavailable or exceeded their separate budget.
- `incomplete`: a query failed, pagination was malformed/cyclic, or a query budget
  was exhausted. The tool sets `isError: true`; collected matches are partial.
- `not_checked`: no eligible public-registry versions were found. No OSV request
  is sent and the tool sets `isError: true` rather than reporting an empty audit.

`queryComplete` applies **only to eligible packages in selected lockfiles**.
Excluded entries are never counted as checked. The report includes exclusion
counts, a bounded preview, completed-query counts, the lockfile digests and the
observation timestamp. The timestamp is when this audit started, not an atomic
snapshot time for the changing upstream advisory database. A request deadline
or cancellation returns an error; it cannot certify that any unfinished query
was clean. There is no cached or offline-clean fallback and no automatic retry.

Queries run in batches of at most 100. Pagination is tracked separately for each
package; an empty page that carries a token is not a completed empty result.
Response arrays must match their request batch, and an entire page batch is
validated before completion states change. At most 128 batch requests, eight
pages per package, and 2,000 package/advisory matches are admitted. Batch responses
are bounded to 4 MiB. A short, oversized, malformed or failed response produces
explicit incompleteness, not a reduced count represented as a completed scan.

After queries complete, at most 128 distinct advisory IDs are hydrated, with a
512 KiB response cap per advisory. IDs remain visible when details cannot be
loaded. The tool preserves bounded summaries, aliases and withdrawn status.
`fixedVersionBoundaries` comes from version ranges for the **same ecosystem and
package**, excluding Git hashes. It is **not** an automatically chosen safe
upgrade: separate release branches may have different boundaries, and other
advisories may affect those versions. Aliases and fixed-boundary lists include
at most 16 entries each and carry truncation flags. Advisory IDs that alias the
same underlying issue are not automatically merged; counts mean package/advisory
matches, not necessarily unique real-world vulnerabilities. Withdrawn records
stay visible and are counted separately.

### Export and scope

The optional `sarifOut` for dependency audits must be a **new root-level filename
ending `.sarif`**. No dependency report is written by default. Existing files,
including symlinks and concurrently created destinations, are never overwritten.
Publication stages a private, bounded file, syncs its content, checks cancellation
and the deadline, and creates the final name without replacement. Staging names
are cleaned up on failure. Filesystem directory-entry crash durability is not
guaranteed. The export is limited to 16 MiB and requires the same supported Unix
descriptor backend as inventory reads. Untrusted code running as the same OS user
is outside this confinement model.

The tool result previews at most 50 matches. SARIF contains every admitted match,
the complete bounded lockfile inventory and per-package query-completion flags.
At most four locations are repeated on each SARIF result; the full location list
is retained once in the inventory and linked by package index. It does not invent
source line numbers for lockfile entries. Findings use warning-level SARIF for
review, or note-level for withdrawn records; those levels are not CVSS scores.
A failed query marks SARIF `executionSuccessful: false`.

Dependency fingerprints cover ecosystem, name, version and advisory ID rather
than line numbers. They occupy the `pi/dependency/v1` namespace. Existing source
`compare` and `disposition` operations continue to operate on source-rule scans;
do not pass dependency SARIF to source `compare`. Use the offline
[`compare_dependencies` operation](dependency-review.md) for saved dependency
audits. Its companion `disposition_dependencies` operation records hash-bound
review decisions in a new SARIF artifact without removing findings, rewriting
the source audit, or installing suppression policy for future audits. A known-advisory
match is not proof of exploitability, and absence of matches is not proof that
code has no vulnerabilities. Installed dependencies, build reachability, patched
forks and lockfile provenance require separate analysis.

`timeoutMs` defaults to 60 seconds, capped at 120 seconds, and covers inventory
and the complete network exchange rather than restarting for each page. Blocking
filesystem calls and synchronous report serialization are not preemptible;
publication rechecks the deadline before making the final name visible.

### Implementation-session verification

Native loopback protocol tests were authored for page-token handling, exact
request privacy, advisory hydration, real SARIF export, HTTP failure and missing
metadata. Pure tests cover malformed/cyclic pagination, match budgets, identity,
version-boundary interpretation, endpoint policy and incomplete reports. Export
tests exercise real temporary files, existing destinations, symlinks and elapsed
deadlines. These are canned OSV protocol responses, not evidence of live OSV
availability or accuracy. **The Rust tests were not executed.**

`dsr quality --tool pi_agent_rust` returned `dsr: command not found` (127) in the
implementation runtime. No Rust compiler was installed, and compilation,
formatting, Clippy and DSR remain unverified. No Bead is closed by these source
changes and no release/cross-platform or live-service validation is claimed.

Protocol references: [OSV batch queries and pagination](https://google.github.io/osv.dev/post-v1-querybatch/)
and [OSV advisory details](https://google.github.io/osv.dev/get-v1-vulns/).
