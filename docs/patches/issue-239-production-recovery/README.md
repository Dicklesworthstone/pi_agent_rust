# Issue #239: pending production terminal recovery

## Status

These are **unapplied candidate patches**, preserved in the repository rather
than only in conversation attachments. Committing this directory does not
change the production FTUI executable. Do not close #239 from this commit.
The native-ConHost trigger has not been reproduced or verified as fixed.

The series was reviewed against `src/interactive_ftui.rs` blob
`567ae5c8cca9d146d35ba0a644c73c2a2912e930`, still present on main at
`c7ffe8fd68ab2efb39ff57a00046e01e174393a1`. It preserves the independently
landed `7ae1181f7b6c63cd607e385741879a9a5a2f51a1` frame-budget mitigation.
That commit reports a tmux reproduction, not native-ConHost validation.

## Application order and intended behavior

Apply in filename order only after checking against a complete current
checkout. These files supersede the older conversation patch bundles; do not
apply both versions. Line numbers alone are not an application guarantee.

1. `01-production-console.patch`: use the existing console-input lease in the
   real launcher and editor handoff. Restore selection after the last release
   write; reapply native input flags before reacquisition output; retain the
   current frame budget and restore the lease before driver join/restart.
2. `02-terminal-recovery.patch`: distinguish fatal reacquisition from an
   ordinary editor failure. Always attempt reacquisition after a partial
   release, cancel live work on failure, stop the runtime, and preserve the
   terminal error outside the consumed model. Includes nine proposed tests.
3. `03-driver-lifecycle.patch`: send a terminal UI notification when the
   driver scope exits, even if an extension handler still owns an event
   sender. Stop bridge forwarding after that notification; retain a
   disconnect fallback and stop-aware sends. The shutdown hook cancels turns
   and pending prompts. Includes eleven proposed tests, including retained
   senders on return and unwind. Aborting panics do not run destructors.
4. `04-terminal-startup-admission.patch`: use the actual model init hook to
   admit runtime/session startup exactly once. Dropping an uninitialized model
   cancels startup instead of consuming buffered CLI prompts. Includes four
   proposed tests. This is terminal-construction admission, not proof that a
   first frame was presented successfully.

The series does not repair same-size diff-baseline invalidation after an
editor handoff, or prove that every ConHost-only stall has the same cause.

## Verification record

All four files parse as unified diffs. No complete-checkout application,
Rust compilation, rustfmt, Clippy, or runtime/native-Windows test pass is
claimed. The 24 proposed tests are source in patches, not active tests in the
current production test target. Existing console-helper tests are not counted
again. Uploaded patch blobs are checked against these local Git blob hashes:

| File | Git blob SHA |
| --- | --- |
| 01 | `77c4a26b7bde9409b3e0f4f76066fa9c5cbe9481` |
| 02 | `8ccb2aca63f932b93e577d904635ebf174cb2356` |
| 03 | `e15aca45c8b3095ebb9e72b3f36884cdacb77669` |
| 04 | `7eae508e59948d6a2d675e5931bb009b37f72dfe` |

On October 14, 2026, the required command was attempted:

```text
dsr quality --tool pi_agent_rust
bash: dsr: command not found
exit_status=127
```

GitHub writes were available for this publication, but direct checkout/source
downloads were blocked. The available write APIs accept whole-file content,
not a patch or a local source path. The large production module was therefore
left unchanged rather than replaced from truncated excerpts. No Cargo/RCH or
GitHub Actions quality bypass, dependency change, or PR was used. Production
integration and DSR verification remain outstanding.
