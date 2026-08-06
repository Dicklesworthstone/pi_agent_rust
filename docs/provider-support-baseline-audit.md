# Provider Support Baseline Audit (`bd-3uqg.11.1`)

Generated at (UTC): `2026-02-13T04:48:33Z`

Machine-readable artifact: `docs/provider-baseline-audit.json`

> Historical snapshot: the counts and execution guidance below describe the
> 2026-02-13 `bd-3uqg.11` planning baseline. They are not current registry or
> release evidence. Use `src/provider_metadata.rs`, `src/providers/mod.rs`, and
> the provider metadata/factory tests for the live tree.

## Summary

- Upstream union providers: **90**
- Matrix rows (including explicit user aliases): **92**
- Pi canonical providers in metadata: **87**

### Current Pi Status Counts

| Status | Count |
|---|---:|
| `alias->native-implemented` | 4 |
| `alias->oai-compatible-preset` | 3 |
| `native-adapter-required-unimplemented` | 2 |
| `native-implemented` | 8 |
| `oai-compatible-preset` | 75 |

### Risk Counts

| Risk | Count |
|---|---:|
| `high` | 7 |
| `low` | 14 |
| `medium` | 71 |

## User-Requested Provider Resolution

| Provider | Canonical | Current status | Target status | Risk |
|---|---|---|---|---|
| `alibaba` | `alibaba` | `oai-compatible-preset` | `promote-to-provider-specific-runtime-path-and-complete-test-doc-evidence` | `high` |
| `cerebras` | `cerebras` | `oai-compatible-preset` | `promote-to-provider-specific-runtime-path-and-complete-test-doc-evidence` | `high` |
| `groq` | `groq` | `oai-compatible-preset` | `promote-to-provider-specific-runtime-path-and-complete-test-doc-evidence` | `high` |
| `kimi` | `moonshotai` | `alias->oai-compatible-preset` | `promote-to-provider-specific-runtime-path-and-complete-test-doc-evidence` | `high` |
| `moonshotai` | `moonshotai` | `oai-compatible-preset` | `promote-to-provider-specific-runtime-path-and-complete-test-doc-evidence` | `high` |
| `openrouter` | `openrouter` | `oai-compatible-preset` | `promote-to-provider-specific-runtime-path-and-complete-test-doc-evidence` | `high` |
| `qwen` | `alibaba` | `alias->oai-compatible-preset` | `promote-to-provider-specific-runtime-path-and-complete-test-doc-evidence` | `high` |

## Execution Guidance

- For historical `bd-3uqg.11` reconstruction only, use this frozen matrix.
- Prioritize `high` risk rows, then `medium` rows that block parity completeness.
