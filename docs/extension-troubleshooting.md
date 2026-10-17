# Extension Troubleshooting Guide

This guide covers common failure patterns in the extension runtime, the
conformance harness, and the capability policy system. Each section maps
a symptom to its root cause and a concrete fix.

## Hostcall Error Codes

When an extension call fails, the host returns a `HostcallOutcome::Error`
with one of these codes:

| Code              | Meaning                                           |
|-------------------|---------------------------------------------------|
| `denied`          | Capability policy rejected the operation           |
| `invalid_request` | Malformed payload, unknown operation, or bad args  |
| `timeout`         | Operation exceeded its budget                      |
| `io`              | Filesystem or network I/O failure                  |
| `internal`        | Unexpected host error (bug)                        |

## Policy Failures

### Symptom: `denied` error on `pi.exec()` or `pi.env()`

**Cause**: The default policy profile (`Standard`) denies `exec` and `env`
capabilities. Extensions that run shell commands or read environment
variables are blocked.

**Fix**:
```toml
# pi.toml
[extensions.policy]
profile = "standard"
allow_dangerous = true
```
Or use the CLI flag: `--extension-policy permissive`

### Symptom: `denied` after switching to `Safe` profile

**Cause**: The `Safe` profile uses `Strict` mode with only `read` and
`write` in `default_caps`. Capabilities like `http`, `events`, and
`session` are not in the allow list and will be denied without prompting.

**Fix**: Switch to `Standard` (allows non-dangerous caps, prompts for
dangerous ones) or `Permissive` (allows everything with audit logging).

### Symptom: Extension works but another extension is denied

**Cause**: Per-extension overrides in the policy. Check for
`per_extension` entries in the resolved policy.

**Diagnosis**:
```bash
# Check effective policy
pi info <extension-id>

# Or inspect the resolved config
pi config show | grep -A 20 extensions.policy
```

### Symptom: "Allow Always" not persisting

**Cause**: The `PermissionStore` writes to
`~/.pi/agent/extension_permissions.json`. If this file is unwritable or
the directory doesn't exist, decisions are session-only.

**Fix**: Ensure `~/.pi/agent/` exists and is writable.

### Policy Precedence (for debugging)

When a capability is evaluated, the 5-layer chain runs in order:

1. Per-extension `deny` list -- always Deny
2. Global `deny_caps` -- always Deny
3. Per-extension `allow` list -- always Allow
4. Global `default_caps` -- always Allow
5. Mode fallback -- Strict:Deny, Prompt:Prompt, Permissive:Allow

To diagnose which layer produced a decision, check the `reason` field
in `PolicyCheck`:
- `"extension_deny"` -- layer 1
- `"deny_caps"` -- layer 2
- `"extension_allow"` -- layer 3
- `"default_caps"` -- layer 4
- `"mode_strict"` / `"mode_prompt"` / `"mode_permissive"` -- layer 5

## Extension Loading Failures

### Symptom: "Do I need to convert JS extensions to descriptors first?"

**Answer**: No. Legacy `.js/.ts` extensions run directly in the embedded
QuickJS runtime. There is no required descriptor conversion step for normal
extension usage.

Descriptor entries (`*.native.json`) are an optional native-rust runtime path.
Current sessions must use one runtime family at a time (JS/TS or native
descriptor).

### Symptom: "Extension entry does not exist"

**Cause**: `JsExtensionLoadSpec::from_entry_path()` cannot find the file.

**Fix**:
- Verify the extension is installed: `ls ~/.pi/agent/extensions/`
- Check that `extension.json` exists in the extension directory
- Ensure `entry_path` in `extension.json` points to a valid `.js`/`.ts`

### Symptom: Extension loads but `activate()` never runs

**Cause**: The entry point doesn't call `pi.register()`. The QuickJS
runtime loads the file but registration requires an explicit call.

**Fix**: Ensure the extension's entry point calls:
```js
pi.register({
  name: "my-extension",
  version: "1.0.0",
  apiVersion: "1.0",
  capabilities: ["read", "session"],
  tools: [...],
  eventHooks: [...]
});
```

### Symptom: "Module not found" for Node built-in

**Cause**: The extension imports a Node module that isn't shimmed in
QuickJS.

**Shimmed modules**: `node:fs`, `node:path`, `node:os`, `node:crypto`,
`node:child_process`, `node:events`, `node:buffer`, `node:url`,
`node:http`, `node:net`, `node:readline`, `node:util`, `node:stream`

**Fix**: If the module is not in the list above, check if a virtual module
stub exists. If not, the extension may need a compatibility patch or the
shim needs to be added.

### Symptom: "Module not found" for npm package

**Cause**: QuickJS doesn't have a node_modules resolver. npm packages must
be either bundled into the extension or provided as virtual module stubs.

**Stubbed packages**: `glob`, `uuid`, `jsonwebtoken`, `shell-quote`,
`chalk`, `chokidar`, `jsdom`, `turndown`, `node-pty`,
`@opentelemetry/*`, `@xterm/*`, `vscode-languageserver-protocol`,
`@sinclair/typebox`, `@mariozechner/pi-ai`

**Fix**: If the package is used for a core feature, it needs a real shim.
If it's used for optional features (telemetry, IDE integration), a no-op
stub may suffice.

## Conformance Harness Failures

### Symptom: Test shows `N/A` instead of `PASS`/`FAIL`

**Cause**: The scenario requires a harness capability that isn't
implemented yet. Common missing capabilities:
- `mock_http` -- HTTP response mocking for extensions that make requests
- `mock_model_registry` -- model registry mocking for provider tests
- `mock_exec` -- subprocess output mocking

**Diagnosis**: Check the `skip_reason` field in the parity log:
```json
{"status":"skip","skip_reason":"requires mock_http"}
```

**Fix**: These are tracked as conformance evidence gaps. See
`tests/ext_conformance/reports/CONFORMANCE_REPORT.md` for the full
classification.

### Symptom: Conformance diff shows false positive

**Cause**: Non-deterministic output (timestamps, paths, random values)
differs between TS oracle and Rust runtime.

**Fix**:
- Set `PI_TEST_MODE=1` to stabilize timestamps and CWD
- Set `PI_CONFORMANCE_SEED=42` for deterministic conformance diffs
- Set `PI_EXT_RANDOM_SEED=42 PI_EXT_RANDOM_N=1` for bounded random-trial smoke runs
- Use path canonicalization assertions (suffix matching, not exact)
- Check `docs/extension-architecture.md` for normalization details

### Symptom: TS oracle times out

**Cause**: The TypeScript oracle (Bun-based) has a default timeout of 30s
per extension. Complex extensions or slow machines may exceed this.

**Fix**:
```bash
export PI_TS_ORACLE_TIMEOUT_SECS=60
```

The harness includes retry logic for flaky oracle timeouts.

### Symptom: Conformance test can't find Bun

**Cause**: The harness expects Bun at `/home/ubuntu/.bun/bin/bun`.

**Fix**:
```bash
# Install Bun
curl -fsSL https://bun.sh/install | bash

# Or symlink an existing installation
ln -sf $(which bun) /home/ubuntu/.bun/bin/bun
```

### Symptom: "npm ci" fails in legacy_pi_mono_code

**Cause**: The TS oracle depends on `legacy_pi_mono_code/pi-mono/` having
its npm dependencies installed.

**Fix**:
```bash
cd legacy_pi_mono_code/pi-mono
npm ci
```

## Session and State Failures

### Symptom: `pi.session("setLabel")` returns null

**Cause**: `Session::add_label` requires the target entry to exist in the
session. If the `target_id` doesn't match any entry, it returns `None`.

**Fix**: Ensure the message/entry exists before labeling. Use
`pi.session("getEntries")` to verify the target ID.

### Symptom: Session operations fail with "no session"

**Cause**: The `ExtensionManager` doesn't have a session attached. This
happens in:
- Test environments without `set_session()` call
- Non-interactive CLI modes (`--print` mode)

**Fix**: For tests, attach a real session:
```rust
let session = SessionHandle(Arc::new(Mutex::new(Session::create())));
manager.set_session(Arc::new(session) as Arc<dyn ExtensionSession>);
```

## Filesystem Escape Patterns

These are security-tested failure modes (see `tests/security_fs_escape.rs`):

| Attack                  | Control                                |
|-------------------------|----------------------------------------|
| `../../etc/passwd`      | Path canonicalization + root check     |
| Symlink to `/etc`       | `canonicalize()` resolves real path    |
| `//server/share`        | UNC path detection                     |
| `/dev/null` read        | Device file exclusion                  |
| Very long path          | Path length limit check                |

Extensions cannot read files outside the working directory root through
the `host_read_fallback` mechanism.

## Structured Concurrency Failures

### Symptom: Extension cleanup hangs at session end

**Cause**: The `ExtensionRegion` shutdown budget (default 5s) may not be
enough for extensions with long-running operations.

**Fix**:
```rust
ExtensionRegion::with_budget(manager, Duration::from_secs(15))
```

### Symptom: Hostcalls fail with "shutdown" after session end

**Cause**: The `JsRuntimeHost` holds a `Weak<Mutex<ExtensionManagerInner>>`
reference. After the `ExtensionManager` is dropped, the weak reference
fails to upgrade, and all hostcalls return `Deny` with reason `"shutdown"`.

**Fix**: This is by design. Extensions should handle shutdown gracefully
and not issue hostcalls during cleanup.

### Symptom: "Budget exceeded" errors

**Cause**: `effective_timeout()` intersects the manager's remaining budget
with the per-operation timeout. If the manager budget is nearly exhausted,
even short operations may time out.

**Diagnosis**: Check `extension_budget` remaining time vs operation
timeout.

## Provider Extension Failures

### Symptom: Custom `streamSimple` provider returns empty responses

**Cause**: The JS `streamSimple()` function must return an
`AsyncIterable` of text chunks or structured assistant events. Since
v0.7.0 an iterator that yields nothing, or a structured stream that ends
without a `done`/`error` terminal, fails with
`PI_EXTENSION_STREAM_INCOMPLETE` instead of completing as an empty reply. An
`error` terminal must carry `reason: "error"` (or `"aborted"`) matching its
message's `stopReason`; anything else is `PI_EXTENSION_STREAM_PROTOCOL`.

**Fix**: Ensure `streamSimple` is an async generator:
```js
async function* streamSimple(model, context, options) {
  yield "Hello ";
  yield "world";
}
```

### Symptom: OAuth token refresh fails

**Cause**: The `refresh_extension_oauth_token()` function expects valid
`OAuthConfig` on the `ModelEntry`. Missing `token_url` or `client_id`
will cause the refresh to fail.

**Fix**: Verify the provider registration includes complete OAuth config:
```js
pi.events("registerProvider", {
  name: "my-provider",
  models: [{ id: "model-1", oauth: {
    authUrl: "...",
    tokenUrl: "...",
    clientId: "...",
    scopes: ["read"]
  }}]
});
```

## Quick Reference

| Error                         | Likely Cause            | First Step                    |
|-------------------------------|-------------------------|-------------------------------|
| `denied`                      | Policy blocks capability| Check profile + deny_caps     |
| `invalid_request`             | Bad payload/op name     | Check JS call args            |
| `timeout`                     | Budget exhausted        | Increase timeout/budget       |
| Module not found              | Missing shim            | Check shimmed module list     |
| N/A in conformance            | Missing harness feature | Check skip_reason in log      |
| Session op returns null       | Missing session/entry   | Attach session, verify ID     |
| Extension not loading         | Missing extension.json  | Check install directory       |
| Cleanup hangs                 | Insufficient budget     | Increase ExtensionRegion budget|
