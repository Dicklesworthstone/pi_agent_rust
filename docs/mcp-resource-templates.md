# Reading MCP resource templates

Each mounted `mcp_context_<server>_<hash>` tool exposes
`read_resource_template`. It connects resource-template discovery and argument
completion to the same trusted-server read path used by `read_resource`.

For a server-advertised template `docs://projects/{project}/files/{path}{?revision}`:

```json
{
  "action": "read_resource_template",
  "uri_template": "docs://projects/{project}/files/{path}{?revision}",
  "variables": {
    "project": "a project",
    "path": "src/main.rs",
    "revision": null
  }
}
```

The request sent to that MCP server is `resources/read`, with URI
`docs://projects/a%20project/files/src%2Fmain.rs`. No URI is opened as a local
file, fetched with an HTTP client, normalized, or resolved against the working
directory. The template does not grant trust or certify prior catalog membership.
The normal server trust, caller capability, cancellation, response-size,
transport-generation, and no-replay checks still apply. Returned resources use
the existing native text/image/media shaping and metadata handling.

## Expansion contract

Variable names and values are exact strings. Names are case-sensitive;
percent-encoded names remain encoded and are not aliases for decoded names.
Supply values unencoded for component expressions such as `{path}` or `{?query}`.
The reserved operator `{+path}` deliberately preserves `/` and other reserved
characters, along with existing valid percent triplets. Choose the exact
operator from the server's template rather than changing its meaning.

The implementation follows RFC 6570 string expansion for all operators:
plain `{x}`, reserved `{+x}`, fragment `{#x}`, label `{.x}`, path `{/x}`,
path parameter `{;x}`, query `{?x}`, and query continuation `{&x}`. It also
supports scalar prefixes (`{x:3}`) and the scalar no-op explode modifier (`{x*}`).
Unicode literals and values are UTF-8 percent-encoded; prefix lengths do not
split Unicode characters or percent-encoded characters. There is no Unicode
normalization or automatic value coercion.

An absent referenced variable is an error, not an implicit omission that could
select a different resource. Use JSON `null` to deliberately omit an optional
variable; an empty string is a defined value and is expanded as such. Arrays,
objects, numbers, and booleans are not accepted as variable values. Composite
Level 4 expansion is outside this string-valued API.

Input admission limits are 16 KiB for the template, 128 supplied variables,
64 KiB of combined UTF-8 variable-name/value bytes, and 1,024 variable
occurrences across expressions. The expanded absolute URI is limited to 16 KiB,
checked during encoding rather than after building an unbounded string. Invalid
syntax, missing variables, unsupported value shapes, and exceeded limits return
`MCP_TEMPLATE_INVALID` before connection setup or `resources/read`. Errors do
not echo templates, names, or variable values.

SDK callers can use `pi::mcp::expand_resource_uri(template, &variables)` for
pure expansion or `McpManager::read_resource_template(server, template,
&variables).await` for the same server-bound operation.

Specification: RFC 6570, especially sections 2.1–2.4 and 3.1–3.2.
