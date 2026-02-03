# Extension Sample Set (bd-ic9)

This document summarizes the **frozen sample set** defined in `docs/extension-sample.json`. The JSON is the source of truth; this file is a human-readable overview.

## Snapshot

- **Source**: `pi-mono` at commit `df5b0f76c026b35fdd7f0fb78cb0dbaaf939c1b5`
- **Sample size**: 16 (min 12, max 20)
- **Selection**: CONFORMANCE.md + EXTENSION_SAMPLING_MATRIX quotas satisfied, with explicit swaps noted in the manifest rationale.
- **Checksums**: `sha256` fields are **null** pending fixture capture (see bd-2qd / bd-3rr).

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

1. Fill `checksum.sha256` for each entry at the pinned commit.
2. Generate fixture outputs for each extension (bd-2qd) and normalize paths/time/randomness (bd-1oz).
3. Use this list as the canonical sample for conformance and benchmark runs.
