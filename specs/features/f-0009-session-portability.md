# f-0009: Session Portability & Collaboration

> Epic: [Rust Investment Roadmap](../epics/rust-investment-roadmap.md) — Item 9
> Priority: Low | Effort: 1-2 weeks

---

## Problem

Sessions are stored as local JSONL files with a SQLite index. The Rust port
has richer session infrastructure than the TypeScript original (metrics,
v2 store, HTML export) but sessions remain single-user, single-machine.
Sharing a session requires manual file transfer.

## Current State

| Component | File | Status |
|-----------|------|--------|
| JSONL storage | `src/session.rs` | Full read/write/branch |
| SQLite index | `src/session_sqlite.rs` | Fast lookup |
| HTML export | `src/session.rs` (`to_html()`) | Functional |
| Session metrics | `src/session_metrics.rs` | Token/time tracking |
| Share module | `src/interactive/share.rs` | Shell-out based |
| Share viewer URL | `session.rs:513` | `https://buildwithpi.ai/session/` |
| SDK export | `src/sdk.rs` (`export_html()`) | RPC-accessible |

## Features

### 9a. Markdown Session Export

HTML export exists; add Markdown for portability and diff-friendliness.

**Format:**
```markdown
# Session: <name>
**Model:** claude-sonnet-4-20250514 | **Date:** 2026-04-11 | **Messages:** 42

---

## User
<user message content>

## Assistant
<assistant message content>

### Tool: bash
\`\`\`bash
$ ls -la
<output>
\`\`\`

---
```

**Implementation:**
- Add `to_markdown()` to `Session` (parallel to `to_html()`)
- Wire to CLI: `pi export --format markdown <session-id>`
- Wire to RPC: `exportMarkdown` method in `src/rpc.rs`

### 9b. Session Sharing via Gist

**Flow:**
1. User runs `pi share` or presses share keybinding
2. Agent exports session to Markdown
3. Uploads to GitHub Gist (using `gh` CLI or GitHub API)
4. Returns shareable URL

**Implementation:**
- Add `share_to_gist()` to `src/interactive/share.rs`
- Use GitHub API: `POST /gists` with session Markdown as content
- Requires `GITHUB_TOKEN` — prompt user to auth if missing
- Return URL: `https://gist.github.com/<id>`
- Also support the existing `buildwithpi.ai/session/` viewer

### 9c. Session Import

Resume a session from a shared export.

**Supported sources:**
- Local JSONL file (already works)
- GitHub Gist URL → download and import
- Markdown file → parse back into session entries

**Implementation:**
- `pi import <url-or-path>`
- Gist import: `GET /gists/<id>`, extract content, create local session
- Markdown import: parse structured format back into `SessionEntry` list
- Imported sessions are read-only until user forks (existing branch mechanism)

### 9d. Session Sync (Future)

Real-time collaborative sessions — larger scope, design only.

**Architecture sketch:**
- WebSocket relay server (could be a separate service)
- CRDT-based entry merging (each user appends, conflict-free)
- Presence indicators in TUI footer
- Permission model: owner, editor, viewer

**Not in scope for v1** — document the design for future implementation.

## Acceptance Criteria

- `pi export --format markdown <session>` produces readable Markdown
- `pi share` uploads to GitHub Gist and returns URL
- `pi import <gist-url>` downloads and creates local session
- Imported sessions can be forked and continued locally
- Markdown export round-trips: export → import preserves all messages
