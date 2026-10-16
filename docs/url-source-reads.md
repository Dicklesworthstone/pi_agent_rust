# Source-aware URL reads

The ordinary `read` tool now fetches a GitHub file page's actual raw document.
For example, a `https://github.com/OWNER/REPO/blob/REF/PATH` input in reader mode
resolves to the raw host before the HTTP request. The result contains file bytes,
not a successful placeholder telling the model to make a second call. Existing
read offset/limit handling continues to page the returned document.

Encoded ref and path segments are preserved. UI query parameters and line
fragments are not forwarded to the raw host. Only the exact public GitHub host
on its default port is recognized. Private repositories do not gain credentials:
no token, cookie, authenticated browser state or local Git credential is loaded.
An inaccessible raw document fails normally instead of inventing file content.
The resolver does not disambiguate unencoded slashes in branch names; use a commit
SHA or an appropriately encoded ref for an unambiguous file URL.

Repository HTML is returned as source, including its script and navigation text;
it is not rendered or executed. Notebook and PDF sources retain their specialized
readers (PDF still requires the existing build feature). The response's final_url
is the URL actually fetched; ordinary source files report github-blob-raw as the
extractor. The separate network-free converter processes its supplied bytes and
never substitutes an instruction to fetch another page.

Raw mode intentionally bypasses the GitHub rewrite as well as content conversion.
HTTP redirects are still followed, as they were previously. All modes now resolve
Location using URL reference semantics: origin-relative paths, dot segments,
query-only changes, scheme-relative destinations and hosts without path suffixes
work consistently. Only 301/302/303/307/308 are redirects. Missing or ambiguous
Location headers, loops, unsafe destinations and more than five redirects fail.

Every actual request is admitted, including a raw-source rewrite reached through
a redirect. URLs must be bounded HTTP(S) addresses without embedded credentials,
controls or backslashes. Canonical parsing recognizes abbreviated/numeric IPv4 and
mapped IPv6 forms; the private-target policy is checked before each request. The
explicit private-target override still applies. This is a syntactic host policy,
NOT DNS pinning or complete protection against DNS rebinding. Hostname resolution
retains the shared HTTP client's existing guarantees.

One effective configured HTTP timeout covers the complete redirect/source fetch
and body, rather than restarting at each hop. Explicitly disabled HTTP timeouts
remain disabled. Cancellation and the existing 10 MiB download bound apply; ready
body streams yield periodically to the deadline. Synchronous conversion and OS
operations remain cooperative rather than preemptible. Download truncation stays
explicit. Errors do not reflect complete URLs with query credentials; successful
final URLs and document text can still contain sensitive application data.

Fourteen new Rust regression tests cover source mapping, source-vs-rendered HTML,
raw behavior, redirects, policy, loops and limits. Four use real loopback HTTP
peers and the production client. They are authored, not executed: the required
`dsr quality --tool pi_agent_rust` could not start because DSR is absent (exit 127).
Rust compilation, formatting, Clippy, full ReadTool and live GitHub acceptance
remain unverified. No dependencies, Actions or quality gates are changed.

## Saved notebook results

Notebook reads now include saved stdout/stderr, execution counts, exception
names/messages/traceback frames, and one textual representation of each saved
display or result. This is a view of existing evidence, not execution, validation
of the original computation, or a new kernel session. It is wired into the same
converter used after GitHub raw retrieval and ordinary URL fetches.

The reader supports nbformat 4. Source/text fields accept strings or string arrays;
arrays concatenate without inserting separators. Traceback frames remain separate.
JSON MIME, whitespace and UTF-8 BOMs no longer hide extensionless notebooks. A
mere occurrence of the word nbformat in ordinary JSON is not enough to classify
it as a notebook. Declared unsupported versions and malformed interpreted fields
fail explicitly. This is not a complete Jupyter schema validator: optional and
extension metadata is not interpreted as an instruction.

MIME selection prefers application/json, then text/plain, text/markdown and
text/html. JSON is streamed as data into the bounded output rather than expanded
into an unbounded pretty-print intermediate. HTML uses the existing static reader,
not a browser. Other saved representations are listed but not duplicated. Images,
widget state and attachments are explicitly noted as unrendered; base64 payloads
are not dumped into model context and attachment URLs are not fetched. Unknown
cell/output types are labeled rather than silently reported as empty results.

Code/raw/output fences cannot be closed by ordinary embedded fence text. Control
characters other than newline, carriage return and tab are rendered visibly,
including ANSI escapes in tracebacks. Notebook text and Markdown remain untrusted
source content and may contain secrets or misleading instructions; this is not a
prompt-injection sandbox or general secret scrubber.

Limits are 10 MiB input, 4,096 cells, 8,192 total outputs, 32 MIME entries per
bundle and 16 MiB rendered text. Exceeding a bound fails without publishing a
silently truncated notebook. A fetch already marked download_truncated is refused
by the notebook converter, even if the retained prefix happens to parse. Normal
read line pagination applies only after this complete bounded conversion.

Thirteen additional Rust tests cover saved results, MIME selection, unsupported
content, errors, literal text, budgets and public converter integration. The
saved fixture was checked with Python nbformat without executing its cells;
that fixture check is not execution of the Rust renderer. These Rust tests remain
uncompiled and unexecuted because DSR is unavailable. The repository's single
quality entrypoint remains `dsr quality --tool pi_agent_rust`.
