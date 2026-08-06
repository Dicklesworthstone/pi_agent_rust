# Troubleshooting

This page collects common failures and practical fixes. If a behavior is still
being implemented, the relevant bead ID is listed for tracking.

## API keys and auth

**Symptom:** `Missing API key` or provider auth errors.

**Fixes:**
- Use env vars: `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GOOGLE_API_KEY`, etc.
- Or set `--api-key` per run.
- Or store credentials in `~/.pi/agent/auth.json` via `/login` (Anthropic OAuth).

**Config precedence (most → least):**
1. `--api-key`
2. Unexpired OAuth/bearer credential in `auth.json`
3. Provider-specific environment variables, in the order declared by provider metadata
4. Stored API key in `auth.json`
5. Supported external coding-CLI credential (global auth storage only)
6. Inline `models.json` provider `apiKey` fallback, when the selected model supplies one

Provider-managed credentials are exceptions to the generic API-key chain:
Bedrock resolves the AWS credential chain/SigV4 at request time, and SAP AI
Core exchanges its client credentials for a bearer token rather than sending a
client secret as an API token.

## Provider errors (401/429/5xx)

**401/403:** Key missing or invalid. Confirm correct provider and key.

**429:** Rate limit. Pi will retry if `retry.enabled` is true in settings.

**5xx/network:** Temporary provider outage or flaky network. Retry or switch models.

**Retry config** lives in `~/.pi/agent/settings.json`:
```json
{
  "retry": {
    "enabled": true,
    "maxRetries": 3,
    "baseDelayMs": 1000,
    "maxDelayMs": 30000
  }
}
```

## VCR mode (tests)

Tests that exercise provider streaming use recorded cassettes for determinism.

Environment variables:
- `VCR_MODE=record|playback|auto`
- `VCR_CASSETTE_DIR=tests/fixtures/vcr` (default)

Common fixes:
- Missing cassette: run with `VCR_MODE=record` once, then commit the cassette.
- No network in CI: use `VCR_MODE=playback`.
- Invalid `VCR_MODE`: only `record`, `playback`, or `auto` are accepted.

## Packages and extensions

**Symptom:** extensions or skills not found.

**Fixes:**
- Check package sources via `pi list`.
- Confirm settings in `~/.pi/agent/settings.json` or `.pi/settings.json`.
- Re-run `pi update` after adding a source.

Extension discovery is tracked under **bd-1e0** (install + resolution). If an
extension fails to load, expect diagnostics to improve as that bead lands.

## Sessions (persistence + recovery)

Sessions live under:
```
~/.pi/agent/sessions/
```

Overrides:
- `PI_CODING_AGENT_DIR` (global base)
- `PI_SESSIONS_DIR` (sessions root)

**Corruption recovery:**
- Run with `--no-session` to bypass persistence.
- Move the offending `.jsonl` file out of the sessions dir.

Interactive UX parity for `/resume`, `/tree`, `/fork` is tracked by **bd-14cc**.

## Keybindings & hotkeys

Keybindings are loaded from:
```
~/.pi/agent/keybindings.json
```

If shortcuts don’t work as expected:
- Delete/rename the file to fall back to defaults.
- Confirm your terminal isn’t intercepting the keys.

Full keybinding parity (including `/hotkeys`) is tracked by **bd-3ip**.

## Terminal quirks

Some terminals reserve key combos (especially on Windows):
- `Ctrl+Enter` / `Alt+Enter` may be intercepted.
- Paste events can differ between terminals.

If a shortcut doesn’t trigger, try a different terminal or remap the key.
Interactive editor parity (autocomplete/bang/paste) is tracked by **bd-1iwi**.

## Missing system dependencies

The `find` tool requires `fd`:
```bash
# Ubuntu/Debian
apt install fd-find

# macOS
brew install fd

# The binary might be named fdfind
ln -s $(which fdfind) ~/.local/bin/fd
```

`rg` (ripgrep) is optional but recommended for faster searches.

## Tool output truncated

Large tool outputs are truncated to protect the context window. Ask for
specific ranges (e.g., “Read lines 2000-4000 of that file”).
