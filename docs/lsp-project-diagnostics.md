# Native project diagnostics

Use the `lsp` tool's `project_diagnostics` action to ask one language server for
its whole-workspace diagnostic report, including unopened and virtual documents.
The anchor file chooses the server and its workspace root; it is not opened as an
LSP document by this action.

```json
{
  "action": "project_diagnostics",
  "file": "src/main.rs",
  "timeout": 60,
  "limit": 100
}
```

This is input to the agent's `lsp` tool, not a new shell subcommand. The file must
exist and have a configured language server. A multi-language repository may
need separate calls with anchors for its different servers.

## Choosing a diagnostic workflow

`project_diagnostics` uses the server's native `workspace/diagnostic` request.
The server must advertise `diagnosticProvider.workspaceDiagnostics: true`.
Unsupported servers return `LSP_UNSUPPORTED`; request and protocol failures are
not replaced with cached diagnostics or an empty success.

`workspace_diagnostics` remains an explicit, bounded walk of a workspace-relative
glob. It synchronizes and checks each matching file and reports per-file failures.
Use it when the server does not support native workspace diagnostics or when a
particular filesystem subset matters. `diagnostics` with one file checks that
file; `diagnostics` with a glob only inspects the existing cache.

## Reading the result

`responseComplete: true` means a complete native response was received and
validated. `complete: true` additionally means every document report in that
response is present in the displayed result. Neither field proves that every
file in the repository was analyzed: the server determines the report's scope,
and this operation is not an atomic filesystem snapshot.

`totalDocuments`, `totalDiagnostics`, `errorCount`, and `warningCount` describe
the entire validated response, including omitted reports. `returnedDocuments`
is the number displayed. When `truncated` is true, the result contains only
whole document reports; it never cuts JSON or silently turns omitted errors into
zero errors. Use an individual-file diagnostic request or the active glob scan
to inspect omitted file reports. Returned URIs are metadata, never paths that Pi
opens or downloads automatically.

The display defaults to 100 documents, capped at 1000, within a 200 KiB JSON
budget. Native responses are separately limited to 8 MiB, 2048 document reports
and 16,384 diagnostics. Exceeding a native limit is an explicit error, not partial
success. Requests send no previous result IDs or partial-result token, and reject
an `unchanged` report without a baseline.

`timeout` covers server startup and response processing after the tool's workflow
lane is acquired, capped at 120 seconds; zero uses the configured request default.
Cancellation prevents new admission or cancels an already posted request. The
client does not reuse a workspace result if its synchronized documents change
while waiting. Unversioned external disk changes still cannot be ruled out.

## Rust API

Embedders with an initialized `pi::lsp::client::LspClient` can call
`client.workspace_diagnostics(timeout).await`. It returns URI-sorted, validated
full document reports without modifying the push or per-document diagnostic
caches. The same cancellation, deadline, capability and response limits apply.
