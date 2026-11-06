# Node/Bun API Usage Matrix

Corpus-based matrix of APIs used by extension corpus, with frequency and criticality scoring.
Machine-readable data: [`api_usage_matrix.json`](api_usage_matrix.json).

Generated: 2026-02-07 | Corpus: 1,167 files across 230 extensions

## Corpus Composition

| Tier | Count | Description |
|------|-------|-------------|
| T1   | 61    | Official pi extensions |
| T2   | 59    | Community extensions |
| T3n  | 81    | npm-sourced extensions |
| T3t  | 23    | Third-party GitHub extensions |
| T3a  | 6     | Agent extensions |

## Node Module Usage (ranked by extension count)

| Module | Ext | T1 | T2 | T3 | Shim | Criticality |
|--------|-----|----|----|-----|------|-------------|
| `node:path` | 131 | 11 | 35 | 85 | Real | P0 |
| `node:fs` | 119 | 10 | 30 | 79 | Real | P0 |
| `node:os` | 100 | 7 | 25 | 68 | Real | P0 |
| `node:child_process` | 82 | 8 | 18 | 56 | Real | P0 |
| `node:url` | 34 | 2 | 6 | 26 | Real | P1 |
| `node:fs/promises` | 32 | 2 | 12 | 18 | Partial | P1 |
| `node:crypto` | 27 | 2 | 7 | 18 | External | P1 |
| `node:util` | 15 | 1 | 4 | 10 | Partial | P2 |
| `node:module` | 10 | 1 | 3 | 6 | Real | P2 |
| `node:readline` | 9 | - | 1 | 8 | Partial | P2 |
| `node:net` | 7 | - | 1 | 6 | Stub | P3 |
| `node:test` | 7 | - | - | 7 | Partial | P3 |
| `node:assert/strict` | 7 | - | - | 7 | Real | P3 |
| `node:http` | 6 | - | 1 | 5 | External | P2 |
| `node:https` | 5 | - | - | 5 | External | P2 |
| `node:events` | 3 | - | - | 3 | Real | P2 |
| `node:stream` | 2 | - | - | 2 | Partial | P3 |
| `node:stream/promises` | 2 | - | - | 2 | Partial | P3 |
| `node:buffer` | 1 | - | - | 1 | External | P2 |
| `node:process` | 1 | - | - | 1 | Real | P1 |
| `node:assert` | 1 | - | - | 1 | Real | P3 |
| `node:string_decoder` | 1 | - | - | 1 | Stub | P3 |
| `node:tty` | 1 | - | - | 1 | Stub | P3 |
| `node:zlib` | 1 | - | - | 1 | Partial | P3 |
| `node:v8` | 1 | - | - | 1 | Stub | P3 |
| `node:perf_hooks` | 1 | - | - | 1 | Stub | P3 |
| `node:vm` | 1 | - | - | 1 | Stub | P3 |
| `node:stream/web` | 1 | - | - | 1 | Partial | P3 |
| `node:readline/promises` | 1 | - | - | 1 | Partial | P3 |

## Top 20 Most-Called APIs

| # | API | Calls | Module | Shim |
|---|-----|-------|--------|------|
| 1 | `path.join` | 1220 | node:path | Real |
| 2 | `process.env` | 753 | global | Real |
| 3 | `exec` | 622 | node:child_process | Real |
| 4 | `fs.existsSync` | 466 | node:fs | Real |
| 5 | `spawn` | 306 | node:child_process | Real |
| 6 | `fs.readFileSync` | 264 | node:fs | Real |
| 7 | `process.cwd` | 263 | global | Real |
| 8 | `os.homedir` | 220 | node:os | Real |
| 9 | `process.exit` | 215 | global | Real |
| 10 | `execSync` | 195 | node:child_process | Real |
| 11 | `path.resolve` | 169 | node:path | Real |
| 12 | `path.dirname` | 165 | node:path | Real |
| 13 | `fs.writeFileSync` | 163 | node:fs | Real |
| 14 | `process.platform` | 134 | global | Real |
| 15 | `fs.readFile` | 131 | node:fs | Real |
| 16 | `fs.mkdirSync` | 115 | node:fs | Real |
| 17 | `Buffer.from` | 113 | global | External |
| 18 | `path.basename` | 94 | node:path | Real |
| 19 | `fs.statSync` | 92 | node:fs | Real |
| 20 | `path.relative` | 83 | node:path | Real |

**Key finding**: The top 20 APIs are all real or external shims. No stubs in the critical path.

## npm Package Usage

| Package | Ext | Shim | Criticality |
|---------|-----|------|-------------|
| `@sinclair/typebox` | 80 | Real | P0 |
| `@modelcontextprotocol/sdk` | 7 | Stub | P1 |
| `ws` | 4 | Stub | P2 |
| `chokidar` | 3 | Stub | P2 |
| `jsdom` | 3 | Stub | P2 |
| `better-sqlite3` | 3 | Partial | P3 |
| `diff` | 3 | Partial | P2 |
| `glob` | 3 | Partial | P2 |
| `dotenv` | 3 | Partial | P2 |
| `open` | 3 | Stub | P2 |
| `commander` | 3 | Stub | P3 |
| `chalk` | 2 | Stub | P3 |
| `node-pty` | 2 | Stub | P3 |
| `@anthropic-ai/sdk` | 2 | Stub | P2 |
| `axios` | 2 | Partial | P2 |

## Bun API Usage

Minimal. Only 5 extensions (all T3) use Bun-specific APIs.
`Bun.write` (4), `Bun.connect` (4), `Bun.which` (2), `Bun.spawn` (1).
**Verdict**: P3 - not worth shimming until Bun-specific extensions grow.

## Shim Coverage Summary

| Status | Count | Description |
|--------|-------|-------------|
| Real | 18 | Full functional implementation |
| Partial | 23 | Mix of real + stubs |
| Stub | 23 | Returns default/empty values |
| External | 5 | Delegated to external JS shim file |
| Error throw | 2 | Throws "not available in PiJS" |
| Missing | 2 | Remaining unregistered npm packages used by corpus |

**Total virtual modules**: 62

## Gap Analysis

### P0 Gaps (none)
All P0 modules (path, fs, os, child_process) have real implementations.

### P1 Gaps
- `node:fs/promises` - mostly real but `chmod`, `chown`, `utimes` are stubs
- `node:crypto` - external shim, coverage unclear

### P2 Gaps
- `fs.watch` (9 calls) - path-checking no-op watcher, no real file watching
- `glob` npm package - partial VFS-backed glob/globSync subset
- `ws` npm package - stubbed WebSocket facade, no real network I/O
- `axios` npm package - real HTTP through capability-gated PiJS fetch, with JSON/form/binary bodies, response errors, interceptors, cancellation, timeout and byte limits; custom transports/agents, progress callbacks and streaming/blob response types are unsupported. Native integration tests await DSR qualification.

### P3 Gaps (low priority)
- `better-sqlite3` - real persistence, statements, bindings, transactions, nested rollback, BLOBs and safe integers through the existing native SQLite engine; advanced statement modes, user-defined SQL functions and native addons remain unsupported. Native integration tests await DSR qualification.
- `child_process.fork` (72 calls) - throws error, not feasible in QuickJS
- `node:net` (7 ext) - stubbed sockets (no network I/O)
- `node:test` (7 ext) - partial runner (test/describe/it + hooks + run); reporters/concurrency not supported
- `node:assert/strict` (7 ext) - strict assert facade backed by `node:assert`
- Low-volume builtins: tty/v8/perf_hooks/vm are stubs; zlib, stream/web, and readline/promises expose partial facades

## Criticality Scoring Methodology

- **P0**: Used by 80+ extensions OR 8+ T1 extensions. Must work correctly.
- **P1**: Used by 25-80 extensions OR 2+ T1 extensions. Should work.
- **P2**: Used by 5-25 extensions. Nice to have real impl.
- **P3**: Used by <5 extensions OR infeasible in QuickJS. Stub/missing acceptable.

T1 extensions weight 4x because they are official and must always pass conformance.
