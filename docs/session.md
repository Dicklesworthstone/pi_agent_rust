# Sessions

Pi stores conversation history in session files.

## Current Storage Model (V1)

### File format

Sessions are stored as JSONL (JSON Lines) files.

### Location

Sessions are grouped by project directory:
`~/.pi/agent/sessions/--encoded-project-path--/`

Filename format: `YYYY-MM-DDTHH-MM-SS.sssZ_id.jsonl`

### Structure

1. Header: the first line is always a `SessionHeader` object containing metadata (ID, timestamp, CWD, initial settings).
2. Entries: subsequent lines are `SessionEntry` objects representing events in the conversation.

### Entry types

- `message`: User or Assistant message.
- `model_change`: User switched models.
- `thinking_level_change`: User changed thinking settings.
- `compaction`: Context was summarized to save tokens.
- `branch_summary`: A summary of a branch point (when forking).
- `session_info`: Updates like session renaming.
- `label`: Metadata label assignment on an entry.
- `custom`: Extension-defined structured payload.

### Tree structure

Pi supports conversation branching. Each entry has an `id` and an optional `parent_id`.

- Linear conversation: `A -> B -> C`
- Branching:
  ```
  A -> B -> C
       \ -> D
  ```

When you navigate to a previous message and reply, Pi creates a new branch.

### Management

#### Resume (`/resume`, `pi -r`)

Opens the session picker to switch between sessions.
- Select: Enter
- Delete: Ctrl+D (requires confirmation)

#### Tree navigator (`/tree`)

Visualizes the branching structure of the current session.
- Navigate: Up/Down
- Switch: Enter (switches active context to the selected node)

#### Forking (`/fork`)

Creates a new session file starting from the current point (or a selected point). This is useful when you want to explore a significantly different direction without cluttering the current session file.

#### Compaction (`/compact`)

Manually triggers context compaction. Pi also compacts automatically based on the `compaction` settings in `settings.json`.

## ADR: Session Store V2 + Wire-Format Contract

- ADR ID: `ADR-SESSION-STORE-V2`
- Bead: `bd-3ar8v.3.1`
- Status: Accepted for implementation in Phase 2
- Date: 2026-02-15

### Context

V1 JSONL sessions are robust and simple, but large long-running sessions pay high read/write amplification during save, resume, and maintenance workflows. Phase-2 performance goals require:

1. Append-path scalability under very large histories.
2. Resume behavior that is `O(index + tail)` in steady state.
3. Deterministic migration and rollback from V1 stores.
4. Explicit corruption detection and bounded recovery paths.

### Decision

Introduce a Session Store V2 layout built around:

1. Segmented append log for session entries.
2. Sidecar offset index for direct entry addressing.
3. Monotonic checkpoints for non-blocking maintenance and recovery.
4. Migration ledger with explicit cutover and rollback evidence.

V1 JSONL remains readable for migration and rollback but is no longer the target architecture for high-scale paths.

### V2 layout (normative)

The logical V2 session container is:

```text
<session-id>.v2/
  manifest.json
  segments/
    0000000000000001.seg
    0000000000000002.seg
  index/
    offsets.jsonl
  checkpoints/
    0000000000000001.json
  migrations/
    ledger.jsonl
  tmp/
```

### Wire-format contract (normative)

Machine-readable schema: `docs/schema/session_store_v2_contract.json`

All serialized JSON member names use lower camel case (`entrySeq`,
`sourceFormat`, `migrationEvents`). Rust identifiers and explanatory prose may
use snake case, but snake-case member names are not accepted on the wire. Enum
values such as `pre_migration` and `native_v2` remain exactly as shown in the
schema. Persisted contract documents reject unknown members; only the
`segment_frame.payload` object is intentionally open to entry-specific fields.

An empty store is represented explicitly: `entriesTotal`, `segmentCount`,
`head.segmentSeq`, and `head.entrySeq` are all zero, `head.entryId` is empty,
and the segment/index/checkpoint arrays may be empty. A non-empty store requires
positive head and segment values plus a contract-valid entry ID. Checkpoints
always require a non-empty head.

Contract schema IDs:

1. `pi.session_store_v2.contract.v1` (bundle-level validation artifact)
2. `pi.session_store_v2.manifest.v1`
3. `pi.session_store_v2.segment_frame.v1`
4. `pi.session_store_v2.offset_index.v1`
5. `pi.session_store_v2.checkpoint.v1`
6. `pi.session_store_v2.migration_event.v1`

Required contract properties:

1. Strictly contiguous `entrySeq` values and monotonic `segmentSeq` values.
2. Stable `entryId` references from index/checkpoint/migration records.
3. Hash-chain integrity material in manifest and checkpoints.
4. Explicit migration correlation IDs and classified outcomes.
5. Deterministic state transitions with fail-closed validation.

### State machine and invariants

Canonical states:

1. `CLEAN`
2. `DIRTY`
3. `SEGMENT_SEALED`
4. `INDEXED`
5. `CHECKPOINTED`
6. `MIGRATION_STAGING`
7. `MIGRATED`
8. `ROLLED_BACK`
9. `FAILED`

Allowed transitions are intentionally narrow and enforced by schema + tests:

1. `CLEAN -> DIRTY | MIGRATION_STAGING`
2. `DIRTY -> SEGMENT_SEALED | FAILED`
3. `SEGMENT_SEALED -> INDEXED | FAILED`
4. `INDEXED -> CHECKPOINTED | DIRTY | FAILED`
5. `CHECKPOINTED -> DIRTY | MIGRATION_STAGING | ROLLED_BACK | FAILED`
6. `MIGRATION_STAGING -> MIGRATED | ROLLED_BACK | FAILED`
7. `MIGRATED -> DIRTY | FAILED`
8. `ROLLED_BACK -> DIRTY | FAILED`
9. `FAILED -> DIRTY | ROLLED_BACK`

Invariant IDs (must hold unless state is `FAILED`):

1. `INV-001`: parent links are closed (`parentEntryId` either null or known).
2. `INV-002`: `entrySeq` is strictly increasing by 1 across the store.
3. `INV-003`: index rows resolve to in-bounds `(segmentSeq, frameSeq, byteOffset, byteLength)` ranges with complete, gap-free segment coverage.
4. `INV-004`: checkpoint head matches manifest head at checkpoint creation time.
5. `INV-005`: hash chain is continuous from first segment frame to current head.
6. `INV-006`: branch heads referenced by active context are indexed.
7. `INV-007`: migration cutover is atomic: both manifest pointer and active store marker move together.

### Steady-state resume validation

Resume keeps bounded hydration honest without treating it as a full audit. It
reads the JSONL header and the complete V2 offset index, then validates the
index documents, contiguous entry/frame/byte ranges, segment metadata, and
complete segment-file coverage. This structural pass is O(index rows + segment
files) and does not scan every frame body. The manifest must pass its self-hash
and declared invariants, must match index-derived entry, byte, segment, head,
and last-CRC facts, and must keep message, branch, and compaction counters
within bounds implied by the indexed entry count.

Hydration uses that same validated index snapshot and reads only the selected
full, active-path, or tail rows. Before a fetched frame is used, the reader
enforces its configured size limit, exact indexed byte range, CRC32C, trailing
LF, schema and index coordinates, entry and parent IDs, entry type, timestamp,
and payload byte count and SHA-256. Consequently, tail resume is O(index rows +
segment files + selected frames). A fetched parent ID must exist in the index,
and cycles wholly visible in the fetched set are rejected; forward parent
references remain valid because migration preserves authoritative JSONL order.
Corruption in an unselected frame body is detected when that frame is fetched,
not by the bounded structural pass.

Full integrity and migration validation remain deliberately stronger:
`validate_integrity`, `validate_session_integrity`, and manifest/store audit
paths scan every frame, validate the parent graph and hash chain, and check
checkpoint evidence. A resume-time failure in a selected frame or in the
manifest/JSONL identity envelope fails closed and invokes repair from the
authoritative JSONL source where that repair is permitted.

### Failure semantics and recovery behavior

#### Append failure

If append fails before segment fsync, no index update is committed and state remains `DIRTY`. Recovery retries append with same logical entry payload.

#### Segment seal failure

If segment seal fails after data write but before manifest/index commit, the segment is treated as pending and reconciled on open by replaying tail checksums.

#### Index update failure

If index write fails after a durable segment write, open-path recovery logs a warning and rebuilds missing index rows from the segment tail. The migration ledger remains reserved for migration and rollback events.

#### Checkpoint failure

Checkpoint files are staged in a temp file and atomically renamed without replacing an existing final checkpoint. A crash-left regular temp file is overwritten on retry; linked or special-file temp occupants are rejected. The last valid published checkpoint remains authoritative.

#### Migration cutover failure

If cutover fails before commit marker, source V1 remains active. If cutover fails after commit marker but before verification finalization, state becomes `FAILED` and deterministic rollback is required before serving writes.

### Migration and rollback contract

Forward migration (`jsonl_v3|sqlite_v1 -> native_v2`) requires:

1. Create migration event `phase=planned`.
2. Build V2 segments/index/checkpoints in staging path.
3. Validate integrity (`entryCountMatch`, `hashChainMatch`, `indexConsistent`).
4. Commit cutover atomically by updating active manifest pointer.
5. Emit `phase=completed` with `outcome=ok`.

Format cutback (`native_v2 -> jsonl_v3|sqlite_v1`) is a migration-layer
operation. It requires:

1. Preserve source snapshot ID and migration correlation ID.
2. Restore previous active pointer atomically.
3. Verify rollback target integrity before reopening writes.
4. Emit rollback event with explicit reason and outcome.

Checkpoint rollback is a distinct, in-place V2 history operation implemented
by `SessionStoreV2::rollback_to_checkpoint`. It does not switch the active
format or restore a V1 pointer. Before changing the active index or segment
set, it durably records a bounded rollback intent and a checksum-bound staged
index. Recovery replays that intent idempotently, publishes the retained index
before truncating or quarantining segment tails, reconciles the manifest,
quarantines checkpoints newer than the target, verifies the resulting store,
and removes the intent only after durable success evidence is recorded.

Recovery from partial migration is deterministic:

1. No commit marker: continue from source format.
2. Commit marker with failed verification: force rollback path.
3. Missing rollback target: hard fail with `FAILED` and operator action required.

### Testability commitments

This ADR is considered implemented when contract tests can prove:

1. Contract examples validate against `docs/schema/session_store_v2_contract.json`.
2. Invalid transitions and missing critical fields fail closed.
3. Migration and rollback records are schema-valid and correlation-linked.
4. Downstream implementation beads (`3.2`, `3.3`, `3.7`) consume this contract directly without reinterpretation.
