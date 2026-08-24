# Web-Remote Access: `pi web`

> **Feature:** WASM Web Remote & QR Console Pairing  
> **Bead IDs:** `bd-cv653.10.1`, `bd-cv653.10.2`, `bd-cv653.10.3`  
> **Module:** `src/web_remote.rs`

---

## 1. Overview

`pi web` serves the live terminal agent interface over a WebSocket frame diff stream to thin browser clients across Tailscale or loopback networks.

---

## 2. Usage & Options

```bash
# Start local web-remote server on default port 8080
pi web

# Bind to Tailscale IP in view-only mode
pi web --bind tailscale --view-only --port 9000
```

---

## 3. Security Architecture

- **Token FSM:** Ephemeral URL fragment tokens (`#t=<token>`) with 10-minute expiry and single-use consumption.
- **Input Arbitration:** Remote input requires local approval; local operator can revoke remote control at any moment.
- **Zero Browser Persistence:** Frames are held in memory only; strict CSP forbids third-party storage or scripts.
- **Audit Ledger:** All connections, takeovers, and inputs are recorded in `pi.web.audit.v1` format.
