# Browser Automation Tool: `browser`

> **Tool family:** Headless Chromium Automation (Opt-in)  
> **Bead ID:** `bd-cv653.2.4`  
> **Module:** `src/browser.rs`

---

## 1. Overview

The `browser` tool enables programmatic headless browser interaction via Chromium DevTools Protocol (CDP):
- Multi-tab lifecycle management (`open`, `goto`, `close`, `list_tabs`).
- In-page JavaScript evaluation (`evaluate`).
- Accessibility tree and DOM snapshotting (`snapshot`, `ax_tree`).
- User input simulation (`click`, `type`, `fill`, `press`, `scroll`, `wait_for`).
- Tab visual capture (`screenshot`).

---

## 2. Configuration & Security

```toml
[browser]
enable_browser = true
headless = true
remote_debugging_port = 9222
domain_allowlist = ["github.com", "docs.rs", "crates.io"]
```

Domain navigation is restricted against the configured `domain_allowlist` to prevent untrusted exfiltration.
