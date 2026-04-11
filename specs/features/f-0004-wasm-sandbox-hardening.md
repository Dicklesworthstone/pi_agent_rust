# f-0004: WASM Sandbox Hardening

> Epic: [Rust Investment Roadmap](../epics/rust-investment-roadmap.md) — Item 4
> Priority: High | Effort: 2-3 weeks

---

## Problem

The extension subsystem (~121k LOC across 20+ files) is the Rust port's
largest investment and its primary competitive moat over the TypeScript
original. The TypeScript agent runs extensions in-process with full access;
the Rust port has wasmtime-based WASM sandboxing (`src/pi_wasm.rs`) and a
hostcall system (7 modules). However, the sandbox needs hardening to become
a production-grade security boundary.

## Current State

| Component | File | LOC | Status |
|-----------|------|-----|--------|
| WASM runtime | `src/pi_wasm.rs` | ~1,200 | Basic wasmtime integration |
| Hostcall dispatch | `src/hostcall_*.rs` (7 files) | ~8,500 | Functional |
| Extension runtime | `src/extensions.rs` | 50,453 | Monolith (see f-0002) |
| Extension JS bridge | `src/extensions_js.rs` | 25,360 | QuickJS integration |
| Validation | `src/extension_validation.rs` | ~2,000 | Exists |
| Scoring | `src/extension_scoring.rs` | ~3,000 | Exists |
| Licensing | `src/extension_license.rs` | ~2,500 | Exists |
| Conformance | `src/extension_conformance_matrix.rs` | ~1,500 | Exists |

## Features

### 4a. Resource Limits

Extensions must not be able to starve the host agent of resources.

**Limits to enforce:**
- **CPU time** — per-invocation wall-clock timeout (configurable, default 30s)
- **Memory** — wasmtime memory cap per extension instance (default 64MB)
- **Filesystem** — restrict to extension's own data directory + explicitly
  granted paths (no access to `~/.ssh`, `~/.aws`, etc.)
- **Network** — allowlist of domains the extension may contact (default: none)
- **Subprocess** — extensions cannot spawn processes unless explicitly permitted

**Implementation:**
- wasmtime `StoreLimits` for memory/table limits
- `tokio::time::timeout` wrapper around extension entry points
- Filesystem access via hostcall — validate paths against allowlist before
  passing to real fs ops in `hostcall_rewrite.rs`

### 4b. Capability-Based Permissions

Extensions declare what they need; users approve at install time.

**Manifest format** (extension's `pi-extension.toml`):
```toml
[capabilities]
filesystem = ["read", "write"]    # or "none"
network = ["api.example.com"]     # domain allowlist
subprocess = false
env_vars = ["HOME", "PATH"]       # which env vars are visible
```

**Implementation:**
- Parse capabilities from extension manifest at install
- Store approved capabilities in `src/permissions.rs` (already has
  `PermissionStore`)
- Hostcalls check capability grants before executing:
  - `dispatch_fs()` checks `filesystem` capability
  - `dispatch_http()` checks `network` capability
  - `dispatch_exec()` checks `subprocess` capability
  - `dispatch_env()` checks `env_vars` capability

### 4c. Extension Marketplace API

The scoring, licensing, and validation modules exist but operate locally.
Connect them to a registry.

**Endpoints needed:**
- `GET /extensions` — search/list published extensions
- `GET /extensions/{id}` — metadata, scores, license info
- `GET /extensions/{id}/versions/{version}` — download WASM bundle
- `POST /extensions` — publish (authenticated)

**Client-side** (in `src/package_manager.rs`):
- `pi extension search <query>`
- `pi extension install <name>[@version]`
- `pi extension publish`

### 4d. Hot-Reload

Load/unload extensions without restarting the agent session.

**Implementation:**
- Watch extension directory for changes (notify crate)
- On change: unload old WASM instance, load new one
- Preserve extension state across reloads via serialization
- Debounce reloads (500ms) to avoid churn during development

## Acceptance Criteria

- Extensions cannot allocate >64MB memory (wasmtime OOM, not host OOM)
- Extensions cannot run longer than configured timeout
- Extensions cannot access files outside their granted paths
- `pi extension install` resolves from registry and validates capabilities
- Extension reload works without dropping the current conversation
