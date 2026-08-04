# Cross-Platform CI Matrix — LINUX

> Generated: 2026-08-04T07:06:47Z
> OS: linux / x86_64
> Required checks: 8/8 passed

## Check Results

| Check | Policy | Status | Tag |
|-------|--------|--------|-----|
| Cargo check compiles | required | PASS | - |
| Test infrastructure functional | required | PASS | - |
| Temp directory writable | required | PASS | - |
| Git CLI available | required | PASS | - |
| Conformance artifacts present | required | PASS | - |
| E2E TUI test support (tmux) | required | PASS | - |
| POSIX file permission support | informational | PASS | - |
| Extension test artifacts present | required | PASS | - |
| Evidence bundle index present | informational | PASS | - |
| Suite classification file present and valid | required | PASS | - |

## Merge Policy

| Platform | Role |
|----------|------|
| Linux | **Required** — all required checks must pass |
| macOS | Informational — failures logged, not blocking |
| Windows | Informational — failures logged, not blocking |

