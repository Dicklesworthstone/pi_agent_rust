# Web-Remote Access Threat Model & Security Posture

## Current implementation: supervised loopback companion

The implemented `pi web` path is documented in the
[browser companion guide](../tools/web_remote.md). It serves bounded completed
conversation snapshots over authenticated loopback HTTP. Prompt proposals
require explicit local F8 acceptance and a final session/credential check at
driver admission. It does not expose raw terminal frames, WebSockets, Tailscale
binding, or unattended remote control.

Current controls include 256-bit single-use pairing links, hashed credentials,
exact Host and Origin checks, a restrictive CSP, privacy discovery before
omission or clipping, non-restorable redaction, bounded memory and connection
lifetimes, and revocation of pending proposals. The in-memory audit ring holds
metadata only. Browser extensions or a compromised browser can still read data
already delivered to that browser; in-memory storage and CSP do not prevent
that access.

The real listener and native admission regressions are in
`src/web_remote/companion/tests.rs`, `src/web_remote/companion/projection.rs`,
and `src/interactive_ftui.rs`. Their authoritative execution path is DSR.

## Explicit terminal mirror

`pi web --terminal` uses `WebRemoteServer` and `WebRemoteBridge` as a separate
authenticated WebSocket surface, also available to embedding hosts. It supports terminal keyframes and
patches, local approval of control leases, and bounded sequenced input with
provenance revalidated at admission, native consumption, and agent submission.
Host input revokes control. The approval panel must visibly contain the complete
client identity and decision hint; small terminals cannot silently approve a
clipped request. A separate remote composer preserves the local draft.

The mirror requires fullscreen FTUI and a local terminal. Loopback is default;
Tailscale and LAN binding require explicit flags. Pairing tokens remain in
memory, expire after ten minutes, and are consumed once. Exact Host/Origin
checks, a strict CSP, request/write deadlines, bounded input queues, and bounded
frame history constrain the transport. The server is owned by the native
session and joins its connections at shutdown.

Complete source blocks are screened before wrapping or clipping with immutable,
revisioned privacy scopes. A vault change retires a stale scoped publication.
Local drafts, authentication/questions, in-flight response text, and truncated
live tool cards are withheld from the browser projection. Completed safe
transcript and status updates remain visible. Credentials learned in previous
session states remain protected within the bounded listener history.

Its socket regressions live in
`src/web_remote/server.rs`; manager and privacy coverage also appears in
`tests/web_remote.rs`, `tests/web_security.rs`, and `src/interactive_ftui.rs`.
Only the selected browser mode starts. Mirror credentials and leases cannot
authorize companion proposals. Neither implementation establishes WASM,
headless, QR, or a passed runtime security gate; DSR execution remains required.

## Historical design: WASM remote terminal

The sections below retain the earlier design and its proposed acceptance
criteria. They do not describe the current companion or establish a passed
security gate for it.

> **Status:** Historical design, not the current runtime contract
> **Bead ID:** `bd-cv653.10.3`  
> **Schema:** `pi.web.threat_model.v1`  
> **Feature:** `pi web` (WASM Browser Client over WebSocket Frame Diffs)

---

## 1. Executive Summary

The `pi web` capability provides relay-free browser access to a live Pi Agent session over local loopback or Tailscale encrypted networks. By architecture, this capability is **fail-closed** and treats browser clients as untrusted viewing/input endpoints.

---

## 2. Threat Analysis & Adversary Model

| Threat ID | Adversary Profile | Attack Vector | Security Impact | Mitigating Control |
|-----------|-------------------|---------------|-----------------|--------------------|
| **T-01** | Tailnet Lateral Attacker | Port scan on Tailscale subnet, unauthorized WebSocket connection attempt | Unauthorized session observation or command injection | **Single-use fragment tokens (>=128-bit entropy)** + default loopback binding. WS connection without valid token rejected. |
| **T-02** | Shoulder Surfer / Token Leaker | Token copied from URL or shared via messaging | Unauthorized access after intended session | **Single-use token consumption upon WS handshake** + 10-minute expiry + explicit revoke commands. |
| **T-03** | Malicious Remote Viewer | Viewer sends unauthorized mutating commands (e.g. `rm`, `write`, `bash`) | Arbitrary remote code execution on host | **Input Arbitration FSM**: Remote actions route through the **LOCAL approval gate**. Remote viewers cannot self-approve mutating tools. |
| **T-04** | Secret Exfiltration via Frames | Agent output containing API keys / tokens is streamed to web client | Credential compromise | **Secrets Vault Obfuscation**: Frame streamer replaces all detected secrets with reversible vault placeholders (`[SECRET_***]`) before transmission. |
| **T-05** | Browser Client XSS / Storage Leak | Malicious browser extension or script attempts to read cached session data | Data leak across tabs or extensions | **Zero Browser Persistence**: Frames exist in DOM/memory only. **Strict CSP** forbids external scripts and inline evaluations. No `localStorage`, `sessionStorage`, or `indexedDB` usage. |
| **T-06** | Cross-Origin WebSocket Hijacking (CSWSH) | Malicious website initiates WS connection to local `pi web` port | CSRF-like hijacking of local agent | **Strict Origin Verification**: WS handler enforces exact host/origin matching. CORS disabled. |
| **T-07** | DoS via Connection Flood | Attacker floods WS handshake or input buffer | Host resource exhaustion | **Connection Cap (default: 4 concurrent viewers)** + input rate limiting. Excess connections receive a 503 wait page. |

---

## 3. Data Flow Diagram

```
[ Local Terminal ] <--------------------+
       |                                | (Local Approval Prompt)
       v                                v
[ Pi Agent Engine ] ---> [ Secrets Vault (Obfuscation) ]
                                |
                                v (pi.web.frame.v1)
                    [ Web-Remote Manager ]
                                |
        +-----------------------+-----------------------+
        | (WebSocket /ws)                               | (HTTP GET /)
        v                                               v
[ Remote Browser (Thin WASM) ]                 [ Static HTML/JS Asset ]
- Zero persistence in Storage                  - Strict Content-Security-Policy
- Applies DOM patches only                     - Subresource Integrity (SRI)
```

---

## 4. Audit & Traceability Contract

All security-relevant web events are recorded in the structured audit log conforming to schema `pi.web.audit.v1`:

- `client_connected`: Recorded with timestamp, client ID, and remote socket address.
- `client_disconnected`: Recorded when WS session terminates.
- `takeover_requested`: Recorded when remote viewer requests steering control.
- `takeover_granted`: Recorded when control is transitioned.
- `control_released_to_local`: Recorded when control returns to host terminal.
- `approval_prompt_issued`: Recorded when mutating tool call triggers local gate.
- `approval_decision`: Recorded with operator decision and actor provenance.

---

## 5. Acceptance & Conformance Gate

To satisfy release criteria:
1. All unit and security test suites in `tests/web_remote.rs` and `tests/web_security.rs` MUST pass.
2. Static inspection MUST verify zero `localStorage` / `indexedDB` invocations in web assets.
3. Obfuscation integration MUST prove placeholder delivery over WS frames.
