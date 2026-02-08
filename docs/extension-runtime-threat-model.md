# Extension Runtime Threat Model

Status: Active  
Primary bead: `bd-k5q5.4.6`  
Last updated: 2026-02-08

This document models realistic abuse paths against the extension runtime and maps each threat to code-level controls and executable tests.

## 1. System Scope

In-scope components:
- `PiJsRuntime` host bridge (`src/extensions_js.rs`)
- hostcall capability policy (`src/extensions.rs`, `src/config.rs`, `src/extension_dispatcher.rs`)
- JS compatibility shims (`node:fs`, `node:child_process`, `node:http`, etc.)
- extension event dispatch and registration surfaces

Out-of-scope components:
- external provider account compromise
- host OS/kernel exploits outside process boundaries
- social engineering outside runtime policy/UI controls

## 2. Assets

Critical assets:
- project workspace file integrity
- secret material in environment/session state
- command execution boundary (`exec`, `child_process`)
- extension event stream and tool invocation integrity
- reproducible conformance and audit evidence artifacts

## 3. Trust Boundaries

Boundaries:
1. Extension JS code (untrusted) -> hostcalls (trusted host boundary)
2. Virtual FS (`__pi_vfs`) -> host filesystem fallback
3. Capability policy decision engine -> execution dispatch
4. User prompt/UI decision channel -> runtime allow/deny effects

The highest-risk boundary is (2), because path access mistakes can expose host files.

## 4. Attacker Model

Attacker classes:
- malicious extension author
- compromised third-party extension package
- benign extension with buggy path/process logic abused by crafted inputs

Attacker goals:
- read sensitive files outside workspace
- escalate to arbitrary command execution
- bypass deny/prompt policy gates
- exfiltrate secrets via permissive hostcalls

## 5. Threat Catalogue (STRIDE-oriented)

### T1: Host filesystem read escape
- Vector: `node:fs` read path such as `/etc/hostname` or traversal-normalized absolute path.
- Impact: sensitive host data disclosure.
- Control:
  - host read fallback now constrained to runtime cwd root in `src/extensions_js.rs`.
  - canonical-path boundary check blocks outside-root reads.
- Evidence:
  - `tests/security_fs_escape.rs::host_read_fallback_denies_outside_workspace`
  - `tests/security_fs_escape.rs::read_file_traversal_with_dot_dot`
  - `tests/extensions_fs_shim.rs::fs_stat_host_fallback`

### T2: Symlink/path traversal escape
- Vector: `..` segments, symlink indirection, or crafted path normalization.
- Impact: read/write outside workspace or policy bypass.
- Control:
  - path normalization and rooted checks in VFS + tool path resolution.
  - deny on resolved outside-root host fallback path.
- Evidence:
  - `tests/security_fs_escape.rs` normalization/write confinement suite
  - `tests/extensions.rs::fs_connector_denies_path_traversal_outside_cwd`
  - `tests/extensions.rs::fs_connector_denies_symlink_escape`

### T3: Capability-policy bypass via method/capability mismatch
- Vector: forging `capability` different from method semantics.
- Impact: invoking privileged behavior through lower-privilege labels.
- Control:
  - `required_capability_for_host_call(...)` authoritative mapping.
  - dispatcher rejection for invalid/mismatched capability requests.
- Evidence:
  - `tests/extensions_policy_negative.rs` capability mapping tests
  - parity/adapter tests under `src/extensions.rs` unit suite

### T4: Dangerous hostcalls enabled unintentionally
- Vector: ambiguous defaults or unknown profile names.
- Impact: accidental `exec`/`env` exposure.
- Control:
  - default profile changed to `safe`.
  - unknown profile tokens fail closed to `safe`.
  - explicit opt-in path for dangerous caps (`allowDangerous`, profile overrides).
- Evidence:
  - `tests/capability_policy_scoped.rs` config resolution tests
  - `tests/e2e_cli.rs` explain/migration guardrail tests

### T5: Prompt fatigue or non-interactive denial ambiguity
- Vector: repeated prompt-required decisions causing operator mistakes.
- Impact: over-broad persistent grants.
- Control:
  - policy decision logs include reason/remediation metadata.
  - deny fallback when prompt manager/UI channel is unavailable.
- Evidence:
  - `tests/extensions_policy_negative.rs`
  - `tests/extensions.rs` prompt/denial path tests

## 6. Abuse-case Test Matrix

| Abuse case | Expected result | Test evidence |
|---|---|---|
| Read `/etc/hostname` from extension | denied (`outside extension root`) | `tests/security_fs_escape.rs::host_read_fallback_denies_outside_workspace` |
| Read workspace file via host fallback | allowed | `tests/security_fs_escape.rs::host_read_fallback_allows_workspace_file` |
| Traversal read `/fake/../etc/hostname` | denied | `tests/security_fs_escape.rs::read_file_traversal_with_dot_dot` |
| Stat/exists outside root via host fallback | denied/false | `tests/extensions_fs_shim.rs::fs_stat_host_fallback` |
| `exec` denied under safe defaults | deny | `tests/extensions_policy_negative.rs::exec_tool_denied_by_default_policy` |
| Non-dangerous default caps in safe mode | allow | `tests/capability_policy_scoped.rs::default_config_resolves_to_safe` |

## 7. Residual Gaps and Owners

| Gap | Risk | Owner | Tracking |
|---|---|---|---|
| Dangerous-capability operator rollout guidance published in `README.md` + `EXTENSIONS.md` | Mitigated | Capability policy UX owner | `bd-k5q5.4.7` (completed) |
| End-to-end abuse corpus across all extension categories | High | Conformance campaign owner | `bd-k5q5.2` / `bd-k5q5.2.4` |
| Full traceability mapping from every security test to requirements | Medium | Verification governance owner | `bd-k5q5.7.12` |

## 8. Verification Commands

Run targeted abuse/security checks:

```bash
cargo test --test security_fs_escape -- --nocapture
cargo test --test extensions_fs_shim fs_stat_host_fallback -- --nocapture
cargo test --test extensions_policy_negative -- --nocapture
cargo test --test capability_policy_scoped -- --nocapture
```

Run quality gates:

```bash
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```
