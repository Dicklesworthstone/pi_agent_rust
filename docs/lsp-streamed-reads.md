# Streamed symbol and navigation reads

The `lsp` tool accepts language-server partial results for workspace `symbols`
(when `query` is provided), `references`, `definition`, `type_definition`, and
`implementation`. Existing arguments and output envelopes are unchanged. Hover
requests and lazy `workspaceSymbol/resolve` requests do not negotiate array
fragments.

Each search attempt includes its own `partialResultToken`. A server can ignore
it and return its ordinary complete response, or send arrays in matching
`$/progress` notifications. Pi preserves fragment order and opaque symbol data,
then validates the complete assembled result before rendering or resolving any
retained symbol. A nonstreamed definition may still return a single Location.

## Completion and limits

Progress does not authorize a result. After sending any fragments, the server
must finish successfully with `null` or an empty array. A nonempty final result
is rejected rather than guessed to replace or append to the fragments. Every
fragment must be an array, including empty fragments. A failed request never
returns cached or partial search results.

The existing 2 MiB semantic response allowance is cumulative across encoded
fragment values and the terminal result. At most 2,048 fragments are retained,
and the assembled result is limited to 4,096 symbols or navigation locations.
The operation-specific item limits and validators remain in force. Array
assembly failures use `LSP_PARTIAL_RESULT`; existing semantic validation errors
retain their named errors. Exceeding a limit is an error, not a clean result.

Display limits apply only after full validation. Malformed omitted items,
reversed or mixed location forms, and source drift still fail the request.
Counts describe the validated result rather than just its first fragment.
Returned target URIs remain metadata: these reads neither open target files nor
execute server commands. Lazy symbol resolution preserves the original opaque
`data` and the existing identity fence.

## Ownership and retries

All attempts share the original deadline. Navigation retains its existing empty
index-warmup retry policy, applied to the assembled result rather than the empty
terminal. Workspace-symbol searches do not gain an empty-result retry policy.
Retries use fresh tokens and discard earlier fragments. Late progress for an
older attempt and unrelated work-done notifications cannot enter the new result.

Cancellation, timeout, abandoned futures, notification loss, and byte/fragment
limits use the shared request-owned partial-result collector. These failures
retire the collection and release its serialized request lane. A successful
result is still one server's source-bound report, not a guarantee that every
project file was inspected or an atomic snapshot of the project.
