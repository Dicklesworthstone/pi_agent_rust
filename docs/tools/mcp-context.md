# MCP resources and prompt references

A trusted MCP server gets a server-bound context tool in addition to its ordinary
`mcp__...` tool wrappers. The context tool name starts with `mcp_context_`, includes
a shortened server label and a stable hash, and is available through the same
startup, runtime trust, and extension-registration paths as other MCP tools.
Pending or denied servers do not expose context tools. Existing wrappers recheck
trust on every call, including after a server has returned its response.

These are the six supported actions. They run against the server attached to the
tool; there is no argument for selecting another server or an arbitrary RPC method.

```json
{"action":"list_resources"}
{"action":"list_resource_templates"}
{"action":"read_resource","uri":"repo://src/main.rs"}
{"action":"list_prompts"}
{"action":"get_prompt","name":"code_review","arguments":{"code":"fn main() {}"}}
{"action":"complete_argument","reference":{"type":"ref/prompt","name":"code_review"},"argument":{"name":"code","value":""}}
```

## Resource discovery and reads

Each listing returns one page, not an automatically accumulated catalog. When the
result contains `nextCursor`, pass that exact string as `cursor` in another call
of the same listing action on the same server. An empty `nextCursor` is still a
cursor; only absence indicates the end. Resource-template discovery returns the
server's `uriTemplate` unchanged. Supply a concrete URI to `read_resource`; this
implementation does not perform URI-template expansion.

Resource URIs are opaque server identifiers. Even a `file://` or `https://` URI is
sent to the trusted server through `resources/read`; Pi does not open the named
local file or fetch the URL. The server may return multiple content blocks.
Text stays text; supported image, audio, and video blobs use Pi's native content
blocks. Unknown binary formats produce an explicit placeholder with the original
payload in tool details, not a base64 dump into model text. Malformed native media
is reported as a content error.

## Selecting a prompt

Ask the agent to inspect a named server's prompt catalog and retrieve the prompt
you selected. A prompt definition includes its declared argument names and any
`required` flags. Prompt argument values must be strings; Pi forwards them without
trimming or interpolation. Omitting `arguments` preserves server defaults, while
an explicit empty object remains an empty object on the wire. The server is the
final authority on required arguments and valid prompt names.

A fetched prompt is **reference material inside a tool result**, not a new user,
assistant, or system turn. Message order and the `user`/`assistant` role labels are
preserved, including intervening native media. No instructions are automatically
executed. Other role values are rejected. Client-only `_meta` and unknown catalog
metadata stay in tool details and do not become model-visible instructions.

## Limits and lifecycle

Resource, template, and prompt listings accept up to 1,024 entries and 2 MiB of
serialized response data per page. Resource reads and prompt retrieval accept up
to 10 MiB of serialized response data. Prompt retrieval accepts at most 256 messages,
leaving room for role labels within the native content shaper's block limit.
Existing transport and media-specific bounds still apply and may be smaller.

Cursors are limited to 4,096 bytes; resource URIs to 16,384 bytes; prompt and
argument names to 1,024 bytes. A prompt accepts at most 128 string arguments and
64 KiB of serialized argument data, including JSON escaping. Each dispatched
request uses the existing 30-second MCP request timeout. Initial connection and
trust validation retain the manager's existing behavior.

Cancellation or a transport failure does not replay a request. Late results from
a revoked server, a replaced transport, or a shut-down session are rejected.
A resource-only or prompt-only server may explicitly reject the initial
`tools/list` with JSON-RPC MethodNotFound without being marked as crashed. Other
errors, including failures on later tool-catalog pages, are not swallowed.
Unsupported resource or prompt methods return their server error explicitly.

This surface is on-demand; it does not add resource subscriptions, prompt
execution, or automatic change-notification refresh.

## Rust API

`McpManager` exposes `list_resources`, `list_resource_templates`, `read_resource`,
`list_prompts`, `get_prompt`, and `complete_argument`. Listings take an optional borrowed cursor;
`get_prompt` takes an optional borrowed `BTreeMap<String, String>`. Methods return
validated raw JSON. Native content conversion happens in the mounted context tool.

## Server-guided argument completion

The same server-bound tool supports `complete_argument`. For example:

```json
{"action":"complete_argument","reference":{"type":"ref/prompt","name":"code_review"},"argument":{"name":"framework","value":"fl"},"context":{"language":"python"}}
```

For a resource template, use its exact advertised URI template, not an expanded
URI, as the completion reference:

```json
{"action":"complete_argument","reference":{"type":"ref/resource","uri":"repo://{branch}/{path}"},"argument":{"name":"path","value":"src/"},"context":{"branch":"main"}}
```

`context` maps previously resolved argument names to string values. Pi sends it
as `context.arguments` in one `completion/complete` request. Prefixes may be
empty. Prefixes, argument values, and the server's relevance order are preserved
exactly. This does not fetch the resource, retrieve a prompt, apply a suggestion,
or poll for more suggestions when `hasMore` is true.

Only `completion.values`, `total`, and `hasMore` enter model-visible output;
client-only metadata remains in tool details. Input size and argument count,
response bytes, and the protocol's 100-suggestion limit are enforced. Unsupported
completion returns the server error without disabling its other features.
Prefixes accept up to 64 KiB; the complete serialized completion request accepts
up to 128 KiB, including JSON escaping and previously resolved arguments.
Trust revocation, cancellation, and connection-generation checks are the same
as for resource reads.

SDK callers can use `McpManager::complete_argument` with the typed
`pi::mcp::McpCompletionReference::{Prompt, Resource}`. There is no automatic
keystroke-triggered requester; interactive embedders should debounce their calls.

Wire contract: MCP specification 2025-06-18, Server Utilities / Completion.
