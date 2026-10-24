# Reading complete skill bundles

The `read` tool resolves a loaded skill's instructions and supporting files
without requiring the model to reconstruct installation paths:

```json
{"path":"skill://review"}
{"path":"skill://review/references/checklist.md"}
{"path":"skill://review/references/checklist.md","offset":20,"limit":40}
{"path":"skill://review/references"}
{"path":"skill://review/."}
```

Bare `skill://review` and `skill://review/` read the instruction file.
`skill://review/.` explicitly lists its base directory. Directory documents
contain sorted names, entry kinds and encoded URLs. Symlinks resolving outside
the skill base, dangling links and special files have no readable URL. The
`readable` flag describes a contained file/directory target, not an access
permission guarantee; a later read revalidates it and may fail.

`skill://` lists up to 128 loaded skill names and short descriptions. A loaded
collision namespace such as `plugin/review` wins over treating `review` as a
supporting path of a bare skill named `plugin`. Percent-encoded UTF-8 names and
spaces work; percent escapes are decoded once, and `+` remains a literal plus.

Supporting files must be valid UTF-8 and fit the existing resource-file byte
limit. They use ordinary `read` offset/limit pagination. Directory scans stop
after 4096 entries and return at most 256, with explicit `scanned` and
`truncated` fields. A truncated scan is not a complete directory inventory.

## Scope and compatibility

Supporting paths are confined to the selected skill's canonical base directory.
Parent traversal, absolute paths, Windows-style prefixes, control characters
and malformed encoding are rejected before filesystem resolution. Internal
symlinks can address other resources inside that base; external targets fail.
Unix supporting-file reads use descriptor-relative, no-follow, nonblocking
opens with identity checks. Unix platforms with Rustix directory-stream support
also enumerate a pinned directory handle. Other platforms retain canonical
containment and ordinary filesystem operations; this is not a claim of an
atomic Windows sandbox against concurrent hostile filesystem mutation.

Existing instruction-file loading and its malformed-resource diagnostics are
preserved. Unknown skills never become ordinary filesystem reads. No changes
are made to skill execution, activation policy, or the existing document,
conflict, GitHub and SSH handlers.

This is a read-only URL surface. It does not add `edit`/`write`/`grep` support for
skill URLs, broaden a skill's scope to its whole plugin package, or fall back
to MCP-advertised resources when a skill name is unknown.
