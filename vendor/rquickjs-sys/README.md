# rquickjs-sys

[![github](https://img.shields.io/badge/github-delskayn/rquickjs-8da0cb.svg?style=for-the-badge&logo=github)](https://github.com/DelSkayn/rquickjs)
[![crates](https://img.shields.io/crates/v/rquickjs.svg?style=for-the-badge&color=fc8d62&logo=rust)](https://crates.io/crates/rquickjs-sys)
[![docs](https://img.shields.io/badge/docs.rs-rquickjs-66c2a5?style=for-the-badge&logo=data:image/svg+xml;base64,PHN2ZyByb2xlPSJpbWciIHhtbG5zPSJodHRwOi8vd3d3LnczLm9yZy8yMDAwL3N2ZyIgdmlld0JveD0iMCAwIDUxMiA1MTIiPjxwYXRoIGZpbGw9IiNmNWY1ZjUiIGQ9Ik00ODguNiAyNTAuMkwzOTIgMjE0VjEwNS41YzAtMTUtOS4zLTI4LjQtMjMuNC0zMy43bC0xMDAtMzcuNWMtOC4xLTMuMS0xNy4xLTMuMS0yNS4zIDBsLTEwMCAzNy41Yy0xNC4xIDUuMy0yMy40IDE4LjctMjMuNCAzMy43VjIxNGwtOTYuNiAzNi4yQzkuMyAyNTUuNSAwIDI2OC45IDAgMjgzLjlWMzk0YzAgMTMuNiA3LjcgMjYuMSAxOS45IDMyLjJsMTAwIDUwYzEwLjEgNS4xIDIyLjEgNS4xIDMyLjIgMGwxMDMuOS01MiAxMDMuOSA1MmMxMC4xIDUuMSAyMi4xIDUuMSAzMi4yIDBsMTAwLTUwYzEyLjItNi4xIDE5LjktMTguNiAxOS45LTMyLjJWMjgzLjljMC0xNS05LjMtMjguNC0yMy40LTMzLjd6TTM1OCAyMTQuOGwtODUgMzEuOXYtNjguMmw4NS0zN3Y3My4zek0xNTQgMTA0LjFsMTAyLTM4LjIgMTAyIDM4LjJ2LjZsLTEwMiA0MS40LTEwMi00MS40di0uNnptODQgMjkxLjFsLTg1IDQyLjV2LTc5LjFsODUtMzguOHY3NS40em0wLTExMmwtMTAyIDQxLjQtMTAyLTQxLjR2LS42bDEwMi0zOC4yIDEwMiAzOC4ydi42em0yNDAgMTEybC04NSA0Mi41di03OS4xbDg1LTM4Ljh2NzUuNHptMC0xMTJsLTEwMiA0MS40LTEwMi00MS40di0uNmwxMDItMzguMiAxMDIgMzguMnYuNnoiPjwvcGF0aD48L3N2Zz4K)](https://docs.rs/rquickjs-sys)
[![status](https://img.shields.io/github/actions/workflow/status/DelSkayn/rquickjs/ci.yml?branch=master&style=for-the-badge&logo=github-actions&logoColor=white)](https://github.com/DelSkayn/rquickjs/actions?query=workflow%3ARust)

This crate is a low level unsafe raw bindings for the [QuickJS](https://bellard.org/quickjs/) JavaScript engine.

__NOTE:__ Usually you shouldn't use this crate directly, instead use [rquickjs](https://crates.io/crates/rquickjs) crate which provides high level safe bindings.

## Patches

In order to fix bugs and get support for some unimplemented features the series of patches applies to released sources.

Hot fixes:
- Fix for _check stack overflow_ (important for Rust)
- Atomic support for `JS_NewClassID` (important for Rust)
- Infinity handling (replacement `1.0 / 0.0` to `INFINITY` constant)

Special patches:
- Reading module exports (`exports` feature)
- Reset stack function (`parallel` feature)
- MSVC support

## Environment Variables

The following environment variables can be used to control which WASI SDK is using during the build:

- `WASI_SDK`: Path to the WASI SDK to use during the build. If unset, this crate will download it automatically instead.
- `RQUICKJS_SYS_NO_WASI_SDK`: If set to `1`, this crate will not attempt to use the WASI SDK
  and instead the `CC`, `AR`, and `CFLAGS` environment variables must be appropriately set.
