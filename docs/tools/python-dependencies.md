# Python lockfile dependency inventory

`security_scan` now accepts `uv.lock` and `poetry.lock` for offline
`dependency_plan`, alongside Cargo and npm lockfiles. Empty `paths` selects
root `Cargo.lock`, `package-lock.json`, `uv.lock`, and `poetry.lock` when present.
Supply explicit relative paths for nested projects or a narrower inventory.

```json
{"op":"dependency_plan","paths":["uv.lock"]}
{"op":"dependency_plan","paths":["backend/poetry.lock","web/package-lock.json"]}
```

Supported schemas are uv version 1 (additive `revision` values are accepted)
and Poetry lock versions 1.1, 2.0, and 2.1. Unknown schemas and malformed TOML
are errors, not successful empty inventories. There is no package manager,
Python interpreter, installation, resolution, download, or code execution.
`requirements.txt`, manifests and `pylock.toml` are not supported by this reader.

## Public identity and privacy

For uv, only a sole `source.registry` pointing to credential-free HTTPS
`pypi.org/simple` qualifies. Missing sources, private indexes, direct wheel/sdist
URLs, Git, editable, virtual and path sources are explicit exclusions. An
ambiguous source never becomes public by choosing a preferred field.

Poetry's absent `package.source` denotes its default PyPI source. An explicit
empty or unfamiliar source does not. Explicit `legacy` sources qualify only
when their URL is the same public PyPI endpoint; direct URLs, directories,
files, Git and private indexes do not. Editable/develop packages are excluded.
This trusts the lockfile's recorded provenance; it does not authenticate it.

Names follow Python package normalization: lowercase, with each run of periods,
underscores or hyphens collapsed to one hyphen. Public PEP 440 versions retain
epochs, release segments, pre-release, post-release and development suffixes.
Alternate spellings and numeric padding are normalized. Release component count
is preserved (`1.0` and `1.0.0` remain separately recorded versions). There is
no SemVer conversion, ordering, safe-upgrade selection or range resolution.
Local versions (`+...`) are excluded rather than stripping a patched fork's
identity. Invalid names and non-concrete versions also remain exclusions.

All locked variants are included regardless of Python/platform markers,
optional flags, extras or dependency groups. This is an inventory of the whole
selected lockfile, not a claim about the current interpreter or installed code.
Repeated normalized package/version pairs are deduplicated without losing table
locations. `packagePath: "package[3]"` identifies the zero-based TOML package
record, not a filesystem or installation path. Exclusion records contain only
that ordinal, the selected lockfile path and a fixed reason: private names,
source URLs, credentials, wheel URLs and marker text are not repeated there.

The existing shared limits remain: 32 files, 4 MiB per file, 16 MiB total,
20,000 entries including excluded rows, and 10,000 public package/version pairs.
Descriptor-confined reads retain the existing supported-Unix requirement and
reject symlink components. Offline inventory performs no vulnerability lookup.

## Validation status

Parser/identity and real-filesystem regression tests are included. They have
not run in the implementation environment: `dsr quality --tool pi_agent_rust`
returns `dsr: command not found` (127). No Rust compilation, formatter, Clippy,
test, release or live-service result is claimed.

References: [uv lockfile versioning](https://docs.astral.sh/uv/concepts/resolution/),
[Poetry lockfile implementation](https://github.com/python-poetry/poetry/blob/main/src/poetry/packages/locker.py),
[Python name normalization](https://packaging.python.org/en/latest/specifications/name-normalization/),
and [Python version identifiers](https://packaging.python.org/en/latest/specifications/version-specifiers/).
