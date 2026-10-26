# Reading ZIP and TAR archives

The built-in `read` tool inspects archives without extracting members or
invoking an external archiver. Supported path suffixes are ZIP, `.jar`, `.war`,
`.ear`, `.apk`, `.whl`, `.tar`, `.tar.gz`, `.tgz`, and Rust `.crate` packages.
Suffix matching is case-insensitive. Use a single colon before a member:

```json
{"path":"release.tar.gz"}
{"path":"release.tar.gz:config"}
{"path":"release.tar.gz:config/production.yml","offset":1,"limit":80}
{"path":"library.crate:library-1.0.0/Cargo.toml"}
{"path":"release.zip:src/main.rs","offset":1,"limit":80}
```

A bare archive path lists immediate children in sorted order. Directories also
appear when only their nested files are present. Listings use the same
`offset`/`limit` paging as member text, so later pages can discover every child.
File reads return UTF-8 text through the normal line-window output; binary or
invalid UTF-8 members fail explicitly. An archive member is not an editable
filesystem path and is never executed.

Set `raw: true` on a bare archive path to bypass archive navigation and use the
ordinary raw-file reader. An explicit member still navigates the archive with
`raw: true`. `hashline: true` is rejected for archive views because their line
numbers cannot be used to edit the outer file.

`offset` and `limit` select rendered lines. Responses include `member`,
`isDirectory`, `entryCount`, `totalLines`, `outputLines`, and `truncated`.
`nextOffset` identifies a further page. Output remains bounded to 2,000 lines
and 1,000,000 body bytes plus a short continuation notice. The read tool's
legacy `details.extractor` label is currently `zip` for all archive views;
that label is not a format discriminator. `contentType` remains `archive`.

## Workspace and descriptor ownership

The outer archive must be readable within the session's allowed workspace
roots. Format detection and every archive read use the same admitted file
descriptor; paths are not reopened after admission. TAR support does not alter
the separate ZIP-only reader used for Office and EPUB documents.

No archive member is extracted, written, followed as a filesystem link, or
passed to a shell. Replacing the outer pathname after opening cannot redirect
that descriptor to the replacement. This is not a filesystem snapshot: another
process modifying the same opened inode is not prevented.

## ZIP packages

Stored and DEFLATE members are supported. Encrypted entries, symbolic links,
special files, unsafe member paths, duplicate normalized names, contradictory
headers, and overlapping entries are rejected. ZIP metadata reads have byte
and operation budgets. Selected text is checked against its declared length
and CRC before it is returned.

## TAR source releases

The native streaming reader supports V7, POSIX ustar, GNU regular/directory
headers, GNU long names, and PAX local/global extended headers. Gzip-compressed
TAR is decoded using the existing Rust gzip library, not a subprocess. Only
recognized archive suffixes select this view: a standalone `log.gz` is not
implicitly treated as TAR. XZ, bzip2, and zstd are not implemented here.

PAX `path` and `size` override the ordinary header fields. Local overrides
apply to one member; global values persist until replaced or deleted. Empty
local values suppress a global override for that member, while empty global
values remove it. Unicode paths must be valid UTF-8. GNU long names are likewise
consumed by exactly one following member. Metadata headers and the optional
root directory marker are not counted as visible entries.

Ownership, timestamp, comment, and recognized xattr metadata are inert: they
are neither restored to the filesystem nor injected into the returned member
text. Unsupported attributes, alternate name encodings, sparse representations,
hard/symbolic links, devices, FIFOs, and other special member types are rejected.
Duplicate keys within a PAX header, consecutive per-member extension headers,
and extensions with no following member fail rather than choosing an ambiguous
interpretation. This is a bounded reader, not a complete implementation of every
vendor TAR extension.

The entire logical TAR stream is checked before a listing or selected member
is returned, including when the selected member appears first. Header checksums,
body lengths, padding boundaries, duplicate normalized names, and file/directory
conflicts are validated. Two zero end blocks are required; partial blocks or
nonzero data after them are errors. Concatenated gzip members can encode one
logical TAR stream, but a second appended TAR archive is rejected. Reading to
physical EOF validates gzip footers, including CRC and length, rather than
silently returning before a damaged trailer. Plain TAR checksums protect headers,
not file bodies; neither format provides authenticity.

## Resource limits and validation

Both formats admit at most 100 MiB of outer input, 64 MiB of declared member
content, and 16 MiB for one selected text member. TAR additionally bounds the
actual expanded stream (headers and padding included) to 100 MiB, physical
headers to 4,096, and read operations independently. Names are at most 4,096
UTF-8 bytes. Each TAR extension payload is at most 64 KiB, aggregate extended
metadata is at most 8 MiB, and each PAX header has at most 256 keys. GNU long-name
payloads must also fit the path bound plus their NUL terminator.

TAR scans skip unselected bodies in fixed-size chunks and retain only the
selected payload plus the bounded directory index. There is no full expanded
archive buffer, on-disk extraction cache, automatic retry, or external tool.
Limits and stricter admission rules can reject archives accepted by a general
purpose extraction utility.

The native regressions in `src/archive_read/tar/tests.rs` cover descriptor
ownership, the public read-tool route, pagination/hashline behavior, formats,
extended-header scoping, truncation, duplicates, gzip footers, and bounds.
Embedded PAX/GNU/ustar fixtures were produced by Python's standard `tarfile`
module and independently inspected with Python and GNU tar. Those fixture
checks do not execute the Rust reader. The authoritative quality entry point is
`dsr quality --tool pi_agent_rust`; compilation and native tests were not run in
the implementation environment because `dsr` was unavailable.

This extends the single-colon workflow in [OMP's files guide](https://omp.sh/docs/files).
Broader compression formats and archive mutation remain separate work.
