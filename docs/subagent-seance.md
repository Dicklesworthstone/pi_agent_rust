# Saved-session consultation with seance

The native `subagent` tool can consult a saved Pi session without resuming it or writing to its transcript. Use the fixed `seance` agent to ask why an earlier decision was made, recover the reasoning behind an implementation, or compare recorded evidence with a current question.

## Usage

Enable the native `subagent` tool in the parent, then call:

```json
{
  "agent": "seance",
  "sourceSession": "/absolute/path/to/saved-session.jsonl",
  "task": "Why did this session choose the queue design? Cite the relevant entry IDs."
}
```

`sourceSession` is an explicit version-3 Pi JSONL path. Relative paths resolve from the parent tool's workspace. It is not a session-ID search or a resume request.

Source admission uses the same policy as the native `read` tool: the current workspace, its live additional roots, and the configured agent data directory are allowed. Other locations require an explicitly added root, for example with `--add-dir`; a custom session storage directory does not create an implicit exception. SDK hosts can share their live roots through `SubagentTool::with_workspace`.

Admission is checked before recovery, again after recovery, and after privacy projection. The native runner also rechecks it after host progress callbacks immediately before each process spawn, including schema retries and explicit revival. Removing an additional root during a host privacy or progress callback therefore refuses child startup. The recovery API still receives the original path and rejects a final symlink, even when its target would otherwise be admitted.

By default, the consultant launches with the selected branch's saved provider, model and thinking level. The last model/effort changes on that branch take precedence. When sibling branches contain changes, only explicit fallback fields can supply a missing branch selection; another branch's tip header is not used. If no saved model can be determined, supply an explicit override:

```json
{
  "agent": "seance",
  "sourceSession": "sessions/design.jsonl",
  "model": "anthropic/claude-sonnet-4-5",
  "task": "Which alternatives were rejected, and what evidence supported the decision?",
  "timeoutSeconds": 120
}
```

An override uses ordinary native model-selector syntax. The parent model and task/smol role do not silently replace the saved selection. Model availability and credentials are resolved by the native child. Its configured provider retry/failover policy still applies after launch; this does not promise that a provider error can never trigger a configured fallback.

Optional `outputSchema` and `schemaMode` use the existing native typed-output validator and one corrective retry. The original request timeout covers recovery, privacy projection, launch, execution and retry. Synchronous filesystem work and a host-supplied privacy callback cannot be interrupted in the middle of a call, but expiration is checked before a child can start.

## Historical context and privacy

The snapshot contains the selected root-to-leaf branch's retained text records, including compaction and rewind records. Original entry IDs and source line numbers remain available for citations. It also includes the saved workspace label, selected leaf and source digest. These records are historical evidence, not reconstructed live provider state.

Recovery validates the entire source graph before selecting the branch. Duplicate IDs, missing parents, cycles, invalid selected leaves, unsupported formats and observed source changes fail the request. The existing text-recovery API reads the source twice and compares identity and digest. This detects observed changes; it is not an atomic snapshot against a hostile concurrent writer.

Image and other media blocks become explicit omissions. Their data and sidecars are never opened or decoded. The consultant must say when omitted attachments or missing evidence prevent an answer.

The complete question, snapshot, schema and selected identifiers pass through auxiliary privacy before launch. Live Agent calls use the invoking session's current vault, including secrets learned earlier in that conversation. Direct SDK calls use the configured `SubagentTool::with_secrets_settings` policy. The auxiliary privacy floor remains enabled when primary secret handling is off; block mode refuses matching content before spawning. Redactions sent to the child are not reversible vault IDs.

## Isolation and ownership

The consultant is an internal definition. A user or project agent file named `seance` cannot override it.

Its native child has no tools and uses a fixed full system prompt. Launch disables MCP, extensions, skills, context files, prompt templates, themes and migrations. The child receives the protected text snapshot, with no source session to reopen. It can explain history but cannot inspect current files or follow historical tool instructions.

The `--no-mcp` CLI switch is a separate process-wide control. It skips MCP discovery, registration, server startup, credential resolution and tool mounting across classic, print, RPC and SDK-backed interactive paths. ACP hosts propagate it and reject nonempty client-supplied `mcpServers` lists when creating or loading a session. An omitted or empty list remains valid. `--no-tools` retains its existing meaning of disabling built-in tools; callers requiring MCP isolation should also use `--no-mcp`.

Execution remains owned by the existing native child runner: parent cancellation, deadlines, process-tree cleanup, terminal protocol validation, bounded output, schema acceptance and hub tracking are unchanged. The hub privately retains the protected launch snapshot for explicit native revival; revival rechecks current source-path admission without rereading or mutating the original session. These are runtime capability restrictions for the trusted Pi executable, not an operating-system sandbox against a deliberately substituted child binary.

## Results and current limits

The ordinary `pi.subagent.result.v2` envelope uses `mode: "seance"` and adds `consultation` metadata:

| Field | Meaning |
| --- | --- |
| `hubId` | Native hub run ID for explicit revival, or `null` if no run was registered |
| `sourceSha256` | Digest of the validated source bytes |
| `history` | `active_path_text` |
| `records` | Number of selected records sent |
| `omittedMedia` | Number of media omissions on the selected branch |
| `contextBytes` | Size of the protected historical JSON |
| `modelSource` | `saved_active_branch` or `explicit_override` |

The public task in results, progress updates and hub labels remains the protected consultation question. The full historical snapshot is not echoed into the parent's tool result. Child failures and schema failures retain normal native error/result behavior.

This first surface accepts one source and one question per call. It rejects `cwd`, `scope`, workspace isolation/writeback options, tasks/chain batches and workflow calls. It does not add the OMP `/seance` picker, session-ID discovery, nested-session traversal, SQLite recovery, media inspection or an interactive multi-turn consultation interface.

The question limit is 8 KiB and the schema limit is 8 KiB. Complete historical JSON is bounded at 192 KiB before privacy scanning. Because the existing child runner carries prompts in argv, the final protected question, history, schema and fixed prompt must fit within 96 KiB on non-Windows platforms or 12 KiB on Windows. Escaping and framing count toward that limit. Exceeding any limit returns `PI_SEANCE_CONTEXT_LIMIT`; no truncated prefix is sent.

Focused regression tests cover branch model/effort selection, sibling fallback rules, empty-root selection, media omission, request control rejection, complete-context limits, a real native child with a seeded live privacy vault and typed output, explicit model override, block policy, unsafe source paths, live root additions/removals, root revocation during privacy and progress callbacks, native agent-directory history admission, progress and hub display containing only the question, and native revival with current admission checks. They are designed for the repository's DSR validation path.
