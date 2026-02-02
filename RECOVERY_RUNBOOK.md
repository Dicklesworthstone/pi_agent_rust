# Recovery Runbook

## Symptoms
- DB corruption detected
- JSONL parse failure
- Mismatched version markers

## Steps
1. Acquire lock
2. Validate source of truth
3. Rebuild target store
4. Update version markers
5. Verify counts/hashes
6. Release lock

## Commands
- `pi sessions import-jsonl` (rebuild SQLite index from JSONL)
- `pi sessions export-jsonl` (dump indexed metadata for inspection)
