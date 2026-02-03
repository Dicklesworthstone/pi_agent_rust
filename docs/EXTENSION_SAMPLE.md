# Extension Sample Set (bd-ic9)

This document summarizes the **frozen sample set** defined in `docs/extension-sample.json`. The JSON is the source of truth; this file is a human-readable overview.

## Snapshot

- **Source**: `pi-mono` at commit `df5b0f76c026b35fdd7f0fb78cb0dbaaf939c1b5`
- **Sample size**: 16 (min 12, max 20)
- **Selection**: CONFORMANCE.md + EXTENSION_SAMPLING_MATRIX quotas satisfied, with explicit swaps noted in the manifest rationale.
- **Checksums**: `checksum.sha256` is filled for every entry (see **Artifacts & Checksums**).

## Artifacts & Checksums

To make conformance reproducible offline, we vendor the extension sources for the sample set:

- **Artifacts path**: `tests/ext_conformance/artifacts/<id>/`
- **Provenance**: copied from `legacy_pi_mono_code/pi-mono` at commit `df5b0f76c026b35fdd7f0fb78cb0dbaaf939c1b5` (MIT licensed).
- **Checksum storage**: `docs/extension-sample.json` → `items[].checksum.sha256`
- **Checksum definition**: content-only `sha256` of the artifact file tree, independent of platform file permissions/mtimes.
  - Enumerate all regular files under `tests/ext_conformance/artifacts/<id>/` recursively.
  - Sort by normalized relative path (POSIX `/` separators).
  - Hash stream: `b\"file\\0\" + path + b\"\\0\" + bytes + b\"\\0\"` for each file.

## Coverage Summary

**Runtime tiers**
- legacy-js: 8
- multi-file: 4
- pkg-with-deps: 2
- provider-ext: 2

**Interaction tags**
- tool_only: 5
- slash_command: 4
- event_hook: 6
- ui_integration: 7
- provider: 2
- input_transform: 1

**Complexity**
- small: 3
- medium: 7
- large: 6

**I/O patterns**
- fs-heavy: 6
- network-heavy: 3
- ui-centric: 6
- cpu-heavy: 2
- os-heavy: 4

## Selected Extensions

| ID | Path | Runtime | Interaction | Capabilities | Complexity | I/O |
|---|---|---|---|---|---|---|
| permission-gate | `packages/coding-agent/examples/extensions/permission-gate.ts` | legacy-js | event_hook, ui_integration | exec, env | medium | ui-centric, os-heavy |
| protected-paths | `packages/coding-agent/examples/extensions/protected-paths.ts` | legacy-js | event_hook | read, write | small | fs-heavy |
| todo | `packages/coding-agent/examples/extensions/todo.ts` | legacy-js | tool_only, slash_command, ui_integration | read, write | medium | fs-heavy, ui-centric |
| hello | `packages/coding-agent/examples/extensions/hello.ts` | legacy-js | tool_only | env | small | ui-centric |
| antigravity-image-gen | `packages/coding-agent/examples/extensions/antigravity-image-gen.ts` | legacy-js | tool_only | http, write | medium | network-heavy |
| plan-mode | `packages/coding-agent/examples/extensions/plan-mode` | multi-file | slash_command, ui_integration | read | large | ui-centric |
| status-line | `packages/coding-agent/examples/extensions/status-line.ts` | legacy-js | event_hook, ui_integration | env | small | ui-centric |
| doom-overlay | `packages/coding-agent/examples/extensions/doom-overlay` | multi-file | ui_integration | env | large | cpu-heavy, ui-centric |
| sandbox | `packages/coding-agent/examples/extensions/sandbox` | pkg-with-deps | event_hook, slash_command, ui_integration | exec, read | large | os-heavy, fs-heavy |
| inline-bash | `packages/coding-agent/examples/extensions/inline-bash.ts` | legacy-js | input_transform | exec | medium | os-heavy |
| dynamic-resources | `packages/coding-agent/examples/extensions/dynamic-resources` | multi-file | event_hook | read | medium | fs-heavy |
| custom-provider-anthropic | `packages/coding-agent/examples/extensions/custom-provider-anthropic` | provider-ext | provider | http | large | network-heavy |
| custom-provider-qwen-cli | `packages/coding-agent/examples/extensions/custom-provider-qwen-cli` | provider-ext | provider | exec, http | large | network-heavy |
| with-deps | `packages/coding-agent/examples/extensions/with-deps` | pkg-with-deps | tool_only, slash_command | read, write | medium | fs-heavy |
| subagent | `packages/coding-agent/examples/extensions/subagent` | multi-file | tool_only, ui_integration | exec, read | large | cpu-heavy, os-heavy |
| git-checkpoint | `packages/coding-agent/examples/extensions/git-checkpoint.ts` | legacy-js | event_hook | exec | medium | fs-heavy |

## Next Steps

1. Define per-extension capture scenarios (bd-2qd) in `docs/extension-sample.json` (`scenario_suite`).
2. Implement the legacy capture pipeline to run scenarios and record outputs (bd-3on), then normalize paths/time/randomness (bd-1oz).
3. Use this list + artifacts as the canonical sample for conformance and benchmark runs.

### Legacy Capture Normalization (bd-1oz)

`pi_legacy_capture` writes normalized artifacts alongside the raw capture outputs:
- `stdout.normalized.jsonl`
- `meta.normalized.json`
- `capture.normalized.log.jsonl`

Normalization rules (remove non-determinism, preserve semantics):
- Replace RFC3339 timestamp strings with `<TIMESTAMP>` and numeric `timestamp` fields with `0`.
- Rewrite absolute paths under the repo to `<PROJECT_ROOT>` and the legacy repo root to `<PI_MONO_ROOT>`.
- Rewrite `run-<uuid>` to `<RUN_ID>` and bare UUIDs to `<UUID>`.
- Rewrite mock OpenAI base URLs to `http://127.0.0.1:<PORT>/v1`.
- Rewrite `Total output lines: N` to `Total output lines: <N>`.

## Regenerating Legacy Fixtures (bd-16n / bd-vbs)

This section is the “new maintainer path” for reproducing the committed legacy fixtures.

### What Gets Generated

- **Raw capture artifacts** (one directory per scenario run): `target/legacy_capture/<scenario_id>/<run_id>/`
  - `stdout.jsonl`, `stderr.txt`, `meta.json`, `capture.log.jsonl`
  - plus normalized siblings: `stdout.normalized.jsonl`, `meta.normalized.json`, `capture.normalized.log.jsonl`
- **Golden fixture outputs** (one file per extension): `tests/ext_conformance/fixtures/<extension_id>.json`
  - Schema: `pi.ext.legacy_fixtures.v1`
  - Captures provenance (legacy pi-mono HEAD, node/npm versions, manifest commit/checksum, etc.)

### Prerequisites

- Rust nightly toolchain (see `rust-toolchain.toml`)
- Node + npm available on PATH
- Legacy pi-mono workspace has its dependencies installed (we need `legacy_pi_mono_code/pi-mono/node_modules/tsx/...`)

If you see `missing tsx runner`, run:

```bash
cd legacy_pi_mono_code/pi-mono
npm install
```

### Verify Pins

The sample set pins the legacy reference source via:

- `docs/extension-sample.json` → `source_commit` (pi-mono revision used to select the sample)
- `docs/extension-sample.json` → `items[].source.commit` + `items[].checksum.sha256` (per-extension provenance)

The capture tool records the actual legacy pi-mono checkout used in each scenario’s `meta.normalized.json` under:

- `pi_mono.head`
- `pi_mono.extension_path`
- `pi_mono.manifest_commit`
- `pi_mono.manifest_checksum_sha256`

### Run the Full Capture

From repo root:

```bash
cargo run --bin pi_legacy_capture
```

Defaults (see `src/bin/pi_legacy_capture.rs`):

- Manifest: `docs/extension-sample.json`
- Legacy root: `legacy_pi_mono_code/pi-mono`
- Raw out dir: `target/legacy_capture`
- Fixtures dir: `tests/ext_conformance/fixtures`
- Deterministic/offline: `--no-env` (default true)
- Timeout per scenario: `--timeout-secs 20`

### Run a Single Scenario (Debugging)

```bash
cargo run --bin pi_legacy_capture -- --scenario-id scn-todo-003
```

This is useful when fixing one scenario without regenerating everything.

### Determinism Notes

`pi_legacy_capture` aims to make runs reproducible:

- Sets `TZ=UTC`.
- Runs legacy pi-mono against a local mock OpenAI server for predictable streaming + tool-call events.
- Supports per-scenario mocking hooks from the manifest:
  - `setup.mock_exec`: generates a `node_preload.cjs` to stub `child_process.spawn`.
  - `setup.mock_http`: stubs `fetch()` for offline “image generation” fixtures.
  - `setup.session_branch`: writes `seed_session.jsonl` to preload session history (e.g. toolResult details).

### Troubleshooting

- **Timeouts / hangs:** re-run with a higher timeout and inspect `stderr.txt` + `capture.log.jsonl` under the scenario directory in `target/legacy_capture/...`.
- **Node preload not applied:** confirm the scenario wrote `node_preload.cjs` and that `meta.json` includes `node_preload`; legacy pi-mono should receive it via `NODE_OPTIONS=--require <abs path>`.
- **Seed session failures:** inspect `seed_session.jsonl` for malformed messages; seeded `toolResult` entries must include `toolCallId`, `toolName`, `content` (array), `isError`, and numeric `timestamp`.
