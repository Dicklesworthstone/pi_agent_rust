# Read implementation code by semantic definition

The existing `lsp` tool's document-symbol lookup also reads source when a
`symbol` or `line` selector is supplied. It uses the same checked, synchronized
source as the outline, not another file read after resolving the definition.
No additional tool, model call, or language-server request is required.

```json
{"action":"symbols","file":"src/service.rs","symbol":"handle#2"}
```

```json
{"action":"symbols","file":"src/service.rs","symbol":"handle","line":120}
```

Name and line selection keep the existing semantics: case-sensitive definition
name substring, optional one-based `#N` in server preorder after name/line
intersection, and a one-based source line intersecting the full definition.
Comments and call sites do not contribute to the numbered definition matches.
`query` still means workspace-symbol search and is not changed by this feature.

An unfiltered `symbols` request remains a metadata-only outline. A selected
request sets `sourceIncluded: true` and includes `source` on every displayed row:

* `text` is an exact prefix of the server-reported full symbol range, at most
  16 KiB of UTF-8. It is not just the identifier's `selectionRange`.
* `range` gives the returned excerpt's zero-based UTF-16 start and end;
  `byteRange` gives its absolute half-open UTF-8 offsets in the source file.
* `totalBytes`, `omittedBytes`, and `truncated` distinguish a complete extent
  from clipped source. The row's original `range` remains the full extent.
* `extent` is `document_symbol_range` for a hierarchical document symbol, or
  `symbol_location_range` for a flat symbol. A flat location may describe only
  an identifier. Pi does not infer braces, a body, or a hierarchy for it.

The response's `sourceSha256` hashes the complete checked source text, including
its original line endings. `sourceTruncated` means that at least one displayed
code excerpt was clipped. This is independent of `truncated`, which continues
to mean that some selected symbol rows were omitted. A complete server response
can therefore have `responseComplete: true` and `sourceTruncated: true`.

Excerpts retain CR, LF, CRLF, tabs, Unicode, and literal text exactly. Clipping
never splits a UTF-8 scalar or the interior of CRLF. The structured JSON output
escapes control characters normally; it does not normalize or secret-redact
source. Consumers must treat returned code as data, not terminal instructions.

The 16 KiB per-excerpt cap does not replace the existing 200 KiB structured
response budget. Entire rows, ancestor metadata, and JSON escaping all spend
that budget. Large selections may return fewer complete rows; use a more
specific name, line, or numbered match. Existing 2 MiB source/response, 8192
symbol, and 32-level hierarchy limits remain unchanged. Endpoint offsets are
retained from the one-pass validation rather than rescanning the whole file for
each definition.

All server members, including hidden and filtered-out symbols, are validated
before any source excerpt is returned. Source changes or a retired server fail
the operation rather than returning stale code. Cancellation and the existing
startup-to-result deadline also cover mapping, extraction, and hashing. A
fingerprint identifies this checked text; it is not an edit approval, a lock on
the file, or an atomic project snapshot. Returned URIs are never opened, and no
command or workspace-edit permission is granted.

Regression coverage is in `tests/lsp_symbol_source.rs` and the outline's
`source_text` unit tests. The authoritative quality entry point remains
`dsr quality --tool pi_agent_rust`.
