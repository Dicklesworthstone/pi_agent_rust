# Read DOCX and PPTX documents

The `read` tool extracts saved text from local Word documents and PowerPoint presentations, and from document URLs. Conversion runs natively in the default build; it does not need an installed Office application, a conversion service, or an additional build feature.

Pass the document path as ordinary `read` input:

```json
{"path":"docs/report.docx"}
```

```json
{"path":"presentations/review.pptx","offset":1,"limit":100}
```

```json
{"path":"https://example.org/downloads/review.pptx"}
```

Local reads use the existing workspace scope checks and the already-opened file descriptor. URL reads use the existing download and redirect policy. Links and external relationships inside a document do not initiate additional downloads.

## Extracted content and order

**DOCX** returns document-body text in XML order. Adjacent text runs are joined; paragraphs, explicit line breaks, tabs, and Unicode text are retained. Text in deleted or moved-from revisions is omitted. Hyperlink labels are included as text, without opening their destinations. The output does not reproduce heading styles, fonts, page geometry, or automatic list numbering.

**PPTX** returns the slides listed by the presentation in their declared order. Each slide starts with `## Slide N`, where `N` is its one-based position in the presentation. Slide filenames and ZIP entry order do not determine the order. Unreferenced slide parts are not included. Text follows the XML order within each slide; spatial placement does not establish a separate visual reading order.

Both formats preserve table rows and cells using pipe-delimited rows. A paragraph or line break inside a cell appears as `<br>`, and a literal pipe in cell text is escaped as `\|`. The converter does not infer a header row, expand merged cells, or reproduce table styling.

Supported document XML includes transitional and strict Office Open XML namespaces, UTF-8, and UTF-16 in either byte order. The main document part and presentation slides are resolved from internal package relationships, so valid custom part locations are supported.

## Pagination and completeness

`offset` and `limit` address **lines of extracted text**. They are not Word page numbers, PowerPoint slide numbers, ZIP offsets, or editable source locations. Start at `offset: 1` and use the continuation offset in the response to read subsequent text.

Each response body is limited to 2,000 lines and 1,000,000 bytes. A smaller positive `limit` requests fewer lines. When more extracted lines remain, the response includes a continuation notice and `details.nextOffset`. Relevant metadata includes `totalLines`, `offset`, `limit`, `outputLines`, `truncated`, and `byteLimited`. The complete extracted text is not duplicated in metadata.

Conversion validates all consumed parts before returning any extracted text. A conversion error is not a successful partial document read. Display pagination is separate: a successfully converted document can require multiple `read` calls. If one rendered line exceeds the response byte limit, the tool reports `DOCUMENT_LINE_LIMIT` instead of splitting that line into an unusable continuation.

Rendered lines cannot provide hashline edit anchors. DOCX and PPTX are binary ZIP packages, and `raw: true` bypasses document conversion; it does not provide an editable Word or PowerPoint source representation. Use a document-aware editor to change the original file.

## Bounds and rejected input

The reader enforces these limits before or during conversion:

| Resource | Limit |
| --- | ---: |
| Complete document input | 10 MiB |
| ZIP entries | 4,096 |
| Declared uncompressed size of all archive entries | 64 MiB |
| One consumed XML part | 16 MiB |
| All consumed XML parts combined | 32 MiB |
| Extracted text before display pagination | 8 MiB |
| Presentation slides | 1,024 |
| XML nesting depth | 128 levels |
| XML events in one part | 2,000,000 |
| Attributes on one XML element | 128 |
| One attribute value | 64 KiB |
| Qualified XML name or resolved namespace URI | 4,096 bytes |

MiB and KiB in the conversion limits use powers of 1,024. The 1,000,000-byte response body limit is separate. Archive-wide limits include members whose content is not rendered, such as images. A truncated URL download cannot be interpreted as a complete Office package.

ZIP members must use stored or deflated compression. Encrypted packages, unsafe member paths, duplicate members, symbolic links, inconsistent archive headers, and corrupt consumed entries are rejected. The XML reader rejects malformed or truncated XML, DTDs, custom entities, invalid character references, undeclared namespace prefixes, and ambiguous attributes. Internal relationship targets must remain within the package. Missing or external main/slide targets, duplicate slide IDs, and multiple slide IDs referring to one part are errors.

These checks do not execute document content. Macros, embedded objects, scripts, and processing instructions are never run.

## Format detection and current limits

Local documents are recognized by a case-insensitive `.docx` or `.pptx` extension. URL documents can also be recognized by the exact corresponding media type; MIME parameters and URL queries/fragments do not prevent recognition:

- `application/vnd.openxmlformats-officedocument.wordprocessingml.document`
- `application/vnd.openxmlformats-officedocument.presentationml.presentation`

Recognition selects a converter; the bytes must still form a valid supported package. Renaming a legacy binary file does not convert its format.

This reader extracts document-body and slide-part text. It does not currently extract Word headers, footers, footnotes, endnotes, or comments; PowerPoint speaker notes; chart data; text from images or scanned pages; or linked/embedded document content. It does not render visual layouts. Legacy `.doc`/`.ppt`, spreadsheets such as `.xlsx`, and macro-enabled filename families are outside this conversion interface.

The native converter is implemented in [`src/document_read.rs`](../../src/document_read.rs), with generated DOCX/PPTX ZIP fixtures covering text output, ordering, encodings, malformed packages, and resource bounds. ZIP validation is shared with [`src/archive_read.rs`](../../src/archive_read.rs).

