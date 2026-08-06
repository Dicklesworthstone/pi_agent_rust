# SEC-1.3: Code-Grounded Baseline Audit vs Node/Bun Risk Posture

> **Bead:** bd-2nr0q
> **Status:** Completed
> **Author:** Claude Opus agent
> **Date:** 2026-02-14
> **Cross-reference:** [SEC-1.1 Threat Model](threat-model.md) (bd-3jyg8)
>
> **Coordinate scope:** Every numeric source-line coordinate below is a
> 2026-02-14 audit snapshot, not a current line-number contract. After the
> extension decomposition, the stable façade and shared dispatch remain in
> `src/extensions.rs`; hostcall validation lives in
> `src/extensions/protocol.rs`; manager risk/ledger methods live in
> `src/extensions/extension_manager_impl.rs`; and the filesystem connector
> implementation lives in `src/extensions/fs_connector.rs`.

---

## 1. Executive Summary

This audit documents the concrete security controls in `pi_agent_rust`'s extension
runtime, compares them with the ambient-access model of Node.js/Bun, and classifies
gaps by severity with exploit narratives and proposed mitigations.

**Overall posture:** The Rust implementation provides a **significantly stronger
security baseline** than Node/Bun through capability-gated hostcalls, process-tree
RAII cleanup, blocklist-based secret filtering, TOCTOU-resistant filesystem checks
(Linux), and hash-chained audit ledgers. However, **4 gaps** remain; G-1, G-2,
and G-4 have since been fixed, leaving low-severity hardening work and
informational design asymmetries.

---

## 2. Methodology

Every finding in this audit references a concrete code path (file, line number,
function name). Findings were produced by:

1. Reading all security-relevant source files (`extensions.rs`, its
   `extensions/` implementation modules, `extensions_js.rs`, `tools.rs`,
   `providers/mod.rs`, `agent.rs`, `config.rs`, `permissions.rs`).
2. Tracing every hostcall from JS entry point through policy check to connector dispatch.
3. Comparing each control surface with the equivalent Node.js/Bun ambient model.
4. Classifying gaps using the threat IDs from [SEC-1.1](threat-model.md) (T1-T8).

A fresh maintainer can reproduce this audit by reading the referenced code paths
and running the existing test suite (`cargo test` targets referenced inline).

---

## 3. Control Inventory

### 3.1 Capability-Gated Hostcall Dispatch

**Code (2026-02-14 snapshot):** `src/extensions.rs` lines 7229-7355
**Function:** `dispatch_host_call_shared()`

Every JS-to-Rust call flows through a single chokepoint:

```
JS hostcall → validate_host_call() → required_capability_for_host_call_static()
            → policy.evaluate_for(capability, extension_id) → runtime risk check
            → dispatch_shared_allowed() → connector
```

**Enforcement sequence (6 stages):**

| Stage | Function | Line | Purpose |
|-------|----------|------|---------|
| 1. Validate | `validate_host_call()` | 3287-3324 | Structural integrity; capability-method consistency |
| 2. Map capability | `required_capability_for_host_call_static()` | 1954-2016 | Method-to-capability derivation |
| 3. Policy check | `evaluate_for()` | 1844-1941 | 5-layer precedence: ext-deny > global-deny > ext-allow > default-caps > mode-fallback |
| 4. Prompt resolve | `resolve_shared_policy_prompt()` | 7460-7508 | User approval with per-(ext, cap) caching |
| 5. Runtime risk | `evaluate_runtime_risk()` | 1671+ | Bayesian risk scoring with Allow/Harden/Deny/Terminate actions |
| 6. Dispatch | `dispatch_shared_allowed()` | 7515-7639 | Route to typed handler |

**Node/Bun equivalent:** None. Extensions in Node run with ambient access to all
APIs (`require('child_process')`, `require('fs')`, `process.env`) with zero
interposition.

**Assessment:** Strong. The single-chokepoint design ensures no capability can be
exercised without policy evaluation.

### 3.2 Policy Profiles

**Code (2026-02-14 snapshot):** `src/extensions.rs` lines 1370-1484
**Functions:** `PolicyProfile::to_policy()`, `ExtensionPolicy::default()`

| Profile | Mode | Default Caps | Denied Caps | Unknown Caps |
|---------|------|-------------|-------------|--------------|
| Safe | Strict | read, write, http, events, session | exec, env | Deny |
| Standard (default) | Prompt | read, write, http, events, session | exec, env | Prompt user |
| Permissive | Permissive | (all) | (none) | Allow |

**5-layer precedence in `evaluate_for()` (lines 1844-1941):**
1. Per-extension deny list (line 1856-1869)
2. Global `deny_caps` (line 1871-1882)
3. Per-extension allow list (line 1884-1897)
4. Global `default_caps` (line 1899-1903)
5. Mode fallback: Strict=deny, Prompt=prompt, Permissive=allow (line 1908-1940)

**Node/Bun equivalent:** No policy layer exists. All capabilities are ambient.

**Assessment:** Strong design. Deny-first precedence order prevents
per-extension overrides from widening global denials.

### 3.3 Environment Variable Filtering

**Code:** `src/extensions_js.rs` lines 63-130
**Function:** `is_env_var_allowed()`

Three-tier blocklist:

| Tier | Pattern | Count | Examples |
|------|---------|-------|---------|
| Exact match | `BLOCKED_EXACT` | 26 | `ANTHROPIC_API_KEY`, `DATABASE_URL`, `GH_TOKEN`, `AWS_ACCESS_KEY_ID` |
| Suffix match | `BLOCKED_SUFFIXES` | 11 | `*_API_KEY`, `*_SECRET`, `*_TOKEN`, `*_PASSWORD`, `*_CREDENTIAL` |
| Prefix match | `BLOCKED_PREFIXES` | 2 | `AWS_SECRET_*`, `AWS_SESSION_*` |

**Bypass prevention:**
- Case-insensitive matching via `.to_ascii_uppercase()` (line 107)
- `PI_*` vars unconditionally allowed (line 125-127) — but this is checked
  *after* the blocklist, so `PI_API_KEY` would still be blocked by suffix match

**process.env implementation** (lines 14310-14343):
- Read-only Proxy: `set()` and `deleteProperty()` silently ignored (lines 14319-14325)
- Enumeration blocked: `ownKeys()` returns empty array (line 14333-14335)
- Each `get()` routes through `__pi_env_get_native()` with `is_env_var_allowed()` gate

**Node/Bun equivalent:** Full `process.env` access with all keys enumerable
and writable. No filtering whatsoever.

**Assessment:** Strong. Blocklist approach with three tiers covers known secret
patterns. Residual risk: novel key names not matching any pattern
(see Gap G-3 below).

### 3.4 Filesystem Confinement

**Code:** `src/extensions_js.rs` lines 11700-11895
**Function:** `readFileSync` native implementation

**Linux (TOCTOU-resistant, lines 11721-11761):**
1. Open file by path to get file descriptor
2. Read `/proc/self/fd/{fd}` to get kernel-resolved real path
3. Verify real path is within allowed roots
4. Read from file descriptor (not path)

**Non-Linux fallback (lines 11773-11831):**
1. Walk ancestor directories with `fs::canonicalize()`
2. Check canonical path against workspace root + extension roots
3. **Gap:** No symlink loop detection on non-Linux (see Gap G-5)

**Allowed roots:**
- Workspace root (CWD) — always allowed
- Extension roots registered via `add_allowed_read_root()` (lines 10483-10485)

**Node/Bun equivalent:** `fs.readFileSync()` reads any path the process user
can access. No confinement.

**Assessment:** Strong on Linux (TOCTOU-safe). Adequate on other platforms
with canonicalization fallback.

### 3.5 Process Execution Controls

#### 3.5.1 Bash Tool (Agent-Level)

**Code:** `src/tools.rs` lines 1326-1500

| Control | Implementation | Line |
|---------|---------------|------|
| Argument separation | `Command::new(shell).arg("-c").arg(&command)` | 1364-1367 |
| Stdin null | `Stdio::null()` prevents interactive input | 1368 |
| Default timeout | 120s (`DEFAULT_BASH_TIMEOUT_SECS`) | 115, 1334-1338 |
| Graceful shutdown | SIGTERM first, 5s grace, then SIGKILL | 1422-1439 |
| Process tree cleanup | `ProcessGuard` with RAII Drop + `kill_process_tree()` | 3543-3638 |
| Output bounding | Truncated to `DEFAULT_MAX_BYTES * 2` with temp file overflow | 1391, 1473-1481 |

#### 3.5.2 Extension Exec (Sync)

**Code:** `src/extensions_js.rs` lines 11900-12045
**Function:** `__pi_exec_sync_native()`

**Security gate (line 11924-11939):**
```rust
if !allow_unsafe_sync_exec {
    return "Capability 'exec' denied by policy ..."
}
```

- Default: `false` (line 4355 in `PiJsRuntimeConfig`)
- Cannot be changed at runtime — set during construction only
- `child_process.execSync()` and `spawnSync()` route through this gate (lines 7229-7269)

**Node/Bun equivalent:** `child_process.exec()` and `spawn()` available to any
`require()` call. No gating, no process tree cleanup, no RAII.

**Assessment:** Strong. Two independent gates (capability policy + sync exec flag)
with RAII cleanup guarantees.

### 3.6 Tool Execution Gating (Extension Hooks)

**Code:** `src/agent.rs` lines 1374-1428

| Hook | Function | Line | Behavior |
|------|----------|------|----------|
| Pre-tool | `dispatch_tool_call_hook()` | 1374-1391 | Extension can block tool execution |
| Post-tool | `apply_tool_result_hook()` | 1407-1428 | Extension can modify output |

**Fail-open on error:** If hook dispatch errors, tool executes anyway (lines
1386-1389). This is a deliberate design choice — extension bugs should not
block agent functionality — but it means a malfunctioning security hook
provides no protection. See Gap G-6.

### 3.7 Network Controls

**Code:** `src/extensions_js.rs` lines 4551-4578

| Control | Implementation | Line |
|---------|---------------|------|
| Module import restriction | Network URLs blocked (`http://`, `https://`) | 4570-4573 |
| Bare package restriction | npm-style specifiers blocked | 4574-4578 |
| HTTP requests | Mediated through `pi.http()` hostcall | 11272-11307 |

HTTP requests go through the capability policy gate (requires `http` capability).
No URL allowlist/blocklist exists at the JS layer — all URL filtering, if any,
occurs at the Rust dispatch layer.

**Node/Bun equivalent:** `require('http')`, `fetch()`, and dynamic `import()`
from URLs are all available with no interposition.

### 3.8 Audit Ledger

**Code (2026-02-14 snapshot):** `src/extensions.rs` lines 1549-1572, 3643-3700

**Runtime risk ledger** — append-only, hash-chained:

```rust
struct RuntimeRiskLedgerEntry {
    ts_ms: i64,
    extension_id: String,
    call_id: String,
    capability: String,
    method: String,
    params_hash: String,        // Canonical param fingerprint
    risk_score: f64,
    posterior: RuntimeRiskPosterior,  // Bayesian {safe, suspicious, unsafe}
    selected_action: RuntimeRiskAction,
    ledger_hash: String,        // SHA-256 of this entry
    prev_ledger_hash: Option<String>,  // Chain linkage
    // ... drift detection, conformal residuals
}
```

**Integrity verification:** `runtime_risk_compute_ledger_hash()` chains entries;
verify/replay helpers exist for forensic analysis.

**Node/Bun equivalent:** No structured audit trail. Extensions execute silently.

### 3.9 Capability Requirement Mapping

**Code (2026-02-14 snapshot):** `src/extensions.rs` lines 1954-2016
**Function:** `required_capability_for_host_call_static()`

| Method | Sub-operation | Required Capability |
|--------|--------------|-------------------|
| `tool` | read, grep, find, ls | `read` |
| `tool` | write, edit | `write` |
| `tool` | bash | `exec` |
| `tool` | (other) | `tool` |
| `fs` | read, list, stat | `read` |
| `fs` | write, mkdir, delete | `write` |
| `exec` | (any) | `exec` |
| `env` | (any) | `env` |
| `http` | (any) | `http` |
| `session` | (any) | `session` |
| `ui` | (any) | `ui` |
| `events` | (any) | `events` |
| `log` | (any) | `log` |

**Validation** (lines 3287-3324): `validate_host_call()` verifies declared
capability matches the derived requirement, preventing JS from claiming a
lower-privilege capability to bypass policy.

### 3.10 Runtime Risk Controller

**Code (2026-02-14 snapshot):** `src/extensions.rs` lines 1486-1573, 1671+

| Setting | Default | Purpose |
|---------|---------|---------|
| `enabled` | `false` | Master switch |
| `alpha` | 0.01 | Type-I error budget |
| `window_size` | 128 | Sliding window for drift |
| `ledger_limit` | 2048 | Max in-memory entries |
| `decision_timeout_ms` | 50 | Fallback if decision too slow |
| `fail_closed` | `true` | Deny on controller failure |

**Dangerous capability risk scores:**

| Capability | Base Score | Dangerous? |
|-----------|-----------|-----------|
| exec | 0.95 | Yes |
| env | 0.85 | Yes |
| http | 0.70 | Yes |
| tool | 0.50 | No |
| write | 0.45 | No |
| session | 0.35 | No |
| events | 0.30 | No |
| ui | 0.20 | No |
| read | 0.15 | No |

**Assessment:** Comprehensive design but **disabled by default**. When enabled,
provides statistical anomaly detection with Bayesian posteriors, conformal
prediction residuals, and hash-chained evidence.

---

## 4. Gap Analysis

### G-1: FsConnector Ignores Per-Extension Policy Overrides

**Severity:** Medium
**Threat ID:** T3 (Dangerous Capability Misconfiguration)
**Location (2026-02-14 snapshot):** `src/extensions.rs` line ~2220
**Current owner:** `src/extensions/fs_connector.rs` (`FsConnector` implementation)

**Current code:**
```rust
let policy_check = self.policy.evaluate_for(capability, extension_id);
```

**Exploit narrative:** An operator configures a per-extension deny override for
`"read"` on a specific extension (e.g., `{"ext-untrusted": {"deny": ["read"]}}`).
The extension calls `pi.fs("read", {path: "~/.ssh/id_rsa"})`. The FsConnector
checks the *global* policy (which allows `read` by default), ignoring the
per-extension deny. The file is read despite the operator's intent.

**Impact:** Per-extension privilege isolation fails for FS operations. An
extension explicitly denied a capability can still exercise it through the FS
connector path.

**Status:** Fixed. `FsConnector` threads `extension_id` through policy evaluation
and enforces per-extension overrides.

**Test:** `cargo test extensions::tests::core::fs_connector_respects_per_extension_deny`
(added).

---

### G-2: Write Tool Path Traversal Not Fully Normalized

**Severity:** Medium
**Threat ID:** T1 (Malicious Extension Input)
**Location:** `src/tools.rs` line ~2310

**Current code:** `resolve_path()` now applies `normalize_dot_segments()` and
write/edit/grep/find/ls enforce that resolved paths stay under the CWD.

**Exploit narrative:** An LLM-steered write call with
`path: "../../../etc/cron.d/backdoor"` could create files outside the working
directory. While agent-level tool calls are not directly from extensions (they
come from the LLM), a compromised extension could influence the LLM's tool
call parameters via session manipulation.

**Impact:** Files created outside expected project directory. On a multi-tenant
system, this could affect other users.

**Status:** Fixed. Dot-segment traversal is normalized and out-of-root writes
are rejected.

**Test:** `tools::tests::test_write_rejects_outside_cwd` (includes `../` escape).

---

### G-3: Env Var Blocklist Uses Deny-List (Not Allow-List)

**Severity:** Low
**Threat ID:** T6 (Secret Exfiltration)
**Location:** `src/extensions_js.rs` lines 63-130

**Current design:** Blocklist approach — known secret patterns are denied,
everything else is allowed.

**Exploit narrative:** A new provider (e.g., `ACME_SECRET_SAUCE`) stores
credentials in an env var that doesn't match any blocked suffix. An extension
calls `pi.env("ACME_SECRET_SAUCE")` and exfiltrates it via `pi.http()`.

**Impact:** Novel secret naming patterns bypass the blocklist. This is inherent
to deny-list designs.

**Proposed mitigation:** Consider an optional allow-list mode for high-security
deployments: `env_allowlist: ["HOME", "PATH", "SHELL", "TERM", "PI_*"]`.
Keep the deny-list as the default for compatibility but offer the allow-list
as a configuration option.

**Downstream bead:** bd-zh0hj, bd-wzzp4

---

### G-4: Tool Execution Hooks Fail Open

**Severity:** Low
**Threat ID:** T4 (Runtime Abuse)
**Location:** `src/agent.rs` lines 1386-1389

**Current code:** Hook errors are fail-open by default, but can be made
fail-closed with `fail_closed_hooks` configuration (tool execution is denied
when the hook errors or times out).

**Exploit narrative:** An extension registers a security-enforcement tool hook
that is supposed to block dangerous bash commands. If the hook crashes or times
out, the bash command executes anyway. A malicious extension could intentionally
cause the hook to fail to bypass another extension's security check.

**Impact:** Security hooks provide weaker guarantees than policy checks. An
extension cannot rely on another extension's hook for security.

**Status:** Fixed. Hook failure handling is now configurable via
`fail_closed_hooks` (default fail-open).

---

### G-5: Filesystem Confinement Weaker on Non-Linux

**Severity:** Low
**Threat ID:** T1 (Malicious Extension Input)
**Location:** `src/extensions_js.rs` lines 11773-11831

**Current code (non-Linux):** Uses `std::fs::canonicalize()` which resolves
symlinks at check time, but the file could be swapped between check and read
(TOCTOU).

**Linux path (lines 11721-11761):** Opens file first, then verifies real path
via `/proc/self/fd/{fd}` — immune to TOCTOU.

**Exploit narrative:** On macOS, an extension creates a symlink
`/tmp/innocent -> /etc/shadow`, then calls `readFileSync("/tmp/innocent")`.
Between the `canonicalize()` check and the actual read, the symlink target
could be changed. Practically difficult but theoretically possible.

**Impact:** Low. Requires precise timing and local filesystem access.
macOS file permissions typically prevent reading `/etc/shadow` regardless.

**Proposed mitigation:** Use `open()` + `fstat()` on all platforms (the
Linux pattern can be adapted using platform-specific fd path resolution).

---

### G-6: Session Operations Not Granular

**Severity:** Low
**Threat ID:** T7 (Persistent Over-Grant)
**Location (2026-02-14 snapshot):** `src/extensions.rs` lines 8118-8278

**Current design:** A single `"session"` capability gates all session operations:
`get_state`, `get_messages`, `set_name`, `set_model`, `set_label`, etc.

**Exploit narrative:** An extension that only needs to read session state
(`get_messages`) is granted `"session"` capability. It can then also call
`set_model` to switch to a cheaper/less capable model, or `set_name` to
confuse the user.

**Impact:** Over-broad capability grant. Read-only session access is not
distinguishable from read-write.

**Proposed mitigation:** Split `session` into `session.read` and `session.write`
sub-capabilities. The existing `session` capability acts as a wildcard for
backward compatibility.

**Downstream bead:** New — recommend creating.

---

### G-7: Runtime Risk Controller Disabled by Default

**Severity:** Informational
**Threat ID:** T4 (Runtime Abuse)
**Location (2026-02-14 snapshot):** `src/extensions.rs` line 1507

**Current default:** `enabled: false`

**Rationale:** The controller is new and needs calibration (referenced in
residual risks of the threat model). However, while disabled, runtime abuse
detection provides zero protection.

**Impact:** Informational. The static policy layer still enforces capability
boundaries. The runtime risk controller adds defense-in-depth for
approved-but-suspicious patterns.

**Proposed mitigation:** Enable by default once calibration is complete
(tracked in bd-3i9da, bd-cu17q, bd-2vlb5).

---

## 5. Comparison: Pi (Rust) vs Node/Bun Risk Posture

### 5.1 Capability Model

| Surface | Pi (Rust) — Gated | Node/Bun — Ambient |
|---------|-------------------|-------------------|
| Shell execution | Denied by default; requires `exec` capability + policy approval | `require('child_process')` — available to any code |
| File read | Allowed by default; confined to workspace root + extension roots | `require('fs')` — reads any path the process user owns |
| File write | Allowed by default; atomic writes with temp files | `require('fs')` — writes anywhere |
| Environment vars | Denied by default; blocklist filters secrets even when allowed | `process.env` — full read/write access to all vars |
| HTTP requests | Allowed by default; mediated through hostcall | `require('http')`, `fetch()` — unrestricted |
| Process signals | Not available (no `kill` equivalent in default policy) | `process.kill()` — available |
| Dynamic code | `eval()`, `Function()` blocked by compatibility scanner | Available everywhere |
| Module loading | Only file/relative/node: imports; network imports blocked | `require()`, dynamic `import()` from URLs |

### 5.2 Security Boundary Count

```
Pi (Rust):
  JS code → [B2: hostcall ABI] → [B3: policy gate] → [B4: runtime risk]
          → [B5: connector] → OS resource

Node/Bun:
  JS code → OS resource

Pi adds 4 interposition boundaries where Node/Bun has 0.
```

### 5.3 Cleanup Guarantees

| Mechanism | Pi (Rust) | Node/Bun |
|-----------|-----------|----------|
| Process tree | RAII `ProcessGuard` — guaranteed cleanup on Drop | Manual `kill()` — cleanup depends on parent not crashing |
| File handles | Rust ownership — closed when scope exits | GC-dependent — may leak if GC delayed |
| Timeouts | Explicit `Duration` guard with SIGTERM→SIGKILL cascade | `setTimeout()` — developer responsibility |
| Stream cancellation | `Drop` on stream state calls `cancel_best_effort()` | Manual `.destroy()` call required |

### 5.4 Audit Trail

| Aspect | Pi (Rust) | Node/Bun |
|--------|-----------|----------|
| Hostcall logging | Structured tracing with call_id, trace_id, extension_id | None |
| Policy decisions | Logged with capability, decision, reason, params_hash | N/A (no policy) |
| Risk scoring | Hash-chained ledger with Bayesian posteriors | None |
| Tamper detection | `ledger_hash` + `prev_ledger_hash` chain | None |

### 5.5 Secret Protection

| Mechanism | Pi (Rust) | Node/Bun |
|-----------|-----------|----------|
| API key access | Blocked by env var blocklist (26 exact + 11 suffix + 2 prefix) | `process.env.ANTHROPIC_API_KEY` returns the key |
| Key enumeration | `ownKeys()` returns `[]` — cannot list env vars | `Object.keys(process.env)` lists everything |
| Key mutation | Read-only Proxy — writes silently ignored | Full read/write access |

---

## 6. Prioritized Remediation List

| Priority | Gap ID | Severity | Effort | Downstream Beads |
|----------|--------|----------|--------|-----------------|
| 1 | G-1 | Medium | Fixed (per-extension overrides enforced) | — |
| 2 | G-2 | Medium | Fixed (dot-segment normalization + CWD bounds) | — |
| 3 | G-3 | Low | Medium (add optional env allow-list mode) | bd-zh0hj, bd-wzzp4 |
| 4 | G-4 | Low | Fixed (`fail_closed_hooks` config option) | — |
| 5 | G-5 | Low | Medium (port Linux fd-based check to other platforms) | New |
| 6 | G-6 | Low | Medium (split session capability) | New |
| 7 | G-7 | Informational | N/A (tracked in existing calibration beads) | bd-3i9da, bd-cu17q |

---

## 7. Existing Test Coverage

| Control | Test Module | Test Count | Coverage |
|---------|------------|------------|---------|
| Policy evaluation | `extensions::tests` | 20+ | Good (all profiles, per-extension overrides) |
| Env var filtering | `extensions_js::tests`, `tests/npm_module_stubs.rs` | 38+ | Good (blocklist patterns, PI_ allowlist) |
| Tool path normalization | `tools::tests` | 10+ | Good (dot segments, traversal) |
| Process tree cleanup | `tools::tests` | 5+ | Adequate (timeout, kill tree) |
| Extension OAuth | `tests/extensions_provider_oauth.rs` | 20 | Good |
| Hostcall dispatch | `extensions::tests` | 30+ | Good (capability mapping, policy enforcement) |
| Runtime risk ledger | `extensions::tests` | 10+ | Good (hash chain, verify, replay) |
| FsConnector policy | `extensions::tests` | Good | Per-extension override test added |

---

## 8. Recommendations for Downstream Beads

1. **bd-2ezm9 (SEC-1.2 Invariants):** Should codify that "every hostcall MUST
   flow through `dispatch_host_call_shared()`" as an invariant. The prior
   FsConnector bypass (G-1) is now fixed, but the invariant should remain
   explicit.

2. **bd-f0huc (SEC-2.1 Manifest v2):** Extension manifest should declare
   required capabilities so that policy can be pre-evaluated at install time,
   before any runtime execution.

3. **bd-21vng (SEC-2.3 Install-time Scanner):** The compatibility scanner
   already detects `process.env` usage (line 494-503 in extensions.rs). It
   should also flag `pi.fs()` calls to allow pre-install capability assessment.

4. **bd-zh0hj + bd-wzzp4 (SEC-4.x Allowlists):** Implement the optional
   env var allow-list mode from G-3 as part of the broader allowlist work.

---

## 9. Conclusion

Pi's Rust extension runtime demonstrates a materially stronger security posture
than the Node/Bun ambient model:

- **4 interposition boundaries** vs 0 in Node/Bun
- **RAII-guaranteed cleanup** vs manual/GC-dependent in Node/Bun
- **Blocklist secret filtering** vs full exposure in Node/Bun
- **Hash-chained audit ledger** vs no audit trail in Node/Bun
- **Bayesian runtime risk scoring** (when enabled) vs nothing in Node/Bun

The remaining gaps are real but bounded: none allows full sandbox escape.
The most critical systemic improvement would be enabling the runtime risk
controller by default once calibration is complete.
