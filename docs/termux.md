# Android (Termux) Notes

Pi can run on Android via [Termux](https://termux.dev/), but some features are limited by the
mobile environment.

## Prerequisites

1. Install **Termux** from F-Droid or GitHub (the Play Store build is deprecated).
2. Install **Termux:API** (required for clipboard integration).
3. In Termux:
   ```bash
   pkg update && pkg upgrade
   pkg install termux-api git
   ```

Pi detects `termux-clipboard-get` and `termux-clipboard-set` when standard clipboard access fails.

> Note: `pi_agent_rust` requires **Rust nightly**. If Termux can’t provide nightly easily,
> build the binary on a desktop and copy it over.

## Clipboard Support

- **Text clipboard**: Works via `termux-clipboard-get` / `termux-clipboard-set`.
- **Image clipboard**: **Not supported** on Termux (the `Ctrl+V` image paste flow will no-op).

## Terminal Quirks

- If arrow keys or shortcuts misbehave, configure the **extra keys row** in Termux settings.
- Some terminals send `Ctrl+Enter` instead of `Shift+Enter` for “insert newline” behavior.

## Storage

- Sessions live in `~/.pi/agent/sessions`.
- To access shared storage (Downloads/Documents), run once:
  ```bash
  termux-setup-storage
  ```

## Troubleshooting

### Clipboard not working

Ensure both apps are installed:
1. Termux (from F-Droid/GitHub)
2. Termux:API

Then install the CLI tools:
```bash
pkg install termux-api
```

### Permission denied for shared storage

Run once to grant storage permissions:
```bash
termux-setup-storage
```
