# Extension System (Big‑Guns Plan)

This document defines the extension architecture for **pi_agent_rust** with the
goal of **maximum compatibility**, **formal safety guarantees**, and **measurable
performance**. The system is **best‑effort** by default, but designed to
converge to full parity with legacy Pi extensions.

---

## 0. Design Goals

1. **Compatibility**: run legacy Pi extensions with best‑effort fidelity.
2. **Performance**: <2ms p95 overhead per tool call (excluding tool work).
3. **Safety**: explicit, auditable capability grants with optional strict mode.
4. **Stability**: versioned protocol + conformance fixtures.
5. **Portability**: same artifact runs on Linux/macOS/Windows.

Non‑goals:
- Custom TUI rendering from extensions (core owns the UI).
- Node‑native addons (must use hostcalls or WASM).

---

## 1. Runtime Tiers (Hybrid, Best‑of‑All Worlds)

**Tier A — WASM Component (default):**
- Fast, sandboxed, portable.
- Typed hostcalls via WIT.

**Tier B — JS Compatibility (compiled):**
- Legacy TS/JS compiled to a single bundle.
- Pre‑compiled to **QuickJS bytecode** or **JS→WASM**.
- No Node runtime required.

**Tier C — MCP (process IPC):**
- For heavy integrations: IDEs, databases, cloud services.

> WASM is the default. JS compatibility is a **compile step**, not a runtime.

---

## 2. Artifact Pipeline (Legacy → Optimized)

**Inputs**
- `extension.json` (manifest)
- Source files (TS/JS or Rust/WASM)

**Pipeline**
1. **SWC build**: TS/JS → bundle (tree‑shaken/minified).
2. **Compatibility scan**: static analysis for forbidden APIs.
3. **Protocol shim**: rewrite legacy extension imports to hostcalls.
4. **Artifact build**:
   - **QuickJS bytecode** (fast startup), or
   - **WASM component** (portable + sandboxed).
5. **Cache** by hash:
   ```
   hash = sha256(manifest + bundle + engine_version)
   ```

**Output**
- `extension.artifact` + `artifact.json` (metadata, engine, hash, caps)

---

## 3. Extension Protocol (v1)

All communication uses a **versioned, JSON‑encoded protocol**:
`docs/schema/extension_protocol.json`.

Core message types:
- `register`
- `tool_call` / `tool_result`
- `slash_command` / `slash_result`
- `event_hook`
- `log` / `error`

WASM components use the **WIT interface** in `docs/wit/extension.wit`.

---

## 4. Capability Policy (Configurable Modes)

`extensions.policy.mode` supports:
- `strict`: deny by default, explicit grants required.
- `prompt`: ask once per capability.
- `permissive`: allow most; warn and log.

Suggested config (document‑only for now):
```json
{
  "extensions": {
    "policy": {
      "mode": "prompt",
      "max_memory_mb": 256,
      "default_caps": ["read", "write", "http"],
      "deny_caps": ["exec", "env"]
    }
  }
}
```

Capabilities are enforced per‑hostcall and logged in an **audit ledger**.

---

## 5. Alien‑Artifact Safety (Formal Decisioning)

We apply a **loss‑aware, evidence‑driven** model to decide capability grants.

**Evidence Ledger** (example):
```
E = { uses_fs: 0.8, uses_exec: 0.1, unsigned: 0.6, size_mb: 0.2 }
```

**Loss matrix** (risk‑averse):
```
           | grant | deny |
-----------+-------+------+
benign     |   0   |   2  |
malicious  | 100   |   1  |
```

Decision rule: grant if expected loss is lower. This supports **strict** and
**prompt** modes with mathematically traceable decisions.

> This is intentionally conservative: false‑deny is cheap; false‑grant is costly.

---

## 6. Conformance Harness

**Golden fixtures** record legacy behavior and validate the compiled artifact.

Process:
1. Capture legacy extension outputs → fixtures JSON.
2. Replay with the compiled artifact.
3. Compare outputs byte‑for‑byte (or normalized where specified).

Artifacts:
- `tests/ext_conformance/fixtures/*.json`
- `tests/ext_conformance/*.rs`

---

## 7. Performance Harness (Extreme Optimization Loop)

Benchmarks:
- Startup (`pi --version`)
- First tool call latency
- Streaming tool throughput

Loop:
1. **Baseline** (hyperfine)
2. **Profile** (flamegraph)
3. **Change one lever**
4. **Prove** isomorphism (golden outputs)
5. **Re‑profile**

---

## 8. Best‑Effort Compatibility Rules

Compatibility scanner outputs:
- **compatible** (safe)
- **warning** (works but constrained)
- **blocked** (unsafe / unsupported)

The system always **tries to run** with warnings unless `strict` is set.

---

## 9. Next Implementation Steps

1. Implement the protocol structs + JSON schema validation.
2. Add the WASM host scaffold + capability checks.
3. Build the SWC‑based `extc` pipeline + cache.
4. Create conformance fixtures from legacy Pi extensions.
