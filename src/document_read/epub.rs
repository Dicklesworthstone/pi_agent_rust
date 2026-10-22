//! EPUB package metadata and spine-ordered XHTML text extraction.
//!
//! No resource outside the admitted ZIP is fetched. Non-text spine items,
//! broken references, and custom DTD entities fail explicitly instead of silently
//! omitting a chapter. XHTML doctype identifiers are inert and never fetched.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::{
    ArchiveReader, Cursor, HashMap, HashSet, Result, TextOutput, XmlBudget, XmlEvent,
    MAX_DOCUMENT_BYTES, MAX_OUTPUT_BYTES, MAX_XML_BYTES, decode_xml, document_error, resolve_target,
    visit_xml,
};

const CONTAINER_NS: &str = "urn:oasis:names:tc:opendocument:xmlns:container";
const PACKAGE_NS: &str = "http://www.idpf.org/2007/opf";
const METADATA_NS: &str = "http://purl.org/dc/elements/1.1/";
const XHTML_NS: &str = "http://www.w3.org/1999/xhtml";
const MAX_SPINE_ITEMS: usize = 1024;
const MAX_MANIFEST_ITEMS: usize = 4096;
// Charge rendered child strings before their parents combine them. This also
// bounds repeated formatting work in deeply nested lists and quotations.
const MAX_RENDER_WORK_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Tag {
    Container,
    Rootfiles,
    Rootfile,
    Package,
    Metadata,
    Manifest,
    Item,
    Spine,
    Itemref,
    Title,
    Creator,
    Language,
    Publisher,
    Date,
    Description,
    Html,
    Body,
}

pub(super) fn tag(namespace: &str, local: &str) -> Option<Tag> {
    Some(match (namespace, local) {
        (CONTAINER_NS, "container") => Tag::Container,
        (CONTAINER_NS, "rootfiles") => Tag::Rootfiles,
        (CONTAINER_NS, "rootfile") => Tag::Rootfile,
        (PACKAGE_NS, "package") => Tag::Package,
        (PACKAGE_NS, "metadata") => Tag::Metadata,
        (PACKAGE_NS, "manifest") => Tag::Manifest,
        (PACKAGE_NS, "item") => Tag::Item,
        (PACKAGE_NS, "spine") => Tag::Spine,
        (PACKAGE_NS, "itemref") => Tag::Itemref,
        (METADATA_NS, "title") => Tag::Title,
        (METADATA_NS, "creator") => Tag::Creator,
        (METADATA_NS, "language") => Tag::Language,
        (METADATA_NS, "publisher") => Tag::Publisher,
        (METADATA_NS, "date") => Tag::Date,
        (METADATA_NS, "description") => Tag::Description,
        (XHTML_NS, "html") => Tag::Html,
        (XHTML_NS, "body") => Tag::Body,
        _ => return None,
    })
}

#[derive(Default)]
pub(super) struct Properties {
    full_path: Option<String>,
    media_type: Option<String>,
    href: Option<String>,
    idref: Option<String>,
    fallback: Option<String>,
}

pub(super) fn property<'a>(
    namespace: &str,
    local: &str,
    properties: &'a mut Properties,
) -> Option<&'a mut Option<String>> {
    if !namespace.is_empty() {
        return None;
    }
    Some(match local {
        "full-path" => &mut properties.full_path,
        "media-type" => &mut properties.media_type,
        "href" => &mut properties.href,
        "idref" => &mut properties.idref,
        "fallback" => &mut properties.fallback,
        _ => return None,
    })
}

fn required(value: Option<String>, label: &str) -> Result<String> {
    value
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| document_error(format!("EPUB {label} is missing")))
}

fn package_target(source: &str, target: &str) -> Result<String> {
    if target.starts_with('/') {
        return Err(document_error("EPUB resources must use package-relative paths"));
    }
    resolve_target(source, target)
}

fn rootfile(bytes: &[u8]) -> Result<String> {
    use super::Tag::Epub;
    let mut rootfiles = 0;
    let mut selected = None;
    let mut count = 0usize;
    visit_xml(bytes, Epub(Tag::Container), |event, parents| {
        match event {
            XmlEvent::Start(Epub(Tag::Rootfiles), _) => {
                rootfiles += 1;
                if parents != [Epub(Tag::Container)] || rootfiles != 1 {
                    return Err(document_error("invalid EPUB rootfiles list"));
                }
            }
            XmlEvent::Start(Epub(Tag::Rootfile), attributes) => {
                count += 1;
                if parents != [Epub(Tag::Container), Epub(Tag::Rootfiles)]
                    || count > MAX_MANIFEST_ITEMS
                {
                    return Err(document_error("invalid or excessive EPUB rootfiles"));
                }
                if attributes.epub.media_type.as_deref() == Some("application/oebps-package+xml")
                    && selected.is_none()
                {
                    let path = required(attributes.epub.full_path, "rootfile path")?;
                    selected = Some(package_target("", &path)?);
                }
            }
            _ => {}
        }
        Ok(())
    })?;
    selected.ok_or_else(|| document_error("EPUB has no supported package rootfile"))
}

struct Item {
    href: String,
    media_type: String,
    fallback: Option<String>,
}

struct Package {
    metadata: Vec<(&'static str, String)>,
    manifest: HashMap<String, Item>,
    spine: Vec<String>,
}

fn metadata_label(tag: Tag) -> Option<&'static str> {
    match tag {
        Tag::Title => Some("Title"),
        Tag::Creator => Some("Author"),
        Tag::Language => Some("Language"),
        Tag::Publisher => Some("Publisher"),
        Tag::Date => Some("Date"),
        Tag::Description => Some("Description"),
        _ => None,
    }
}

fn read_package(bytes: &[u8]) -> Result<Package> {
    use super::Tag::Epub;
    let mut package = Package {
        metadata: Vec::new(),
        manifest: HashMap::new(),
        spine: Vec::new(),
    };
    let mut sections = HashSet::new();
    let mut references = HashSet::new();
    let mut current_metadata: Option<(Tag, &'static str, TextOutput)> = None;
    visit_xml(bytes, Epub(Tag::Package), |event, parents| {
        match event {
            XmlEvent::Start(Epub(current @ (Tag::Metadata | Tag::Manifest | Tag::Spine)), _) => {
                if parents != [Epub(Tag::Package)] || !sections.insert(current) {
                    return Err(document_error("duplicate or misplaced EPUB package section"));
                }
            }
            XmlEvent::Start(Epub(Tag::Item), attributes) => {
                if parents != [Epub(Tag::Package), Epub(Tag::Manifest)]
                    || package.manifest.len() >= MAX_MANIFEST_ITEMS
                {
                    return Err(document_error("invalid or excessive EPUB manifest"));
                }
                let id = required(attributes.id, "manifest ID")?;
                let item = Item {
                    href: required(attributes.epub.href, "manifest href")?,
                    media_type: required(attributes.epub.media_type, "manifest media type")?,
                    fallback: attributes.epub.fallback,
                };
                if package.manifest.insert(id, item).is_some() {
                    return Err(document_error("duplicate EPUB manifest ID"));
                }
            }
            XmlEvent::Start(Epub(Tag::Itemref), attributes) => {
                if parents != [Epub(Tag::Package), Epub(Tag::Spine)]
                    || package.spine.len() >= MAX_SPINE_ITEMS
                {
                    return Err(document_error("invalid EPUB spine or more than 1024 items"));
                }
                let id = required(attributes.epub.idref, "spine reference")?;
                if !references.insert(id.clone()) {
                    return Err(document_error("duplicate EPUB spine reference"));
                }
                package.spine.push(id);
            }
            XmlEvent::Start(Epub(current), _) => {
                if parents == [Epub(Tag::Package), Epub(Tag::Metadata)]
                    && let Some(label) = metadata_label(current)
                {
                    current_metadata = Some((current, label, TextOutput::default()));
                }
            }
            XmlEvent::Text(text) => {
                if let Some((_, _, output)) = &mut current_metadata {
                    output.append(text)?;
                }
            }
            XmlEvent::End(Epub(current)) => {
                if parents == [Epub(Tag::Package), Epub(Tag::Metadata)]
                    && current_metadata.as_ref().is_some_and(|(active, _, _)| *active == current)
                    && let Some((_, label, output)) = current_metadata.take()
                {
                    let value = output.text.split_whitespace().collect::<Vec<_>>().join(" ");
                    if !value.is_empty() {
                        package.metadata.push((label, value));
                    }
                }
            }
            _ => {}
        }
        Ok(())
    })?;
    if package.manifest.is_empty() || package.spine.is_empty() {
        return Err(document_error("EPUB requires a manifest and a nonempty reading spine"));
    }
    Ok(package)
}

fn content_item<'a>(package: &'a Package, id: &'a str) -> Result<&'a Item> {
    let mut current = id;
    let mut visited = HashSet::new();
    loop {
        if !visited.insert(current) {
            return Err(document_error("cyclic EPUB content fallback"));
        }
        let item = package
            .manifest
            .get(current)
            .ok_or_else(|| document_error("EPUB spine or fallback refers to a missing item"))?;
        if item.media_type == "application/xhtml+xml" {
            return Ok(item);
        }
        current = item
            .fallback
            .as_deref()
            .filter(|fallback| !fallback.is_empty())
            .ok_or_else(|| document_error("EPUB spine item has no supported XHTML fallback"))?;
    }
}

/// Permit inert HTML/XHTML declarations without interpreting DTD content.
/// Internal subsets and trailing declarations are deliberately unsupported.
pub(super) fn validate_doctype(declaration: &str) -> Result<()> {
    let whitespace = |c| matches!(c, ' ' | '\t' | '\r' | '\n');
    let declaration = declaration.trim_matches(whitespace);
    let mut rest = declaration
        .strip_prefix("html")
        .ok_or_else(|| document_error("EPUB XHTML doctype must name html"))?;
    if rest.is_empty() {
        return Ok(());
    }
    if !rest.starts_with(whitespace) {
        return Err(document_error("invalid EPUB XHTML doctype"));
    }
    rest = rest.trim_start_matches(whitespace);
    let literals = if let Some(value) = rest.strip_prefix("PUBLIC") {
        rest = value;
        2
    } else if let Some(value) = rest.strip_prefix("SYSTEM") {
        rest = value;
        1
    } else {
        return Err(document_error("EPUB DTD internal subsets are unsupported"));
    };
    for _ in 0..literals {
        if !rest.starts_with(whitespace) {
            return Err(document_error("invalid EPUB XHTML doctype identifier"));
        }
        rest = rest.trim_start_matches(whitespace);
        let quote = rest
            .chars()
            .next()
            .filter(|quote| matches!(*quote, '\'' | '"'))
            .ok_or_else(|| document_error("unquoted EPUB XHTML doctype identifier"))?;
        let end = rest[1..]
            .find(quote)
            .ok_or_else(|| document_error("unterminated EPUB XHTML doctype identifier"))?;
        rest = &rest[end + 2..];
    }
    if !rest.trim_matches(whitespace).is_empty() {
        return Err(document_error("EPUB DTD internal subsets are unsupported"));
    }
    Ok(())
}

/// Render table cells and rows without padding them to the widest cell.
/// The default HTML converter's alignment padding can multiply a single long
/// header by every row, even when the complete input is small.
fn compact_table_content(tag: &str, content: &str) -> Result<String> {
    let mut output = TextOutput::default();
    match tag {
        "td" | "th" => {
            output.append(" ")?;
            for part in content.trim().split_inclusive(['|', '\n', '\r']) {
                if let Some(prefix) = part.strip_suffix('|') {
                    output.append(prefix)?;
                    output.append("&#124;")?;
                } else if let Some(prefix) = part.strip_suffix('\n') {
                    output.append(prefix)?;
                    output.append("<br>")?;
                } else if let Some(prefix) = part.strip_suffix('\r') {
                    output.append(prefix)?;
                } else {
                    output.append(part)?;
                }
            }
            output.append(" |")?;
        }
        "tr" => {
            let content = content.trim();
            if !content.is_empty() {
                output.append("| ")?;
                output.append(content)?;
                output.append("\n")?;
            }
        }
        "table" => {
            output.append("\n\n")?;
            let mut header_seen = false;
            for line in content.lines().map(str::trim).filter(|line| !line.is_empty()) {
                output.append(line)?;
                output.append("\n")?;
                if !header_seen && line.starts_with('|') && line.ends_with('|') {
                    // Cell text has already escaped every literal pipe. The
                    // remaining pipes describe the actual cells, including
                    // empty cells, so this separator needs no rectangular grid.
                    let columns = line.bytes().filter(|byte| *byte == b'|').count() - 1;
                    output.append("|")?;
                    for _ in 0..columns {
                        output.append(" --- |")?;
                    }
                    output.append("\n")?;
                    header_seen = true;
                }
            }
            output.append("\n")?;
        }
        _ => return Err(document_error("unexpected EPUB table element")),
    }
    Ok(output.text)
}

fn chapter_text(bytes: &[u8]) -> Result<String> {
    use super::Tag::Epub;
    let mut bodies = 0usize;
    visit_xml(bytes, Epub(Tag::Html), |event, parents| {
        if let XmlEvent::Start(Epub(Tag::Body), _) = event {
            bodies += 1;
            if parents != [Epub(Tag::Html)] || bodies != 1 {
                return Err(document_error("invalid EPUB XHTML body"));
            }
        }
        Ok(())
    })?;
    if bodies != 1 {
        return Err(document_error("EPUB XHTML chapter has no body"));
    }
    let (html, _) = decode_xml(bytes)?;
    let render_overflow = Arc::new(AtomicBool::new(false));
    let render_work = Arc::new(AtomicUsize::new(0));
    let handler_overflow = Arc::clone(&render_overflow);
    let handler_work = Arc::clone(&render_work);
    let converter = htmd::HtmlToMarkdown::builder()
        .skip_tags(vec![
            "head", "script", "style", "noscript", "iframe", "object", "embed", "svg",
        ])
        .add_handler(
            // Wrap htmd's built-in formatting handlers. Unknown elements use
            // its non-expanding child traversal. Skipped tags still fall back
            // to the skip handler registered above.
            vec![
                "html", "head", "body", "a", "img", "ol", "ul", "li", "blockquote",
                "code", "pre", "strong", "b", "i", "em", "h1", "h2", "h3", "h4",
                "h5", "h6", "br", "hr", "table", "tr", "td", "th", "thead", "tbody",
                "tfoot", "caption", "p", "span", "address", "article", "aside", "base",
                "basefont", "center", "col", "colgroup", "dd", "details", "dialog",
                "dir", "div", "dl", "dt", "fieldset", "figcaption", "figure", "footer",
                "form", "frame", "frameset", "header", "iframe", "legend", "link",
                "main", "menu", "menuitem", "nav", "noframes", "optgroup", "option",
                "param", "script", "search", "section", "style", "summary", "textarea",
                "title", "track",
            ],
            move |handlers: &dyn htmd::element_handler::Handlers, element: htmd::Element<'_>| {
                if handler_overflow.load(Ordering::Relaxed) {
                    return None;
                }
                let result = if matches!(element.tag, "table" | "tr" | "td" | "th") {
                    let content = handlers.walk_children(element.node).content;
                    match compact_table_content(element.tag, &content) {
                        Ok(content) => Some(content.into()),
                        Err(_) => {
                            handler_overflow.store(true, Ordering::Relaxed);
                            None
                        }
                    }
                } else {
                    handlers.fallback(element)
                };
                if handler_overflow.load(Ordering::Relaxed) {
                    return None;
                }
                let result = result?;
                if result.content.len() > MAX_OUTPUT_BYTES {
                    handler_overflow.store(true, Ordering::Relaxed);
                    return None;
                }
                if handler_work
                    .try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                        used.checked_add(result.content.len())
                            .filter(|total| *total <= MAX_RENDER_WORK_BYTES)
                    })
                    .is_err()
                {
                    // htmd callbacks cannot return an error. Preserve both the
                    // failure and its cause instead of silently dropping text.
                    handler_work.store(usize::MAX, Ordering::Relaxed);
                    handler_overflow.store(true, Ordering::Relaxed);
                    return None;
                }
                Some(result)
            },
        )
        .build();
    let markdown = converter.convert(&html).map_err(document_error)?;
    if render_work.load(Ordering::Relaxed) == usize::MAX {
        return Err(document_error("EPUB chapter exceeds the 64 MiB render-work limit"));
    }
    if render_overflow.load(Ordering::Relaxed) || markdown.len() > MAX_OUTPUT_BYTES {
        return Err(document_error("EPUB chapter exceeds the 8 MiB rendered-content limit"));
    }
    Ok(markdown)
}

pub(super) fn convert(bytes: &[u8]) -> Result<String> {
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return Err(document_error("EPUB exceeds the 10 MiB compressed document limit"));
    }
    let mut archive = ArchiveReader::new(Cursor::new(bytes))?;
    if archive.read_member("mimetype", 128)? != b"application/epub+zip" {
        return Err(document_error("invalid EPUB mimetype member"));
    }
    let mut budget = XmlBudget(MAX_XML_BYTES);
    let main_part = rootfile(&budget.read(&mut archive, "META-INF/container.xml")?)?;
    let package = read_package(&budget.read(&mut archive, &main_part)?)?;
    let mut output = TextOutput::default();
    for (label, value) in &package.metadata {
        output.append(&format!("**{label}:** {value}\n"))?;
    }
    output.separate("\n\n")?;
    for (index, id) in package.spine.iter().enumerate() {
        let item = content_item(&package, id)?;
        let path = package_target(&main_part, &item.href)?;
        let bytes = budget.read(&mut archive, &path)?;
        let markdown = chapter_text(&bytes)?;
        output.append(&format!("## Section {}\n\n", index + 1))?;
        if markdown.trim().is_empty() {
            output.append("[No extractable text in this section]")?;
        } else {
            output.append(markdown.trim())?;
        }
        output.separate("\n\n")?;
    }
    Ok(output.text.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn book(manifest: &str, spine: &str, chapters: &[(&str, &str)]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        let container = format!(
            r#"<container xmlns="{CONTAINER_NS}"><rootfiles><rootfile full-path="Book/package.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#
        );
        let package = format!(
            r#"<package xmlns="{PACKAGE_NS}" xmlns:dc="{METADATA_NS}"><metadata><dc:title>Reader &amp; Book</dc:title><dc:creator>One Author</dc:creator></metadata><manifest>{manifest}</manifest><spine>{spine}</spine></package>"#
        );
        for (name, content) in [
            ("mimetype", "application/epub+zip"),
            ("META-INF/container.xml", container.as_str()),
            ("Book/package.opf", package.as_str()),
        ] {
            zip.start_file(name, options).expect("fixture member");
            zip.write_all(content.as_bytes()).expect("fixture contents");
        }
        for (name, content) in chapters {
            zip.start_file(*name, options).expect("chapter member");
            zip.write_all(content.as_bytes()).expect("chapter contents");
        }
        zip.finish().expect("fixture archive").into_inner()
    }

    fn xhtml(body: &str) -> String {
        format!(r#"<html xmlns="{XHTML_NS}"><head><title>Hidden title</title></head><body>{body}</body></html>"#)
    }

    fn render(bytes: &[u8]) -> Result<String> {
        super::super::convert_epub(bytes)
    }

    #[test]
    fn metadata_and_chapters_follow_spine_instead_of_zip_order() {
        let first = xhtml("<h1>First chapter</h1><p>First &amp; important.</p>");
        let second = xhtml("<p>Second chapter.</p><script>do_not_show()</script>");
        let bytes = book(
            r#"<item id="b" href="second.xhtml" media-type="application/xhtml+xml"/><item id="a" href="first.xhtml" media-type="application/xhtml+xml"/>"#,
            r#"<itemref idref="a"/><itemref idref="b"/>"#,
            &[("Book/second.xhtml", &second), ("Book/first.xhtml", &first)],
        );
        let text = render(&bytes).expect("EPUB text");
        assert!(text.contains("**Title:** Reader & Book"));
        assert!(text.contains("**Author:** One Author"));
        assert!(text.find("First chapter").unwrap() < text.find("Second chapter").unwrap());
        assert!(text.contains("First & important."));
        assert!(!text.contains("do_not_show"));
        assert!(!text.contains("Hidden title"));
    }

    #[test]
    fn resolves_encoded_paths_and_declared_xhtml_fallbacks() {
        let chapter = xhtml("<p>Fallback content.</p>");
        let bytes = book(
            r#"<item id="cover" href="cover.svg" media-type="image/svg+xml" fallback="text"/><item id="text" href="./text%20part.xhtml" media-type="application/xhtml+xml"/>"#,
            r#"<itemref idref="cover"/>"#,
            &[("Book/text part.xhtml", &chapter)],
        );
        assert!(render(&bytes).expect("fallback").contains("Fallback content."));
    }

    #[test]
    fn broken_missing_and_external_content_never_silently_omit_chapters() {
        let chapter = xhtml("<p>Valid.</p>");
        for href in ["https://example.org/chapter.xhtml", "../../escape.xhtml", "missing.xhtml"] {
            let manifest = format!(r#"<item id="a" href="{href}" media-type="application/xhtml+xml"/>"#);
            let bytes = book(&manifest, r#"<itemref idref="a"/>"#, &[("Book/a.xhtml", &chapter)]);
            assert!(render(&bytes).is_err(), "{href}");
        }
        let missing_reference = book(
            r#"<item id="a" href="a.xhtml" media-type="application/xhtml+xml"/>"#,
            r#"<itemref idref="missing"/>"#,
            &[("Book/a.xhtml", &chapter)],
        );
        assert!(render(&missing_reference).is_err());
    }

    #[test]
    fn duplicate_ids_cycles_and_unsupported_spine_are_rejected() {
        for manifest in [
            r#"<item id="a" href="a.xhtml" media-type="application/xhtml+xml"/><item id="a" href="b.xhtml" media-type="application/xhtml+xml"/>"#,
            r#"<item id="a" href="a.svg" media-type="image/svg+xml" fallback="b"/><item id="b" href="b.svg" media-type="image/svg+xml" fallback="a"/>"#,
            r#"<item id="a" href="a.svg" media-type="image/svg+xml"/>"#,
        ] {
            assert!(render(&book(manifest, r#"<itemref idref="a"/>"#, &[])).is_err());
        }
    }

    #[test]
    fn xhtml_doctypes_are_inert_and_common_named_entities_are_supported() {
        for declaration in [
            "<!DOCTYPE html>",
            r#"<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.1//EN" "http://www.w3.org/TR/xhtml11/DTD/xhtml11.dtd">"#,
            r#"<!DOCTYPE html SYSTEM "https://example.invalid/must-not-be-fetched.dtd">"#,
        ] {
            let valid = format!(
                r#"{declaration}<html xmlns="{XHTML_NS}"><body><p title="A&nbsp;B">Readable &copy; &eacute;.</p><aside>Footnote.</aside><nav>Contents.</nav></body></html>"#
            );
            let text = chapter_text(valid.as_bytes()).expect("inert doctype and named entities");
            assert!(text.contains("Readable © é."));
            assert!(text.contains("Footnote."));
            assert!(text.contains("Contents."));
        }
        for invalid in [
            format!(r#"<!DOCTYPE html [<!ENTITY local "replacement">]><html xmlns="{XHTML_NS}"><body>&local;</body></html>"#),
            format!(r#"<!DOCTYPE html><!DOCTYPE html><html xmlns="{XHTML_NS}"><body/></html>"#),
            format!(r#"<html xmlns="{XHTML_NS}"><body><p>broken</body></html>"#),
            format!(r#"<html xmlns="{XHTML_NS}"><body>&unknown;</body></html>"#),
            format!(r#"<html xmlns="{XHTML_NS}"><body/><body/></html>"#),
        ] {
            assert!(chapter_text(invalid.as_bytes()).is_err());
        }
    }

    #[test]
    fn html_entity_support_does_not_relax_package_or_office_xml() {
        let container = format!(
            r#"<container xmlns="{CONTAINER_NS}"><rootfiles><rootfile full-path="&nbsp;.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#
        );
        assert!(rootfile(container.as_bytes()).is_err());
        let package = format!(
            r#"<package xmlns="{PACKAGE_NS}" xmlns:dc="{METADATA_NS}"><metadata><dc:title>&copy;</dc:title></metadata><manifest><item id="a" href="a.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="a"/></spine></package>"#
        );
        assert!(read_package(package.as_bytes()).is_err());
        let office = br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="&nbsp;" Type="x" Target="a.xml"/></Relationships>"#;
        assert!(super::super::read_relationships(office).is_err());
        let office_doctype = br#"<!DOCTYPE Relationships><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"/>"#;
        assert!(super::super::read_relationships(office_doctype).is_err());
    }

    #[test]
    fn tables_preserve_cells_without_multiplying_wide_headers_by_row_count() {
        let header = "Wide".repeat(16 * 1024);
        let mut body = format!(
            "<table><thead><tr><th>{header}</th><th>Notes</th><th>Empty</th></tr></thead><tbody>"
        );
        for row in 0..256 {
            body.push_str(&format!(
                "<tr><td>row {row}</td><td>first<br/>second | third</td><td/></tr>"
            ));
        }
        body.push_str("</tbody></table>");
        let chapter = xhtml(&body);
        let bytes = book(
            r#"<item id="a" href="a.xhtml" media-type="application/xhtml+xml"/>"#,
            r#"<itemref idref="a"/>"#,
            &[("Book/a.xhtml", &chapter)],
        );
        let text = render(&bytes).expect("compact table");
        assert!(text.contains(&header), "the complete long header is retained");
        assert!(text.contains("row 255"), "the final row is retained");
        assert!(text.contains("<br>"), "cell line breaks remain visible");
        assert!(text.contains("&#124;"), "literal pipes cannot split a cell");
        assert!(text.contains("| --- | --- | --- |"), "all three columns remain");
        let row = text.lines().find(|line| line.starts_with("| row 255 |")).unwrap();
        assert_eq!(row.bytes().filter(|byte| *byte == b'|').count(), 4);
        assert!(row.ends_with("|  |"), "the final empty cell remains present");
        assert!(text.len() < 100_000, "the unpadded table must remain compact");
    }

    #[test]
    fn nested_quotations_have_a_cumulative_render_work_budget() {
        let ordinary = xhtml(
            "<blockquote><p>A saved quotation.</p><p>Its second paragraph.</p></blockquote>"
        );
        let text = chapter_text(ordinary.as_bytes()).expect("ordinary quotation");
        assert!(text.contains("A saved quotation."));
        assert!(text.contains("Its second paragraph."));

        // Every outer quotation prefixes every line of its child's rendered
        // result. No single result needs to reach 8 MiB for the accumulated
        // formatting work to become excessive.
        let mut body = "<blockquote>".repeat(80);
        body.push_str("<pre>");
        body.push_str(&"x\n".repeat(24 * 1024));
        body.push_str("</pre>");
        body.push_str(&"</blockquote>".repeat(80));
        let chapter = xhtml(&body);
        let bytes = book(
            r#"<item id="a" href="a.xhtml" media-type="application/xhtml+xml"/>"#,
            r#"<itemref idref="a"/>"#,
            &[("Book/a.xhtml", &chapter)],
        );
        let error = render(&bytes).expect_err("nested formatting exceeds the work budget");
        assert!(error.to_string().contains("render-work limit"));
    }

    #[test]
    fn table_cell_expansion_fails_the_whole_read_instead_of_omitting_content() {
        // Encoding pipes for a Markdown table expands each one to six bytes.
        // The input itself fits both the ZIP and XML limits.
        let pipes = "|".repeat(MAX_OUTPUT_BYTES / 6 + 1);
        let chapter = xhtml(&format!("<table><tr><td>{pipes}</td></tr></table>"));
        let bytes = book(
            r#"<item id="a" href="a.xhtml" media-type="application/xhtml+xml"/>"#,
            r#"<itemref idref="a"/>"#,
            &[("Book/a.xhtml", &chapter)],
        );
        let error = render(&bytes).expect_err("rendered cell exceeds the output budget");
        assert!(error.to_string().contains("rendered-content limit"));
    }

    #[test]
    fn incomplete_url_download_is_rejected_and_mime_routes_to_epub() {
        let chapter = xhtml("<p>URL book.</p>");
        let bytes = book(
            r#"<item id="a" href="a.xhtml" media-type="application/xhtml+xml"/>"#,
            r#"<itemref idref="a"/>"#,
            &[("Book/a.xhtml", &chapter)],
        );
        let out = crate::url_read::convert_bytes(
            "https://example.org/download", "application/epub+zip", &bytes, false,
        ).expect("URL EPUB");
        assert_eq!(out.kind, crate::url_read::UrlContentKind::Epub);
        assert!(out.content.contains("URL book."));
        assert!(crate::url_read::convert_bytes(
            "https://example.org/book.epub", "", &bytes, true,
        ).is_err());
    }
}
