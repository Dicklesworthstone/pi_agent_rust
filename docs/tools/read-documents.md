# Read DOCX, PPTX, XLSX, and EPUB documents

The `read` tool extracts saved text from local Word documents, PowerPoint presentations, Excel workbooks, and EPUB books, and from document URLs. Conversion runs natively in the default build; it does not need an installed Office application, a conversion service, or an additional build feature.

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

```json
{"path":"spreadsheets/orders.xlsx","offset":1,"limit":100}
```

```json
{"path":"books/guide.epub","offset":1,"limit":100}
```

Local reads use the existing workspace scope checks and the already-opened file descriptor. URL reads use the existing download and redirect policy. Links and external relationships inside a document do not initiate additional downloads.

## Extracted content and order

**DOCX** returns document-body text in XML order. Adjacent text runs are joined; paragraphs, explicit line breaks, tabs, and Unicode text are retained. Text in deleted or moved-from revisions is omitted. Hyperlink labels are included as text, without opening their destinations. The output does not reproduce heading styles, fonts, page geometry, or automatic list numbering.

**PPTX** returns the slides listed by the presentation in their declared order. Each slide starts with `## Slide N`, where `N` is its one-based position in the presentation. Slide filenames and ZIP entry order do not determine the order. Unreferenced slide parts are not included. Text follows the XML order within each slide; spatial placement does not establish a separate visual reading order.

DOCX and PPTX preserve table rows and cells using pipe-delimited rows. A paragraph or line break inside a cell appears as `<br>`, and a literal pipe in cell text is escaped as `\|`. The converter does not infer a header row, expand merged cells, or reproduce table styling.

Supported document XML includes transitional and strict Office Open XML namespaces, UTF-8, and UTF-16 in either byte order. The main document part, presentation slides, workbook sheets, and shared-string tables are resolved from internal package relationships, so valid custom part locations are supported.

## Excel workbooks

**XLSX** returns sheets in workbook order under their saved names. Hidden and very hidden sheets are included and labelled; saved rows are read regardless of filtering or visibility. Empty sheets remain visible with `[No populated cells]`. Chart sheets are identified with `[Chart sheet: no worksheet cell data]`; their chart data is not rendered.

Tables preserve original coordinates. A `Row` column identifies the worksheet row, and Excel column letters identify the data columns. Empty rows and columns without populated cells are omitted, while gaps inside displayed rows remain empty cells. A value stored at `C9` therefore stays under column `C` on row `9`; sparse XML does not move it to another column. Cell references omitted by the producer use the next sequential row or column. Tables with more than 64 populated columns, or more than 100,000 displayed row/column combinations, use a `Cell | Value` table with full coordinates instead of expanding a large grid.

Shared strings, inline strings, and rich-text runs are joined without numeric coercion, so identifiers such as `0007` retain their leading zeros. Phonetic annotations are omitted. Booleans appear as `TRUE` or `FALSE`; error values such as `#DIV/0!` remain visible. Numeric values retain their stored text and precision. ISO date cells retain their saved date text. Number formats are not applied: date serials, percentages, and currencies stored as numbers remain the underlying numbers, without inferred dates, units, or symbols.

Formulas are never executed. The reader uses a saved cached result, which may be stale if the workbook was not recalculated before saving. A formula without a cached result displays `[Formula result unavailable: no cached value]`. Formula expressions, external workbook links, macros, and data connections are not evaluated or fetched. Merged regions retain their stored cells without copying an anchor value into the other cells. Table cell line breaks and literal pipes use the same escaping as DOCX/PPTX tables.

## EPUB books

**EPUB** returns saved title, author, language, publisher, date, and description metadata when present, followed by XHTML content in the package's declared reading-spine order. Each item begins with `## Section N`. ZIP order and chapter filenames do not establish the reading order. All listed spine items are included, including supplementary items marked `linear="no"`; unlisted resources are omitted.

The reader resolves the first supported rootfile in `META-INF/container.xml`, then the package manifest and spine. Custom package locations and percent-encoded filenames are supported. A non-XHTML spine item must provide an internal manifest fallback chain ending in XHTML. Missing chapters, missing or cyclic fallbacks, duplicate identities, malformed XML, and unsupported spine content are errors, including when a request asks only for the first display page. Empty sections remain visible with an explicit marker.

Chapter HTML is converted to Markdown. Headings, paragraphs, lists, links, and tables remain readable. Book navigation, footnotes, and text inside `aside`, `header`, and `footer` elements are retained. Scripts, styles, embedded executable content, and SVG are omitted. Images, audio, video, fonts, CSS layout, and DRM-protected content are not rendered or executed.

UTF-8 and UTF-16 XHTML are supported. HTML/XHTML doctype declarations may contain inert public or system identifiers; these identifiers are never fetched. Standard HTML named entities are recognized in chapter text and attributes. DTD internal subsets and custom entities are rejected. Container and package metadata continue to require XML predefined entities.

## Pagination and completeness

`offset` and `limit` address **lines of extracted text**. They are not Word page numbers, PowerPoint slide numbers, Excel row numbers, EPUB page numbers, ZIP offsets, or editable source locations. Start at `offset: 1` and use the continuation offset in the response to read subsequent text.

Each response body is limited to 2,000 lines and 1,000,000 bytes. A smaller positive `limit` requests fewer lines. When more extracted lines remain, the response includes a continuation notice and `details.nextOffset`. Relevant metadata includes `totalLines`, `offset`, `limit`, `outputLines`, `truncated`, and `byteLimited`. The complete extracted text is not duplicated in metadata.

Conversion validates all consumed parts before returning any extracted text. A conversion error is not a successful partial document read. Display pagination is separate: a successfully converted document can require multiple `read` calls. If one rendered line exceeds the response byte limit, the tool reports `DOCUMENT_LINE_LIMIT` instead of splitting that line into an unusable continuation.

Rendered lines cannot provide hashline edit anchors. DOCX, PPTX, XLSX, and EPUB are binary ZIP packages, and `raw: true` bypasses document conversion; it does not provide an editable document source representation. Use a document-aware editor to change the original file.

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
| EPUB spine items | 1,024 |
| EPUB manifest items | 4,096 |
| Cumulative rendering work in one EPUB chapter | 64 MiB |
| Workbook sheets, including chart sheets | 1,024 |
| Stored worksheet rows across the workbook | 100,000 |
| Stored worksheet cells across the workbook | 250,000 |
| Shared-string entries | 100,000 |
| Shared-string text combined | 8 MiB |
| Expanded cell values in one worksheet | 8 MiB |
| One cell or shared-string text value | 256 KiB |
| XML nesting depth | 128 levels |
| XML events in one part | 2,000,000 |
| Attributes on one XML element | 128 |
| One attribute value | 64 KiB |
| Qualified XML name or resolved namespace URI | 4,096 bytes |

MiB and KiB in the conversion limits use powers of 1,024. The 1,000,000-byte response body limit is separate. Archive-wide limits include members whose content is not rendered, such as images. A truncated URL download cannot be interpreted as a complete document package.

ZIP members must use stored or deflated compression. Encrypted packages, unsafe member paths, duplicate members, symbolic links, inconsistent archive headers, and corrupt consumed entries are rejected. The XML reader rejects malformed or truncated XML, DTD internal subsets, custom entities, invalid character references, undeclared namespace prefixes, and ambiguous attributes. Doctype declarations are accepted only for EPUB XHTML chapters under the rules above. Internal relationship targets must remain within the package. Missing or external main/slide/sheet/shared-string targets, duplicate slide or sheet identities, and multiple slides or sheets referring to one part are errors. XLSX also rejects duplicate or out-of-order stored row/cell coordinates, references outside Excel worksheet bounds, cells whose reference disagrees with the containing row, invalid shared-string indices, and ambiguous scalar values.

These checks do not execute document content. Macros, embedded objects, scripts, and processing instructions are never run.

## Format detection and current limits

Local documents are recognized by a case-insensitive `.docx`, `.pptx`, `.xlsx`, or `.epub` extension. URL documents can also be recognized by the exact corresponding media type; MIME parameters and URL queries/fragments do not prevent recognition:

- `application/vnd.openxmlformats-officedocument.wordprocessingml.document`
- `application/vnd.openxmlformats-officedocument.presentationml.presentation`
- `application/vnd.openxmlformats-officedocument.spreadsheetml.sheet`
- `application/epub+zip` (also `application/epub` and `application/x-epub+zip`)

Recognition selects a converter; the bytes must still form a valid supported package. Renaming a legacy binary file does not convert its format.

This reader extracts document-body text, slide-part text, saved worksheet cell values, and EPUB reading-spine text. It does not currently extract Word headers, footers, footnotes, endnotes, or comments; PowerPoint speaker notes; chart data; text from images or scanned pages; or linked/embedded document content. It does not render visual layouts. Legacy `.doc`/`.ppt`/`.xls`, binary `.xlsb` workbooks, and macro-enabled filename families are outside this conversion interface.

The native converter is implemented in [`src/document_read.rs`](../../src/document_read.rs), with Excel handling in [`src/document_read/xlsx.rs`](../../src/document_read/xlsx.rs) and EPUB handling in [`src/document_read/epub.rs`](../../src/document_read/epub.rs). Generated DOCX/PPTX/XLSX/EPUB ZIP fixtures cover text output, ordering, encodings, sparse coordinates, cached values, malformed packages, and resource bounds. ZIP validation is shared with [`src/archive_read.rs`](../../src/archive_read.rs).

