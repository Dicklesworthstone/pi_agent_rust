# Extension Registry and Local Index

This document defines Pi's *offline-first* extension discovery registry and the local on-disk index
used by user-facing discovery commands (planned: `pi search`, `pi info`, `pi update-index`).

Key goals:
- No central server: data comes from public backends (npm + GitHub) plus curated seed data shipped
  with the Pi binary.
- Offline-first: discovery must work on first run with zero network access.
- Fail-open: network failures never break search or install; Pi always falls back to seed/cached data.

## Concepts

### Index

The **index** is a single JSON file containing a list of extension descriptors + metadata, suitable
for local (client-side) search.

Default location:
- `~/.pi/agent/extension-index.json`

### Seed Index (Bundled)

Pi ships a **seed index** embedded in the binary at compile time. It provides:
- Immediate results on first run (no "please wait")
- A useful offline experience
- A stable fallback if a refresh fails or cache becomes corrupt

The seed index is updated with each Pi release.

### Cache (User Machine)

Pi writes a cached index to disk after refreshing from remote sources. Pi prefers the cache when it
is valid, but will transparently fall back to seed on errors.

## Data Sources

Pi merges multiple sources into a single index:

1. **npm registry search**
   - Find packages with keywords like `pi-extension` / `pi-agent-extension`.
   - Populate: package name, version, description, repository URL, last publish date.

2. **GitHub search**
   - Search repositories by topic (e.g. `topic:pi-extension`) and/or query terms.
   - Populate: repo name, description, stars, last updated, repo URL.

3. **Curated manifest (Pi-maintained)**
   - A static list of known-good extensions (high-signal, tested, pinned).
   - This is the primary content for the seed index shipped in the binary.

## Schema: `pi.ext.index.v1`

`extension-index.json` uses a versioned schema so future changes are explicit and migratable.

Example:

```json
{
  "schema": "pi.ext.index.v1",
  "version": 1,
  "generatedAt": "2026-02-06T08:00:00Z",
  "lastRefreshedAt": "2026-02-06T08:00:00Z",
  "entries": [
    {
      "id": "npm/checkpoint-pi",
      "name": "checkpoint-pi",
      "description": "Checkpoint and restore your Pi sessions",
      "tags": ["npm", "extension"],
      "license": "MIT",
      "source": {
        "type": "npm",
        "package": "checkpoint-pi",
        "version": "1.2.3",
        "url": "https://www.npmjs.com/package/checkpoint-pi"
      },
      "installSource": "npm:checkpoint-pi@1.2.3"
    }
  ]
}
```

Field notes:
- `id`: globally unique identifier within the index (stable key).
- `name`: primary display identifier (often npm package name or repo name).
- `installSource`: optional string compatible with Pi package manager (e.g. `npm:pkg@ver`,
  `git:https://github.com/org/repo@ref`). If absent, the entry is discoverable but not directly
  install-resolvable by id.

## Refresh Strategy

### When To Refresh

- Auto-refresh when the cache is missing or older than 24 hours.
- Manual refresh via a dedicated command (planned): `pi update-index`.

### Failure Semantics (Critical)

Refreshing is *best-effort*:
- Network errors MUST NOT fail discovery commands.
- If refresh fails, Pi continues using the cached index (if present) or the seed index.

### Corruption Handling

If the cached file cannot be parsed:
- Warn (non-fatal).
- Fall back to seed index.
- Overwrite cache on next successful refresh.

## Search Algorithm (Client-Side)

Search is computed locally over cached data:
- Tokenize the user query on whitespace.
- Weighted scoring:
  - Name matches: highest weight
  - Tag matches: medium
  - Description matches: lower
- Tie-breakers:
  - Prefer entries with `installSource`
  - Prefer higher-quality signals (future: conformance tier, stars, downloads, recency)

The goal is "good enough" relevance without pulling in a heavy fuzzy-matching dependency.

## Install Resolution by ID

For ergonomics, Pi should support:
- `pi install <id-or-name>` for entries where `installSource` is present and the match is unique.

Resolution rules:
1. Exact match (case-insensitive) on `name`
2. Exact match (case-insensitive) on `id`
3. Provider-specific aliases (e.g. `npm/<name>`)

If multiple entries match, Pi should refuse to guess and instruct the user to pass an explicit
`npm:` or `git:` source string.

