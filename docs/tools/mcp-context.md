# MCP resources and prompt references

A trusted MCP server gets a server-bound context tool in addition to its ordinary
`mcp__...` tool wrappers. The context tool name starts with `mcp_context_`, includes
a shortened server label and a stable hash, and is available through the same
startup, runtime trust, and extension-registration paths as other MCP tools.
Pending, denied, or configuration-disabled servers do not expose context tools. Existing wrappers recheck
trust on every call, including after a server has returned its response.

These actions run against the server attached to the
tool; there is no argument for selecting another server or an arbitrary RPC method.

```json
{"action":"list_resources"}
{"action":"list_resource_templates"}
{"action":"read_resource","uri":"repo://src/main.rs"}
{"action":"read_resource_template","uri_template":"repo://{path}","variables":{"path":"main.rs"}}
{"action":"list_prompts"}
{"action":"get_prompt","name":"code_review","arguments":{"code":"fn main() {}"}}
{"action":"complete_argument","reference":{"type":"ref/prompt","name":"code_review"},"argument":{"name":"code","value":""}}
{"action":"subscribe_resource","uri":"repo://src/main.rs"}
{"action":"poll_updates"}
{"action":"unsubscribe_resource","uri":"repo://src/main.rs"}
```

## Resource discovery and reads

Each listing returns one page, not an automatically accumulated catalog. When the
result contains `nextCursor`, pass that exact string as `cursor` in another call
of the same listing action on the same server. An empty `nextCursor` is still a
cursor; only absence indicates the end. Resource-template discovery returns the
server's `uriTemplate` unchanged. Supply a concrete URI to `read_resource`, or use
`read_resource_template` with that exact template and its variables.

Resource URIs are opaque server identifiers. Even a `file://` or `https://` URI is
sent to the trusted server through `resources/read`; Pi does not open the named
local file or fetch the URL. The server may return multiple content blocks.
Text stays text; supported image, audio, and video blobs use Pi's native content
blocks. Unknown binary formats produce an explicit placeholder with the original
payload in tool details, not a base64 dump into model text. Malformed native media
is reported as a content error.

## Watching resources and catalog changes

When the server advertises `resources.subscribe`, `subscribe_resource` watches
one exact resource URI and returns `{"uri":"…","subscribed":true}` after the
server acknowledges it. Repeating an already acknowledged subscription is a
local success. `unsubscribe_resource` returns `subscribed:false`; unsubscribing
an unwatched URI is also a local success. A second mutation for the same URI
while its first operation is pending returns `MCP_SUBSCRIPTION_BUSY`.

`poll_updates` consumes the connection's pending change notices without making a
server request. It reports active subscriptions, changed resource URIs, and
resource/prompt catalog invalidations:

```json
{
  "connected": true,
  "capabilities": {
    "subscribe": true,
    "resources_list_changed": true,
    "prompts_list_changed": true
  },
  "subscriptions": ["repo://src/main.rs"],
  "updated_resources": ["repo://src/main.rs"],
  "resources_list_changed": false,
  "prompts_list_changed": true,
  "expired_subscriptions": [],
  "expired_overflow": false
}
```

Repeated notifications for a URI coalesce into one entry between polls. Only
requested URIs are accepted, with their exact bytes preserved; notification
metadata and unsolicited resource identifiers never enter model context.
Changes arriving before the subscribe acknowledgement are retained, then become
visible after that acknowledgement succeeds. An explicit server rejection rolls
back the local mutation; a rejected unsubscribe leaves the existing watch active.

Resource and prompt list-change flags are admitted independently according to
the server's `resources.listChanged` and `prompts.listChanged` capabilities.
They indicate that another explicit listing may be useful. Notifications never
automatically read a resource, refresh a catalog, retrieve a prompt, or start an
agent turn. Use `read_resource`, `list_resources`, or `list_prompts` when fresh
content is needed. Native stdio, Streamable HTTP POST/GET SSE, and legacy SSE
receive paths all feed the same bounded inbox.

Watches belong to one connection and protocol session. A disconnect or HTTP
session replacement expires them; Pi never silently resubscribes or replays an
uncertain subscription mutation. Polling reports expired URIs once and returns
`connected:false` while disconnected. It does not reconnect. A later explicit
context operation may reconnect, after which desired watches must be requested
again. Old readers and late acknowledgements cannot affect the new session.
Trust revocation and session shutdown discard all retained watch state.

Each server accepts at most 64 active or pending subscriptions, with at most
64 KiB of combined URI bytes and 16 KiB per URI. Expiry notices have the same
count and total-byte bounds; `expired_overflow:true` means some expired names
could not be retained across repeated unpolled reconnects. No subscription is
restored automatically, including when this flag is set. Polling drains change
and expiry notices but leaves acknowledged subscriptions active.

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

## Forms during retrieval

A resource read or prompt retrieval can ask for additional non-sensitive values
through the session's installed ask picker. Pi identifies the requesting server,
validates each field, and shows the complete values for explicit review before
sharing them. The picker works even when the model-facing `ask` tool is disabled.
Headless sessions cancel the form; automatic recommended answers are not sent.
See [interactive server forms](mcp-configuration.md#interactive-server-forms)
for supported fields and the submission, edit, decline, and cancel choices.

Listings, template discovery, argument completion, and subscription mutations
cannot open a server form.
If concurrent requests make a form's initiating request ambiguous, Pi cancels it.
The original request's timeout includes the time spent answering. Cancellation,
timeout, trust revocation, or a changed protocol session removes the pending form
and prevents late answers from publishing context. A request and its form reply
remain in their original session and are never replayed after HTTP session expiry.

## Limits and lifecycle

Resource, template, and prompt listings accept up to 1,024 entries and 2 MiB of
serialized response data per page. Resource reads and prompt retrieval accept up
to 10 MiB of serialized response data. Prompt retrieval accepts at most 256 messages,
leaving room for role labels within the native content shaper's block limit.
Existing transport and media-specific bounds still apply and may be smaller.

Cursors are limited to 4,096 bytes; resource URIs to 16,384 bytes; prompt and
argument names to 1,024 bytes. A prompt accepts at most 128 string arguments and
64 KiB of serialized argument data, including JSON escaping. Each dispatched
request uses the server's configured MCP timeout, defaulting to 30 seconds.
See [MCP configuration](mcp-configuration.md) for imported settings, bounded
timeout values, and the earlier startup/caller deadlines that still apply.
Initial connection and trust validation retain the manager's existing behavior.

Cancellation or a transport failure does not replay a request. Late results from
a revoked server, a replaced transport, or a shut-down session are rejected.
A resource-only or prompt-only server may explicitly reject the initial
`tools/list` with JSON-RPC MethodNotFound without being marked as crashed. Other
errors, including failures on later tool-catalog pages, are not swallowed.
Unsupported resource or prompt methods return their server error explicitly.

Prompt execution and automatic change-notification refresh are not performed.

## Rust API

`McpManager` exposes `list_resources`, `list_resource_templates`, `read_resource`,
`read_resource_template`, `list_prompts`, `get_prompt`, `complete_argument`,
`subscribe_resource`, `unsubscribe_resource`, and `poll_updates`.
Listings take an optional borrowed cursor;
`get_prompt` takes an optional borrowed `BTreeMap<String, String>`. Methods return
validated raw JSON for server reads and locally generated JSON for subscription
state and update polling. Native content conversion happens in the mounted context tool.

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
