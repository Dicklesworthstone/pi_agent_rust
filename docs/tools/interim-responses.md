# Interim Markdown replies with `out://`

The native `write` tool can show an interim assistant reply while the agent
continues working. This follows the per-write reply workflow introduced in
[omp 18.14](https://github.com/can1357/oh-my-pi/releases/tag/v18.14.0).

```json
{
  "path": "out://",
  "content": "## Progress\n\nThe parser fix is in place. I am checking the remaining inputs."
}
```

Each accepted write produces one Markdown reply. The URL suffix is ignored;
`out://progress` has the same behavior as `out://`. Replies are limited to
65,536 UTF-8 bytes and must contain non-whitespace text. An output write
does not create a file, an undo entry, or a completed task. Reading an
`out://` URL returns an explanatory write-only error.

## Enablement

The top-level interactive terminal enables replies by default. Print, RPC,
ACP and direct SDK sessions default to disabled. Configure the session with:

```json
{
  "out": {
    "enabled": true
  }
}
```

An explicit `false` disables the feature in an interactive session too.
Native subagents cannot send `out://` replies, even with an enabled setting;
they return their findings through the parent result channel.

SDK hosts can supply `SessionOptions::out_enabled` as an explicit override.
`out_default_enabled` is a separate host default, used only if both the
override and loaded configuration are unset. That distinction preserves
workspace settings when an interactive host reloads or switches sessions.

## Delivery and saved history

The tool validates the complete invocation before publishing a reply. Both
terminal interfaces render the successful result as assistant Markdown,
retain the full reply, and continue the active turn. Failed writes and
provisional tool updates remain outside the accepted reply history.

The model receives the short acknowledgment `Shown to the user.`. Tool-result
metadata retains the accepted Markdown for display and saved-session replay:

```json
{
  "schema": "pi.interim_response.v1",
  "text": "## Progress\n\nThe parser fix is in place."
}
```

SDK and JSON/RPC hosts receive this metadata through the existing tool event
and message interfaces. To display a reply once, consume a successful
`tool_execution_end` for `toolName: "write"`, then extract the text with
`pi::tools::interim_response_text`. The helper also rejects failed results,
invalid metadata and content outside the reply limits. An enabled plain
text print session writes the accepted reply to stdout without suppressing
the agent's later final answer.

Replies appear after accepted tool execution. Token-by-token rendering of
unfinished write arguments and embedded-shell `> out://` redirection are
not implemented by this native workflow.
