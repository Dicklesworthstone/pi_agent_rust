# Native agent result URLs

`read` can retrieve a native subagent's result without copying its entire
transcript into the parent context:

```json
{"path":"agent://"}
{"path":"agent://scout-1"}
{"path":"agent://scout-1/findings/0/path"}
```

The first call lists the current parent process's child IDs and statuses. Use
an ID from that roster, not just an agent definition name. The second reads
its final output. The third extracts a JSON field: slash components are literal
object keys or decimal array indices. Percent-encode `/` inside a key as `%2F`.
String values are returned as text; arrays, objects, numbers, booleans and null
are returned as JSON. Missing fields are errors, not null values.

The older OMP spelling `agent://scout-1/findings.0.path` also works. A literal
object key matching the whole component takes precedence. Percent-encoded
components and multi-component slash paths never use the dotted fallback.

## Acceptance, failure and retention

Results are published by the native execution lease only after protocol
completion, schema acceptance and any isolated-worktree disposition. Running
children return status information, not guessed JSON from their live output.
Failed, killed, cancelled and schema-invalid children cannot supply projected
values. Successful results without an output schema support projection only
when their complete final text is JSON; embedded JSON in prose is not guessed.

Ordinary `read` offset/limit pagination applies. Result text uses the same
secret-obfuscation pass as hub transcript reads. Error metadata is redacted as
well. No URL component is interpreted as a filesystem path.

Retention is process-local and bounded: at most 64 results, 1 MiB of serialized
payload per result and 8 MiB in total. Oldest results are evicted first. An
unretained terminal run remains visible by status; projecting its output
returns `PI_URL_AGENT_OUTPUT_UNAVAILABLE`. Hub transcripts are independent.
Run IDs are qualified internally by the hub's private namespace, so reusing a
display ID cannot expose a previous namespace's output.

This surface does not yet resolve another process's hub, persist result URLs
across application restarts, or send messages through `write agent://`.
