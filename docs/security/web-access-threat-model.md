# Web-Remote Access Threat Model & Security Posture

> **Status:** Active Security Gate  
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
