# Reading ZIP archives

The built-in `read` tool can inspect ZIP files and ZIP-based `.jar`, `.war`,
`.ear`, `.apk`, and `.whl` packages without extracting them or invoking an
external archiver. Use a single colon between the archive path and its member:

```json
{"path":"release.zip"}
{"path":"release.zip:config"}
{"path":"release.zip:config/production.yml","offset":1,"limit":80}
```

A bare archive path lists its immediate children in sorted order. Directories
also appear when the ZIP contains only their nested files. Directory listings
use the same `offset`/`limit` paging as member text, so later pages can discover
every child. File reads return UTF-8 text through the usual line-window output.
Binary members produce an explicit error.

Set `raw: true` on a bare archive path to bypass the listing and inspect the
outer file through the ordinary raw-file reader. An explicit member path
still navigates the archive when `raw: true` is set. Archive listings and
members are read-only views: `hashline: true` is rejected because their line
numbers cannot be used to edit the outer ZIP file.

`offset` and `limit` select rendered lines. Each response includes `member`,
`isDirectory`, `entryCount`, `totalLines`, `outputLines`, and `truncated` in
its details. When another output page exists, `nextOffset` gives its starting
line. The response is bounded to 2,000 lines and 1,000,000 bytes of body text,
plus a short continuation notice.

The outer archive must be readable within the session's allowed workspace
roots. All archive reads use the same already-opened file descriptor; they do
not reopen the path after admission. Archive entries are never extracted,
written, executed, or followed as filesystem links.

Supported members use ZIP Stored or DEFLATE compression. Encrypted archives,
symbolic links and special files, unsafe member paths, duplicate normalized
names, contradictory local headers, and overlapping entries are rejected.
Input is limited to 100 MiB, 4,096 entries, and 64 MiB of declared expanded
content across the archive. An individual text member is limited to 16 MiB,
with a separate cap applied while decompressing and a CRC check before text
is returned. Metadata reads have byte and operation budgets as well.

This implements the single-colon ZIP navigation workflow described by
[OMP's current files guide](https://omp.sh/docs/files). Its broader archive
formats and archive mutation remain separate work.
