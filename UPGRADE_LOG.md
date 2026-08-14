# Dependency Upgrade Log

**Date:** 2026-08-14  |  **Project:** pi_agent_rust  |  **Language:** Rust (nightly-2026-07-05)

## Summary

- **Updated:** 13 direct majors/minors + full semver lock refresh (~120 packages)
- **Deliberately deferred:** 1 (FrankenSQLite cutover — blocked upstream, see below)
- **Regressions introduced:** 0 (all 29 residual lib-test failures reproduced identically at the pre-upgrade baseline: sandbox lacks `rg`/`fd`; macOS `/var`-symlink TMPDIR; macOS `linkat` semantics)

## Headline updates

### asupersync: 0.3.9 → 0.4.4
- **Breaking (library-wide):** only `TrackedSender::try_reserve` gained a `&Cx` param (v0.3.10) — pi does not use `channel::session`; zero call-site changes needed.
- **Behavioral audit performed:** no `JoinError::Cancelled` assertions around aborts (v0.4.4 cancellation-result contract N/A); TLS connectors all use `.with_webpki_roots()` so the new empty-root-store fail-closed hardening is inert; `run_test` logging is now scoped/thread-local (no pi test depended on the old global subscriber); no `io::copy`-under-select usage.
- **Tests:** full `cargo test --lib` = 7040 passed; failures baseline-identical.

### sqlmodel-sqlite 0.2.2 → 0.3.3, sqlmodel-core 0.2.2 → 0.3.2
- Additive for pi's synchronous API usage; adds the exact native error-code API for future use.
- **Note:** sqlmodel-sqlite 0.3.x pins `asupersync =0.3.10`, so the lockfile now carries two asupersync versions (0.3.10 inside sqlmodel, 0.4.4 for pi). pi only calls sqlmodel's *sync* API, so the 0.3.10 copy is dead code; LTO + `codegen-units=1` should strip it, but **verify the 22 MiB release size budget** on the next release build.

### FrankenSQLite (fsqlite): cutover prepared, deliberately deferred
Goal state is fsqlite (pure-Rust, asupersync-native, MVCC) replacing sqlmodel-sqlite + libsqlite3-sys. Blocked today on three verified facts:
1. Registry `fsqlite 0.3.1` carries the open P0 **bd-r82et** (false `BusySnapshot` on single-writer multi-statement DDL batches — exactly pi's `INIT_SQL`/schema-bootstrap shape). The fix is merged in the frankensqlite working tree but **not published**; no 0.3.2 release is staged.
2. pi publishes to crates.io, so a git-pinned fsqlite dep cannot ship; `[patch.crates-io]` would silently ship the buggy registry code in releases.
3. pi's session index is **multi-process** and would transiently mix engines with older installed pi binaries (libsqlite3) against the same DB file — explicitly unsupported by the fsqlite concurrency contract (multi-process is `partial`; open P0s bd-9inpb/bd-zywqc).

**Unblock path:** publish fsqlite 0.3.2 (fix ≥ `f1c24fab4`), then swap the 6 sqlmodel-consuming files (`session_sqlite.rs`, `session_index.rs`, `session_picker.rs`, `doctor.rs`, `error.rs`, `error_hints.rs`; complete call-site spec captured during this pass), enable `strict_multi_process`, keep the DirLock single-writer discipline, batch statements per `block_on` (754 ns/op re-entry cost), and re-verify the two `Display`-string dependencies (`"no such table: pi_session_meta"`, `"locked"/"busy"`) against fsqlite's error text. Tracked in bead (see .beads).

## Other updates

### RustCrypto digest family → 0.11 generation
sha2/sha1/md-5 0.10 → 0.11.0, hmac 0.12 → 0.13.0, pbkdf2 0.12.2 → 0.13.0, scrypt 0.11 → 0.12.
- **Breaking:** digest outputs lost `LowerHex` (generic-array → hybrid-array); ~60 `format!("{:x}")` sites now route through the shared `hex_encode` (byte-identical output — cache keys/hashline tags unchanged); `hmac::KeyInit` import; scrypt `Params::new` dropped the `len` arg (out-of-range keylens now succeed, closer to Node semantics).
- Known-answer vectors all green (crypto_shim 43/43, auth 367/367).

### rquickjs 0.11 → 0.12.2
- **Breaking:** `Loader::load`/`Resolver::resolve` gained an `ImportAttributes` param (ignored, preserving 0.11 semantics). Same QuickJS-ng lineage, no engine swap. extensions_js 189/189.

### swc family (TypeScript transpiler) → newest coherent set
common 18→26, ast 20→29, parser 34→45, codegen 23→32, transforms_base 37→49, transforms_typescript 41→55, visit 20→29.
- Zero API changes needed (code already on current API shape). `COMPILED_MODULE_CACHE_VERSION` bumped 3→4 so stale transpile-cache entries can never be read back.

### fs4 0.13 → 1.1
- **Breaking:** `fs4::fs_std::FileExt` → root `fs4::FileExt`; `lock_exclusive()` → `lock()`; `try_lock_exclusive() -> io::Result<bool>` → `try_lock() -> Result<(), TryLockError>` (WouldBlock now a typed variant). 6 src + 3 test call sites migrated.

### base64 0.22 → 0.23
- No code changes required (Engine API stable across the bump).

### scripts/ (npm): @earendil-works/pi-ai 0.82.1 → 0.84.0
- Exact-pinned per repo convention; catalog generator verified against 0.84.0 (drift vs the vendored snapshot is the normal post-bump state; catalog refresh is a separate deliberate step).

### Semver lock refresh (`cargo update`)
~120 packages refreshed (clap 4.6.6, chrono 0.4.45, regex 1.13.1, cc 1.4.3, etc.).

## Not updated (deliberate)

- **wasmtime 47.0.3, sysinfo 0.39.6, crossterm 0.29, image 0.25, crc32c, xxhash-rust, ring 0.17.14** — already at latest stable.
- **charmed-* 0.2.0, rich_rust 0.2.2** — latest published versions of the sibling TUI stack.
- **loom git fork pin** — intentional (upstream stack-size limitation), preserved.

## Incidental fix landed during verification

- Pre-existing `ts_multiple_extensions_loaded` e2e failure (verified failing before today's changes): `collect_js_extension_roots` keyed exclusive ownership per parent directory, so two single-file extensions in one `extensions/` dir always collided. Ownership is now per-entry-file inside an independent-extensions root (`e2e_ts_extension_loading` 128/128).
