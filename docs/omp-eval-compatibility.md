# OMP eval input compatibility and native image output

The native `eval` tool accepts OMP-style `language` and `timeout` fields as
aliases for Pi's existing `kernel` and `timeout_secs` fields. Existing native
calls are unchanged. OMP's current single-cell language tokens are `py` and
`js`; Pi additionally accepts `python` and `javascript`.

```json
{"language":"js","code":"globalThis.answer = 42; answer","timeout":10}
```

A later native call shares the same language/session namespace:

```json
{"kernel":"js","code":"answer"}
```

A requested language is never ignored in favor of Python. Unknown values,
including `auto`, fail before admission or reset. Supplying both aliases for
one field (even with matching values) is an error, rather than an implicit
precedence rule. A rejected reset request does not discard either kernel.

Timeout values retain Pi's native policy: nonnegative integer seconds,
default 30, clamped to 1–600. In particular, `timeout: 0` does **not** disable
the watchdog as it does in OMP. Fractional seconds are not accepted. These
aliases do not add backend sniffing or multiple cells. Agent-owned eval now
supports [policy-checked tool orchestration](eval-structured-results.md#agent-scoped-tool-orchestration)
through `tool.call` and `tool.result`, plus the explicit typed-judgment bridge.
OMP's retained subagent/workpool API remains separate and is not implemented.

## JavaScript images

JavaScript `display(value, mimeType)` accepts `image/png`, `image/jpeg`,
`image/gif`, and `image/webp`. Pass a `Uint8Array` (including a subarray view)
or canonical standard base64. For example, with image bytes already obtained
inside the cell:

```javascript
display(jpegBytes, "image/jpeg");
display(webpBase64, "image/webp");
```

The model receives a native image content block, not a printed base64 string.
The SDK display descriptor includes width, height, MIME type and encoded frame
bytes, without another copy of the image. Inputs are explicit data: this helper
does not read paths, fetch URLs, execute SVG/HTML, or invoke custom serializers.
Python's public display helper remains PNG-only; this change extends the native
JavaScript helper and the shared host MIME validator.

Host checks preserve the existing limits: 32 displays per cell, 2 MiB per
serialized frame, 4 MiB across frames, 16,384 pixels per axis, and 16,777,216
pixels per canvas. JPEG baseline/progressive frame headers, GIF87a/GIF89a
logical screens, and WebP VP8/VP8L/VP8X headers are inspected without raster
decoding. MIME/header mismatches, invalid base64, missing envelope markers,
truncated segment/chunk boundaries and oversized canvases fail explicitly.
A bad image rejects the final rich-output collection rather than publishing a
partial successful result. Already-delivered live frames cannot be recalled.
These are envelope checks, not certification of image validity or bounds on
animation frame counts or downstream decoder memory.

## Regression coverage

`src/eval/lifecycle.rs` covers OMP aliases, ambiguous/malformed requests,
namespace reuse, explicit reset and watchdog behavior. Host image tests live
in `src/eval/display/images.rs`. They require the repository's Rust toolchain
and dependency workspace.

The embedded JavaScript encoder also has a dependency-free Node harness:

```sh
node --test tests/eval_image_display.test.cjs
```

This executes the actual factory, including all four MIME types, byte views,
base64 remainders, frame limits, unsupported types, snapshot semantics and
captured-intrinsic tampering. It is not a substitute for native QuickJS and
Rust integration tests.
