//! Native, bounded text extraction from DOCX, PPTX, and XLSX packages.
//!
//! Only document XML and internal package relationships are interpreted. Images,
//! charts, macros, embedded objects, and external relationships are never opened
//! or executed. Presentation order comes from the presentation's slide list,
//! rather than ZIP entry order or slide filenames.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::io::Cursor;

use quick_xml::XmlVersion;
use quick_xml::events::{BytesDecl, BytesRef, BytesStart, Event};
use quick_xml::name::{NamespaceResolver, ResolveResult};
use quick_xml::reader::NsReader;

use crate::archive_read::ArchiveReader;
use crate::error::{Error, Result};

mod xlsx;

/// Maximum compressed document size, shared by local and URL conversion.
pub const MAX_DOCUMENT_BYTES: usize = 10 * 1024 * 1024;
const MAX_PART_BYTES: u64 = 16 * 1024 * 1024;
const MAX_XML_BYTES: u64 = 32 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_XML_DEPTH: usize = 128;
const MAX_XML_EVENTS: usize = 2_000_000;
const MAX_XML_NAME_BYTES: usize = 4096;
const MAX_SLIDES: usize = 1024;

const WORD_NS: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const DRAWING_NS: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
const PRESENTATION_NS: &str = "http://schemas.openxmlformats.org/presentationml/2006/main";
const SPREADSHEET_NS: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const RELATIONSHIPS_NS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
const OFFICE_REL_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const STRICT_WORD_NS: &str = "http://purl.oclc.org/ooxml/wordprocessingml/main";
const STRICT_DRAWING_NS: &str = "http://purl.oclc.org/ooxml/drawingml/main";
const STRICT_PRESENTATION_NS: &str = "http://purl.oclc.org/ooxml/presentationml/main";
const STRICT_SPREADSHEET_NS: &str = "http://purl.oclc.org/ooxml/spreadsheetml/main";
const STRICT_OFFICE_REL_NS: &str = "http://purl.oclc.org/ooxml/officeDocument/relationships";

/// Supported Office Open XML document families.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentKind {
    Docx,
    Pptx,
    Xlsx,
}

impl DocumentKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Docx => "docx",
            Self::Pptx => "pptx",
            Self::Xlsx => "xlsx",
        }
    }
}

/// Recognize the requested format from a filename/URL or its exact media type.
/// The converter subsequently validates the bytes; a filename is not proof of
/// a valid package. Legacy binary Office formats are deliberately excluded.
#[must_use]
pub fn recognize(path: &str, content_type: &str) -> Option<DocumentKind> {
    let media_type = content_type.split(';').next().unwrap_or_default().trim();
    if media_type.eq_ignore_ascii_case(
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    ) {
        return Some(DocumentKind::Docx);
    }
    if media_type.eq_ignore_ascii_case(
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    ) {
        return Some(DocumentKind::Pptx);
    }
    if media_type.eq_ignore_ascii_case(
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    ) {
        return Some(DocumentKind::Xlsx);
    }
    let path = path.split(['?', '#']).next().unwrap_or(path);
    let extension = std::path::Path::new(path).extension()?.to_str()?;
    if extension.eq_ignore_ascii_case("docx") {
        Some(DocumentKind::Docx)
    } else if extension.eq_ignore_ascii_case("pptx") {
        Some(DocumentKind::Pptx)
    } else if extension.eq_ignore_ascii_case("xlsx") {
        Some(DocumentKind::Xlsx)
    } else {
        None
    }
}

/// Convert a complete, already-admitted local or downloaded document.
///
/// Package metadata and every consumed XML part must be valid. A malformed,
/// encrypted, incomplete, oversized, or ambiguous package returns an error
/// instead of partial text that could be mistaken for the whole document.
pub fn convert(bytes: &[u8], kind: DocumentKind) -> Result<String> {
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return Err(document_error("document exceeds the 10 MiB input limit"));
    }
    let mut archive = ArchiveReader::new(Cursor::new(bytes))?;
    let mut budget = XmlBudget(MAX_XML_BYTES);
    let package_rels = read_relationships(&budget.read(&mut archive, "_rels/.rels")?)?;
    let mut main_part = None;
    for relationship in package_rels.values() {
        if is_relationship(&relationship.kind, "officeDocument") {
            if main_part.is_some() {
                return Err(document_error("multiple main document relationships"));
            }
            main_part = Some(relationship.internal_target("")?);
        }
    }
    let main_part = main_part.ok_or_else(|| document_error("missing main document relationship"))?;
    let main_xml = budget.read(&mut archive, &main_part)?;
    let mut output = TextOutput::default();
    match kind {
        DocumentKind::Docx => render_text(&main_xml, Tag::Document, &mut output)?,
        DocumentKind::Pptx => {
            let slides = presentation_order(&main_xml)?;
            let rels_path = relationship_part(&main_part);
            let relationships = read_relationships(&budget.read(&mut archive, &rels_path)?)?;
            let mut visited = HashSet::new();
            for (index, id) in slides.iter().enumerate() {
                let relationship = relationships
                    .get(id)
                    .ok_or_else(|| document_error("slide relationship is missing"))?;
                if !is_relationship(&relationship.kind, "slide") {
                    return Err(document_error("slide ID refers to a non-slide relationship"));
                }
                let target = relationship.internal_target(&main_part)?;
                if !visited.insert(target.clone()) {
                    return Err(document_error("multiple slide IDs refer to the same part"));
                }
                let xml = budget.read(&mut archive, &target)?;
                output.append(&format!("## Slide {}\n\n", index + 1))?;
                render_text(&xml, Tag::Slide, &mut output)?;
                output.separate("\n\n")?;
            }
        }
        DocumentKind::Xlsx => {
            xlsx::render(&main_xml, &main_part, &mut archive, &mut budget, &mut output)?;
        }
    }
    while output.text.ends_with('\n') {
        output.text.pop();
    }
    Ok(output.text)
}

fn document_error(message: impl std::fmt::Display) -> Error {
    Error::tool("read", format!("[DOCUMENT_INVALID] {message}"))
}

struct XmlBudget(u64);

impl XmlBudget {
    fn read(&mut self, archive: &mut ArchiveReader<Cursor<&[u8]>>, name: &str) -> Result<Vec<u8>> {
        if self.0 == 0 {
            return Err(document_error("document exceeds the 32 MiB XML budget"));
        }
        let bytes = archive.read_member(name, self.0.min(MAX_PART_BYTES))?;
        self.0 -= bytes.len() as u64;
        Ok(bytes)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tag {
    Document,
    Body,
    WordRun,
    WordParagraph,
    WordText,
    WordTable,
    WordRow,
    WordCell,
    WordBreak,
    WordTab,
    Deleted,
    NoBreakHyphen,
    SoftHyphen,
    DrawingParagraph,
    DrawingText,
    DrawingTable,
    DrawingRow,
    DrawingCell,
    DrawingBreak,
    Presentation,
    SlideList,
    SlideId,
    Slide,
    Workbook,
    WorkbookSheets,
    WorkbookSheet,
    SharedStrings,
    SharedString,
    SpreadsheetRun,
    SpreadsheetText,
    Worksheet,
    ChartSheet,
    SheetData,
    SheetRow,
    SheetCell,
    CellValue,
    CellFormula,
    CellInlineString,
    Relationships,
    Relationship,
    Other,
}

fn namespace_name(namespace: ResolveResult<'_>) -> Result<&str> {
    match namespace {
        ResolveResult::Bound(namespace) if namespace.0.len() <= MAX_XML_NAME_BYTES => {
            Ok(namespace.0)
        }
        ResolveResult::Bound(_) => Err(document_error("XML namespace exceeds 4096 bytes")),
        ResolveResult::Unbound => Ok(""),
        ResolveResult::Unknown(_) => Err(document_error("undeclared XML namespace prefix")),
    }
}

fn tag(namespace: &str, local: &str) -> Tag {
    match (namespace, local) {
        (WORD_NS | STRICT_WORD_NS, "document") => Tag::Document,
        (WORD_NS | STRICT_WORD_NS, "body") => Tag::Body,
        (WORD_NS | STRICT_WORD_NS, "r") => Tag::WordRun,
        (WORD_NS | STRICT_WORD_NS, "p") => Tag::WordParagraph,
        (WORD_NS | STRICT_WORD_NS, "t") => Tag::WordText,
        (WORD_NS | STRICT_WORD_NS, "tbl") => Tag::WordTable,
        (WORD_NS | STRICT_WORD_NS, "tr") => Tag::WordRow,
        (WORD_NS | STRICT_WORD_NS, "tc") => Tag::WordCell,
        (WORD_NS | STRICT_WORD_NS, "br" | "cr") => Tag::WordBreak,
        (WORD_NS | STRICT_WORD_NS, "tab") => Tag::WordTab,
        (WORD_NS | STRICT_WORD_NS, "del" | "moveFrom") => Tag::Deleted,
        (WORD_NS | STRICT_WORD_NS, "noBreakHyphen") => Tag::NoBreakHyphen,
        (WORD_NS | STRICT_WORD_NS, "softHyphen") => Tag::SoftHyphen,
        (DRAWING_NS | STRICT_DRAWING_NS, "p") => Tag::DrawingParagraph,
        (DRAWING_NS | STRICT_DRAWING_NS, "t") => Tag::DrawingText,
        (DRAWING_NS | STRICT_DRAWING_NS, "tbl") => Tag::DrawingTable,
        (DRAWING_NS | STRICT_DRAWING_NS, "tr") => Tag::DrawingRow,
        (DRAWING_NS | STRICT_DRAWING_NS, "tc") => Tag::DrawingCell,
        (DRAWING_NS | STRICT_DRAWING_NS, "br") => Tag::DrawingBreak,
        (PRESENTATION_NS | STRICT_PRESENTATION_NS, "presentation") => Tag::Presentation,
        (PRESENTATION_NS | STRICT_PRESENTATION_NS, "sldIdLst") => Tag::SlideList,
        (PRESENTATION_NS | STRICT_PRESENTATION_NS, "sldId") => Tag::SlideId,
        (PRESENTATION_NS | STRICT_PRESENTATION_NS, "sld") => Tag::Slide,
        (SPREADSHEET_NS | STRICT_SPREADSHEET_NS, "workbook") => Tag::Workbook,
        (SPREADSHEET_NS | STRICT_SPREADSHEET_NS, "sheets") => Tag::WorkbookSheets,
        (SPREADSHEET_NS | STRICT_SPREADSHEET_NS, "sheet") => Tag::WorkbookSheet,
        (SPREADSHEET_NS | STRICT_SPREADSHEET_NS, "sst") => Tag::SharedStrings,
        (SPREADSHEET_NS | STRICT_SPREADSHEET_NS, "si") => Tag::SharedString,
        (SPREADSHEET_NS | STRICT_SPREADSHEET_NS, "r") => Tag::SpreadsheetRun,
        (SPREADSHEET_NS | STRICT_SPREADSHEET_NS, "t") => Tag::SpreadsheetText,
        (SPREADSHEET_NS | STRICT_SPREADSHEET_NS, "worksheet") => Tag::Worksheet,
        (SPREADSHEET_NS | STRICT_SPREADSHEET_NS, "chartsheet") => Tag::ChartSheet,
        (SPREADSHEET_NS | STRICT_SPREADSHEET_NS, "sheetData") => Tag::SheetData,
        (SPREADSHEET_NS | STRICT_SPREADSHEET_NS, "row") => Tag::SheetRow,
        (SPREADSHEET_NS | STRICT_SPREADSHEET_NS, "c") => Tag::SheetCell,
        (SPREADSHEET_NS | STRICT_SPREADSHEET_NS, "v") => Tag::CellValue,
        (SPREADSHEET_NS | STRICT_SPREADSHEET_NS, "f") => Tag::CellFormula,
        (SPREADSHEET_NS | STRICT_SPREADSHEET_NS, "is") => Tag::CellInlineString,
        (RELATIONSHIPS_NS, "Relationships") => Tag::Relationships,
        (RELATIONSHIPS_NS, "Relationship") => Tag::Relationship,
        _ => Tag::Other,
    }
}

#[derive(Default)]
struct Properties {
    id: Option<String>,
    relationship_id: Option<String>,
    target: Option<String>,
    target_mode: Option<String>,
    kind: Option<String>,
    name: Option<String>,
    reference: Option<String>,
    cell_type: Option<String>,
    sheet_id: Option<String>,
    sheet_state: Option<String>,
}

fn properties(resolver: &NamespaceResolver, start: &BytesStart<'_>) -> Result<Properties> {
    let mut result = Properties::default();
    let mut names = HashSet::new();
    for (index, attribute) in start.attributes().enumerate() {
        if index >= 128 {
            return Err(document_error("too many attributes on an XML element"));
        }
        let attribute = attribute.map_err(document_error)?;
        validate_qname(attribute.key.as_ref())?;
        let (namespace, local) = resolver.resolve_attribute(attribute.key);
        let namespace = namespace_name(namespace)?;
        if !names.insert((namespace.to_owned(), local.as_ref().to_owned())) {
            return Err(document_error("duplicate expanded XML attribute name"));
        }
        if attribute.value.len() > 64 * 1024 || attribute.value.contains('<') {
            return Err(document_error("invalid or oversized XML attribute"));
        }
        let value = attribute
            .normalized_value(XmlVersion::Implicit1_0)
            .map_err(document_error)?;
        if !value.chars().all(xml_character) {
            return Err(document_error("invalid XML character in attribute"));
        }
        let slot = match (namespace, local.as_ref()) {
            ("", "Id" | "id") => &mut result.id,
            (OFFICE_REL_NS | STRICT_OFFICE_REL_NS, "id") => &mut result.relationship_id,
            ("", "Target") => &mut result.target,
            ("", "TargetMode") => &mut result.target_mode,
            ("", "Type") => &mut result.kind,
            ("", "name") => &mut result.name,
            ("", "r") => &mut result.reference,
            ("", "t") => &mut result.cell_type,
            ("", "sheetId") => &mut result.sheet_id,
            ("", "state") => &mut result.sheet_state,
            _ => continue,
        };
        if slot.replace(value.into_owned()).is_some() {
            return Err(document_error("ambiguous document attribute"));
        }
    }
    Ok(result)
}

enum XmlEvent<'a> {
    Start(Tag, Properties),
    End(Tag),
    Text(&'a str),
}

/// Strict event traversal shared by content and relationship parsing. Parent
/// tags exclude the current start/end element, and include a text's container.
fn visit_xml(
    bytes: &[u8],
    expected_root: Tag,
    mut visit: impl FnMut(XmlEvent<'_>, &[Tag]) -> Result<()>,
) -> Result<()> {
    let (text, encoding) = decode_xml(bytes)?;
    let mut reader = NsReader::from_str(&text);
    reader.config_mut().enable_all_checks(true);
    reader.config_mut().expand_empty_elements = true;
    let mut parents = Vec::new();
    let mut root_seen = false;
    let mut first_event = true;
    for _ in 0..MAX_XML_EVENTS {
        let event = reader.read_event().map_err(document_error)?;
        match event {
            Event::Start(start) => {
                validate_qname(start.name().as_ref())?;
                let (namespace, local) = reader.resolver().resolve_element(start.name());
                let current = tag(namespace_name(namespace)?, local.as_ref());
                if parents.is_empty() {
                    if root_seen || current != expected_root {
                        return Err(document_error("unexpected or multiple XML document roots"));
                    }
                    root_seen = true;
                }
                if parents.len() >= MAX_XML_DEPTH {
                    return Err(document_error("XML nesting exceeds 128 levels"));
                }
                let attributes = properties(reader.resolver(), &start)?;
                visit(XmlEvent::Start(current, attributes), &parents)?;
                parents.push(current);
            }
            Event::End(_) => {
                let current = parents
                    .pop()
                    .ok_or_else(|| document_error("unmatched XML end"))?;
                visit(XmlEvent::End(current), &parents)?;
            }
            Event::Text(text) => {
                if text.contains("]]>") {
                    return Err(document_error("CDATA terminator in ordinary XML text"));
                }
                let text = text.xml10_content();
                if parents.is_empty() {
                    if !text.chars().all(|c| matches!(c, ' ' | '\t' | '\r' | '\n')) {
                        return Err(document_error("text outside the XML document root"));
                    }
                } else {
                    visit(XmlEvent::Text(&text), &parents)?;
                }
            }
            Event::CData(text) => {
                if parents.is_empty() {
                    return Err(document_error("CDATA outside the XML document root"));
                }
                visit(XmlEvent::Text(&text.xml10_content()), &parents)?;
            }
            Event::GeneralRef(reference) => {
                if parents.is_empty() {
                    return Err(document_error("entity outside the XML document root"));
                }
                visit(XmlEvent::Text(&resolve_reference(&reference)?), &parents)?;
            }
            Event::DocType(_) => return Err(document_error("XML document types are forbidden")),
            Event::Decl(declaration) => {
                if !first_event {
                    return Err(document_error("misplaced XML declaration"));
                }
                validate_declaration(&declaration, encoding)?;
            }
            Event::PI(instruction) => validate_instruction(instruction.as_ref())?,
            Event::Comment(_) => {}
            Event::Eof => {
                return if root_seen && parents.is_empty() {
                    Ok(())
                } else {
                    Err(document_error("incomplete XML document"))
                };
            }
            Event::Empty(_) => return Err(document_error("unexpanded XML element")),
        }
        first_event = false;
    }
    Err(document_error("XML event budget exceeded"))
}

fn resolve_reference(reference: &BytesRef<'_>) -> Result<Cow<'static, str>> {
    if let Some(character) = reference.resolve_char_ref().map_err(document_error)? {
        if !xml_character(character) {
            return Err(document_error("invalid XML character reference"));
        }
        return Ok(Cow::Owned(character.to_string()));
    }
    Ok(Cow::Borrowed(match reference.as_ref() {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "apos" => "'",
        "quot" => "\"",
        _ => return Err(document_error("custom XML entities are forbidden")),
    }))
}

fn validate_declaration(declaration: &BytesDecl<'_>, encoding: XmlEncoding) -> Result<()> {
    let start = BytesStart::from_content(declaration.as_ref(), 3);
    let mut seen = HashSet::new();
    for (index, attribute) in start.attributes().enumerate() {
        let attribute = attribute.map_err(document_error)?;
        let key = attribute.key.as_ref();
        let value = attribute.value.as_ref();
        if !seen.insert(key.to_owned())
            || (index == 0 && key != "version")
            || !match key {
                "version" => value == "1.0",
                "encoding" => encoding.accepts(value) && !seen.contains("standalone"),
                "standalone" => matches!(value, "yes" | "no"),
                _ => false,
            }
        {
            return Err(document_error("invalid or unsupported XML declaration"));
        }
    }
    if !seen.contains("version") {
        return Err(document_error("XML declaration is missing its version"));
    }
    Ok(())
}

fn validate_instruction(instruction: &str) -> Result<()> {
    let target = instruction
        .split([' ', '\t', '\r', '\n'])
        .next()
        .unwrap_or_default();
    validate_qname(target)?;
    if target.eq_ignore_ascii_case("xml") {
        return Err(document_error("reserved XML processing instruction target"));
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum XmlEncoding {
    Utf8,
    Utf16Le,
    Utf16Be,
}

impl XmlEncoding {
    fn accepts(self, declaration: &str) -> bool {
        match self {
            Self::Utf8 => declaration.eq_ignore_ascii_case("UTF-8"),
            Self::Utf16Le => {
                declaration.eq_ignore_ascii_case("UTF-16")
                    || declaration.eq_ignore_ascii_case("UTF-16LE")
            }
            Self::Utf16Be => {
                declaration.eq_ignore_ascii_case("UTF-16")
                    || declaration.eq_ignore_ascii_case("UTF-16BE")
            }
        }
    }
}

fn decode_xml(bytes: &[u8]) -> Result<(Cow<'_, str>, XmlEncoding)> {
    if bytes.len() as u64 > MAX_PART_BYTES {
        return Err(document_error("XML part exceeds 16 MiB"));
    }
    let (bytes, encoding) = if let Some(bytes) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        (bytes, XmlEncoding::Utf16Le)
    } else if let Some(bytes) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        (bytes, XmlEncoding::Utf16Be)
    } else if bytes.starts_with(&[b'<', 0]) {
        (bytes, XmlEncoding::Utf16Le)
    } else if bytes.starts_with(&[0, b'<']) {
        (bytes, XmlEncoding::Utf16Be)
    } else {
        (
            bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes),
            XmlEncoding::Utf8,
        )
    };
    let text = match encoding {
        XmlEncoding::Utf8 => Cow::Borrowed(std::str::from_utf8(bytes).map_err(document_error)?),
        XmlEncoding::Utf16Le | XmlEncoding::Utf16Be => {
            if !bytes.len().is_multiple_of(2) {
                return Err(document_error("truncated UTF-16 XML"));
            }
            let units: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|pair| match encoding {
                    XmlEncoding::Utf16Le => u16::from_le_bytes([pair[0], pair[1]]),
                    _ => u16::from_be_bytes([pair[0], pair[1]]),
                })
                .collect();
            Cow::Owned(String::from_utf16(&units).map_err(document_error)?)
        }
    };
    if !text.chars().all(xml_character) {
        return Err(document_error("invalid XML character"));
    }
    Ok((text, encoding))
}

fn xml_character(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\r'
            | '\u{20}'..='\u{D7FF}'
            | '\u{E000}'..='\u{FFFD}'
            | '\u{10000}'..='\u{10FFFF}'
    )
}

fn validate_qname(name: &str) -> Result<()> {
    if name.len() > MAX_XML_NAME_BYTES {
        return Err(document_error("XML qualified name exceeds 4096 bytes"));
    }
    let mut parts = name.split(':');
    let valid = |part: &str| {
        let mut characters = part.chars();
        characters.next().is_some_and(xml_name_start)
            && characters.all(|c| {
                xml_name_start(c)
                    || matches!(
                        c,
                        '-' | '.' | '0'..='9'
                            | '\u{B7}'
                            | '\u{300}'..='\u{36F}'
                            | '\u{203F}'..='\u{2040}'
                    )
            })
    };
    if !parts.next().is_some_and(valid)
        || parts.next().is_some_and(|part| !valid(part))
        || parts.next().is_some()
    {
        return Err(document_error("invalid XML qualified name"));
    }
    Ok(())
}

fn xml_name_start(c: char) -> bool {
    matches!(
        c,
        'A'..='Z' | '_' | 'a'..='z'
            | '\u{C0}'..='\u{D6}'
            | '\u{D8}'..='\u{F6}'
            | '\u{F8}'..='\u{2FF}'
            | '\u{370}'..='\u{37D}'
            | '\u{37F}'..='\u{1FFF}'
            | '\u{200C}'..='\u{200D}'
            | '\u{2070}'..='\u{218F}'
            | '\u{2C00}'..='\u{2FEF}'
            | '\u{3001}'..='\u{D7FF}'
            | '\u{F900}'..='\u{FDCF}'
            | '\u{FDF0}'..='\u{FFFD}'
            | '\u{10000}'..='\u{EFFFF}'
    )
}

struct Relationship {
    kind: String,
    target: String,
    external: bool,
}

impl Relationship {
    fn internal_target(&self, source: &str) -> Result<String> {
        if self.external {
            return Err(document_error("document content must use internal relationships"));
        }
        resolve_target(source, &self.target)
    }
}

fn is_relationship(kind: &str, wanted: &str) -> bool {
    [OFFICE_REL_NS, STRICT_OFFICE_REL_NS]
        .into_iter()
        .any(|namespace| {
            kind.strip_prefix(namespace)
                .and_then(|suffix| suffix.strip_prefix('/'))
                == Some(wanted)
        })
}

fn read_relationships(bytes: &[u8]) -> Result<HashMap<String, Relationship>> {
    let mut relationships = HashMap::new();
    visit_xml(bytes, Tag::Relationships, |event, parents| {
        if let XmlEvent::Start(Tag::Relationship, attributes) = event {
            if parents != [Tag::Relationships] || relationships.len() >= 4096 {
                return Err(document_error("invalid or excessive package relationships"));
            }
            let id = attributes
                .id
                .filter(|id| !id.is_empty())
                .ok_or_else(|| document_error("relationship is missing its ID"))?;
            let target = attributes
                .target
                .filter(|target| !target.is_empty())
                .ok_or_else(|| document_error("relationship is missing its target"))?;
            let kind = attributes
                .kind
                .filter(|kind| !kind.is_empty())
                .ok_or_else(|| document_error("relationship is missing its type"))?;
            let external = match attributes.target_mode.as_deref() {
                None | Some("Internal") => false,
                Some("External") => true,
                _ => return Err(document_error("invalid relationship target mode")),
            };
            if relationships
                .insert(
                    id,
                    Relationship {
                        kind,
                        target,
                        external,
                    },
                )
                .is_some()
            {
                return Err(document_error("duplicate relationship ID"));
            }
        }
        Ok(())
    })?;
    Ok(relationships)
}

fn relationship_part(source: &str) -> String {
    source.rsplit_once('/').map_or_else(
        || format!("_rels/{source}.rels"),
        |(directory, filename)| format!("{directory}/_rels/{filename}.rels"),
    )
}

/// Resolve an OPC internal part URI without filesystem access. Encoded path
/// separators and URI schemes are rejected before dot-segment normalization.
fn resolve_target(source: &str, target: &str) -> Result<String> {
    if target.starts_with("//")
        || target.contains(['\\', ':', '?', '#'])
        || target.chars().any(char::is_control)
    {
        return Err(document_error("unsafe document relationship target"));
    }
    let mut decoded = Vec::with_capacity(target.len());
    let mut bytes = target.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let high = bytes.next().and_then(|c| char::from(c).to_digit(16));
            let low = bytes.next().and_then(|c| char::from(c).to_digit(16));
            let (Some(high), Some(low)) = (high, low) else {
                return Err(document_error("invalid percent encoding in document target"));
            };
            let byte = u8::try_from(high * 16 + low).map_err(document_error)?;
            if matches!(byte, b'/' | b'\\' | b':' | b'?' | b'#' | b'%') || byte.is_ascii_control() {
                return Err(document_error("encoded separator in document relationship"));
            }
            decoded.push(byte);
        } else {
            decoded.push(byte);
        }
    }
    let decoded = std::str::from_utf8(&decoded).map_err(document_error)?;
    if decoded.chars().any(char::is_control) {
        return Err(document_error("control character in document relationship"));
    }
    let mut parts: Vec<&str> = if target.starts_with('/') {
        Vec::new()
    } else {
        source
            .rsplit_once('/')
            .map_or_else(Vec::new, |(directory, _)| directory.split('/').collect())
    };
    for part in decoded.trim_start_matches('/').split('/') {
        match part {
            "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return Err(document_error("relationship escapes the document package"));
                }
            }
            "" => return Err(document_error("empty document relationship path component")),
            _ => parts.push(part),
        }
    }
    if parts.is_empty() {
        return Err(document_error("empty document relationship target"));
    }
    Ok(parts.join("/"))
}

fn presentation_order(bytes: &[u8]) -> Result<Vec<String>> {
    let mut slides = Vec::new();
    let mut ids = HashSet::new();
    let mut relationship_ids = HashSet::new();
    let mut lists = 0;
    visit_xml(bytes, Tag::Presentation, |event, parents| {
        match event {
            XmlEvent::Start(Tag::SlideList, _) => {
                lists += 1;
                if parents != [Tag::Presentation] || lists > 1 {
                    return Err(document_error("invalid presentation slide list"));
                }
            }
            XmlEvent::Start(Tag::SlideId, attributes) => {
                if parents != [Tag::Presentation, Tag::SlideList] || slides.len() >= MAX_SLIDES {
                    return Err(document_error("invalid slide list or more than 1024 slides"));
                }
                let id = attributes
                    .id
                    .ok_or_else(|| document_error("slide is missing its ID"))?
                    .parse::<u32>()
                    .map_err(document_error)?;
                let relationship_id = attributes
                    .relationship_id
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| document_error("slide is missing its relationship ID"))?;
                if !(256..2_147_483_648).contains(&id)
                    || !ids.insert(id)
                    || !relationship_ids.insert(relationship_id.clone())
                {
                    return Err(document_error("invalid or duplicate presentation slide ID"));
                }
                slides.push(relationship_id);
            }
            _ => {}
        }
        Ok(())
    })?;
    Ok(slides)
}

#[derive(Default)]
struct TextOutput {
    text: String,
}

impl TextOutput {
    fn append(&mut self, text: &str) -> Result<()> {
        if self.text.len().saturating_add(text.len()) > MAX_OUTPUT_BYTES {
            return Err(document_error("extracted text exceeds the 8 MiB output limit"));
        }
        self.text.push_str(text);
        Ok(())
    }

    fn separate(&mut self, separator: &str) -> Result<()> {
        if !self.text.is_empty() && !self.text.ends_with(separator) {
            self.append(if separator == "\n\n" && self.text.ends_with('\n') {
                "\n"
            } else {
                separator
            })?;
        }
        Ok(())
    }

    fn content(&mut self, text: &str, in_cell: bool) -> Result<()> {
        if !in_cell {
            return self.append(text);
        }
        for part in text.split_inclusive(['|', '\n']) {
            if let Some(prefix) = part.strip_suffix('|') {
                self.append(prefix)?;
                self.append("\\|")?;
            } else if let Some(prefix) = part.strip_suffix('\n') {
                self.append(prefix)?;
                self.append("<br>")?;
            } else {
                self.append(part)?;
            }
        }
        Ok(())
    }
}

fn render_text(bytes: &[u8], root: Tag, output: &mut TextOutput) -> Result<()> {
    let mut body_count = 0;
    visit_xml(bytes, root, |event, parents| {
        if let XmlEvent::Start(Tag::Body, _) = &event {
            body_count += 1;
            if root != Tag::Document || parents != [Tag::Document] || body_count > 1 {
                return Err(document_error("invalid document body"));
            }
        }
        if parents.contains(&Tag::Deleted)
            || (root == Tag::Document && !parents.contains(&Tag::Body))
        {
            return Ok(());
        }
        let in_cell = parents
            .iter()
            .any(|tag| matches!(tag, Tag::WordCell | Tag::DrawingCell));
        match event {
            XmlEvent::Text(text)
                if matches!(parents.last(), Some(Tag::WordText | Tag::DrawingText)) =>
            {
                output.content(text, in_cell)?;
            }
            XmlEvent::Start(Tag::WordBreak, _) if parents.last() == Some(&Tag::WordRun) => {
                output.append(if in_cell { "<br>" } else { "\n" })?;
            }
            XmlEvent::Start(Tag::DrawingBreak, _)
                if parents.last() == Some(&Tag::DrawingParagraph) =>
            {
                output.append(if in_cell { "<br>" } else { "\n" })?;
            }
            XmlEvent::Start(Tag::WordTab, _) if parents.last() == Some(&Tag::WordRun) => {
                output.append("\t")?;
            }
            XmlEvent::Start(Tag::NoBreakHyphen, _) if parents.last() == Some(&Tag::WordRun) => {
                output.append("\u{2011}")?;
            }
            XmlEvent::Start(Tag::SoftHyphen, _) if parents.last() == Some(&Tag::WordRun) => {
                output.append("\u{AD}")?;
            }
            XmlEvent::Start(Tag::WordRow | Tag::DrawingRow, _) => {
                output.separate(if in_cell { "<br>" } else { "\n" })?;
                output.append("|")?;
            }
            XmlEvent::Start(Tag::WordCell | Tag::DrawingCell, _) => output.append(" ")?,
            XmlEvent::End(Tag::WordParagraph | Tag::DrawingParagraph) => {
                output.append(if in_cell { "<br>" } else { "\n\n" })?;
            }
            XmlEvent::End(Tag::WordCell | Tag::DrawingCell) => {
                while output.text.ends_with("<br>") {
                    output.text.truncate(output.text.len() - 4);
                }
                output.append(" |")?;
            }
            XmlEvent::End(Tag::WordRow | Tag::DrawingRow) => {
                output.append(if in_cell { "<br>" } else { "\n" })?;
            }
            XmlEvent::End(Tag::WordTable | Tag::DrawingTable) => {
                output.separate(if in_cell { "<br>" } else { "\n\n" })?;
            }
            _ => {}
        }
        Ok(())
    })?;
    if root == Tag::Document && body_count != 1 {
        return Err(document_error("document body is missing"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use zip::CompressionMethod;
    use zip::write::SimpleFileOptions;

    const DOCX_RELS: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;
    const PPTX_RELS: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/></Relationships>"#;
    const CONTENT_TYPES: &str = r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/></Types>"#;

    fn zip_fixture(entries: &[(&str, &[u8])], method: CompressionMethod) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(method);
        for (name, contents) in entries {
            writer.start_file(*name, options).expect("start fixture member");
            writer.write_all(contents).expect("write fixture member");
        }
        writer.finish().expect("finish fixture ZIP").into_inner()
    }

    fn word_xml(body: &str) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><w:document xmlns:w=\"{WORD_NS}\" xmlns:r=\"{OFFICE_REL_NS}\"><w:body>{body}</w:body></w:document>"
        )
    }

    fn docx_with_xml(xml: &[u8]) -> Vec<u8> {
        zip_fixture(
            &[
                ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
                ("_rels/.rels", DOCX_RELS.as_bytes()),
                ("word/document.xml", xml),
            ],
            CompressionMethod::Deflated,
        )
    }

    fn presentation_xml(slides: &str) -> String {
        format!(
            "<p:presentation xmlns:p=\"{PRESENTATION_NS}\" xmlns:r=\"{OFFICE_REL_NS}\"><p:sldIdLst>{slides}</p:sldIdLst></p:presentation>"
        )
    }

    fn slide_xml(content: &str) -> String {
        format!(
            "<p:sld xmlns:p=\"{PRESENTATION_NS}\" xmlns:a=\"{DRAWING_NS}\"><p:cSld><p:spTree>{content}</p:spTree></p:cSld></p:sld>"
        )
    }

    fn slide_relationships(content: &str) -> String {
        format!("<Relationships xmlns=\"{RELATIONSHIPS_NS}\">{content}</Relationships>")
    }

    fn pptx_fixture(presentation: &str, rels: &str, slides: &[(&str, &[u8])]) -> Vec<u8> {
        let mut entries = vec![
            ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
            ("_rels/.rels", PPTX_RELS.as_bytes()),
            ("ppt/presentation.xml", presentation.as_bytes()),
            ("ppt/_rels/presentation.xml.rels", rels.as_bytes()),
        ];
        entries.extend_from_slice(slides);
        zip_fixture(&entries, CompressionMethod::Deflated)
    }

    #[test]
    fn recognizes_document_filenames_and_exact_media_types() {
        assert_eq!(
            recognize("https://example.test/DATA.XLSX?download=1", ""),
            Some(DocumentKind::Xlsx)
        );
        assert_eq!(
            recognize(
                "https://example.test/download",
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet; charset=binary",
            ),
            Some(DocumentKind::Xlsx)
        );
        assert_eq!(
            recognize("https://example.test/SCORE.DOCX?download=1", ""),
            Some(DocumentKind::Docx)
        );
        assert_eq!(
            recognize("slides.PPTX#preview", "application/octet-stream"),
            Some(DocumentKind::Pptx)
        );
        assert_eq!(
            recognize(
                "download",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document; charset=binary"
            ),
            Some(DocumentKind::Docx)
        );
        assert_eq!(
            recognize(
                "download",
                "application/vnd.openxmlformats-officedocument.presentationml.presentation"
            ),
            Some(DocumentKind::Pptx)
        );
        assert_eq!(recognize("legacy.doc", "application/msword"), None);
        assert_eq!(
            recognize("sheet.xlsx", "application/zip"),
            Some(DocumentKind::Xlsx)
        );
        assert_eq!(recognize("sheet.xls", "application/zip"), None);
        assert_eq!(recognize("plain.txt?name=.docx", "text/plain"), None);
    }

    #[test]
    fn docx_preserves_runs_unicode_paragraphs_breaks_and_tables() {
        let xml = word_xml(
            r#"<w:p><w:r><w:t xml:space="preserve">A &amp; &lt; &gt; &quot; &apos; é 世界 </w:t></w:r><w:r><w:t>&#127932;</w:t></w:r></w:p>
            <w:p><w:r><w:t xml:space="preserve">before </w:t></w:r><w:del><w:r><w:t>deleted</w:t></w:r></w:del><w:hyperlink r:id="externalLink"><w:r><w:t>link</w:t></w:r></w:hyperlink><w:r><w:tab/><w:t>tab</w:t><w:br/><w:t>new line</w:t></w:r></w:p>
            <w:tbl><w:tr><w:tc><w:p><w:r><w:t>Name</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>Value</w:t></w:r></w:p></w:tc></w:tr>
            <w:tr><w:tc><w:p><w:r><w:t>A|B</w:t></w:r></w:p><w:p><w:r><w:t>second paragraph</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>42</w:t></w:r></w:p></w:tc></w:tr></w:tbl>
            <w:p><w:r><w:t>After the table</w:t></w:r></w:p>"#,
        );
        let result =
            convert(&docx_with_xml(xml.as_bytes()), DocumentKind::Docx).expect("DOCX text");
        assert_eq!(
            result,
            "A & < > \" ' é 世界 🎼\n\nbefore link\ttab\nnew line\n\n| Name | Value |\n| A\\|B<br>second paragraph | 42 |\n\nAfter the table"
        );
    }

    #[test]
    fn docx_uses_namespace_uris_and_supports_utf16_parts() {
        let xml = format!(
            "<?xml version='1.0' encoding='UTF-16'?><document xmlns='{STRICT_WORD_NS}'><body><p><r><t>café 日本 🎶</t></r></p></body></document>"
        );
        for little_endian in [true, false] {
            let mut encoded = if little_endian {
                vec![0xFF, 0xFE]
            } else {
                vec![0xFE, 0xFF]
            };
            for unit in xml.encode_utf16() {
                encoded.extend_from_slice(&if little_endian {
                    unit.to_le_bytes()
                } else {
                    unit.to_be_bytes()
                });
            }
            assert_eq!(
                convert(&docx_with_xml(&encoded), DocumentKind::Docx).expect("UTF-16 DOCX"),
                "café 日本 🎶"
            );
        }
    }

    #[test]
    fn docx_tab_stop_properties_do_not_inject_text() {
        let xml = word_xml(
            "<w:p><w:pPr><w:tabs><w:tab w:val='left' w:pos='720'/></w:tabs></w:pPr><w:r><w:t>one</w:t><w:tab/><w:t>two</w:t><w:noBreakHyphen/><w:t>three</w:t></w:r></w:p>",
        );
        assert_eq!(
            convert(&docx_with_xml(xml.as_bytes()), DocumentKind::Docx).expect("paragraph tabs"),
            "one\ttwo‑three"
        );
    }

    #[test]
    fn docx_follows_package_main_relationship_and_cdata() {
        let rels = format!(
            "<Relationships xmlns='{RELATIONSHIPS_NS}'><Relationship Id='main' Type='{STRICT_OFFICE_REL_NS}/officeDocument' Target='/parts/main%20document.xml'/></Relationships>"
        );
        let xml =
            word_xml("<w:p><w:r><w:t><![CDATA[Literal <text> & symbols]]></w:t></w:r></w:p>");
        let archive = zip_fixture(
            &[
                ("_rels/.rels", rels.as_bytes()),
                ("parts/main document.xml", xml.as_bytes()),
            ],
            CompressionMethod::Stored,
        );
        assert_eq!(
            convert(&archive, DocumentKind::Docx).expect("alternate main part"),
            "Literal <text> & symbols"
        );
    }

    #[test]
    fn pptx_uses_declared_slide_order_and_keeps_paragraph_and_table_boundaries() {
        let presentation = presentation_xml(
            "<p:sldId id='258' r:id='third'/><p:sldId id='256' r:id='first'/><p:sldId id='257' r:id='second'/>",
        );
        let rels = slide_relationships(&format!(
            "<Relationship Id='first' Type='{OFFICE_REL_NS}/slide' Target='slides/slide1.xml'/><Relationship Id='second' Type='{OFFICE_REL_NS}/slide' Target='slides/slide2.xml'/><Relationship Id='third' Type='{OFFICE_REL_NS}/slide' Target='/ppt/slides/slide10.xml'/>"
        ));
        let slide1 = slide_xml(
            "<p:sp><p:txBody><a:p><a:r><a:t>One</a:t></a:r></a:p></p:txBody></p:sp>",
        );
        let slide2 = slide_xml(
            "<p:sp><p:txBody><a:p><a:r><a:t>Two</a:t></a:r><a:br/><a:r><a:t>next line</a:t></a:r></a:p></p:txBody></p:sp>",
        );
        let slide10 = slide_xml(
            "<p:sp><p:txBody><a:p><a:r><a:t>Ten &amp; 🎼</a:t></a:r></a:p><a:p><a:r><a:t>Paragraph two</a:t></a:r></a:p></p:txBody></p:sp><a:tbl><a:tr><a:tc><a:txBody><a:p><a:r><a:t>A</a:t></a:r></a:p></a:txBody></a:tc><a:tc><a:txBody><a:p><a:r><a:t>B</a:t></a:r></a:p></a:txBody></a:tc></a:tr></a:tbl>",
        );
        let orphan =
            slide_xml("<a:p><a:r><a:t>Orphan slide must not appear</a:t></a:r></a:p>");
        let archive = pptx_fixture(
            &presentation,
            &rels,
            &[
                ("ppt/slides/slide2.xml", slide2.as_bytes()),
                ("ppt/slides/slide1.xml", slide1.as_bytes()),
                ("ppt/slides/slide10.xml", slide10.as_bytes()),
                ("ppt/slides/slide99.xml", orphan.as_bytes()),
            ],
        );
        assert_eq!(
            convert(&archive, DocumentKind::Pptx).expect("PPTX text"),
            "## Slide 1\n\nTen & 🎼\n\nParagraph two\n\n| A | B |\n\n## Slide 2\n\nOne\n\n## Slide 3\n\nTwo\nnext line"
        );
    }

    #[test]
    fn malformed_xml_entities_and_encoding_are_rejected_without_partial_text() {
        let valid_prefix = format!(
            "<w:document xmlns:w='{WORD_NS}'><w:body><w:p><w:r><w:t>valid text"
        );
        let suffix = "</w:t></w:r></w:p></w:body></w:document>";
        for xml in [
            valid_prefix.clone(),
            format!("{valid_prefix}</w:p></w:t></w:r></w:body></w:document>"),
            format!("{valid_prefix}&custom;{suffix}"),
            format!("{valid_prefix}&#x1;{suffix}"),
            format!("{valid_prefix}&#xD800;{suffix}"),
            format!("{valid_prefix}&amp{suffix}"),
            format!("{valid_prefix}\u{0}{suffix}"),
            format!("{valid_prefix}]]>{suffix}"),
            format!("{valid_prefix}{suffix}<second/>"),
            format!("{valid_prefix}{suffix}trailing"),
            format!(
                "<!DOCTYPE w:document [<!ENTITY custom SYSTEM 'file:///etc/passwd'>]>{valid_prefix}&custom;{suffix}"
            ),
            format!(
                "<?xml version='1.0' encoding='ISO-8859-1'?>{valid_prefix}{suffix}"
            ),
            format!("<?xml version='1.0' version='1.1'?>{valid_prefix}{suffix}"),
            format!("<?XML version='1.0'?>{valid_prefix}{suffix}"),
            word_xml("<w:p><w:r><w:t bad:attribute='x'>text</w:t></w:r></w:p>"),
            word_xml("<w:p><w:r><w:t attribute='&custom;'>text</w:t></w:r></w:p>"),
            word_xml("<w:p><w:r><w:t attribute='<' >text</w:t></w:r></w:p>"),
            word_xml(
                "<w:p><w:r><w:t xmlns:x='urn:a' xmlns:y='urn:a' x:id='a' y:id='b'>text</w:t></w:r></w:p>",
            ),
        ] {
            assert!(
                convert(&docx_with_xml(xml.as_bytes()), DocumentKind::Docx).is_err(),
                "accepted hostile XML: {xml}"
            );
        }
        assert!(
            convert(
                &docx_with_xml(&[0xFF, 0xFE, b'<']),
                DocumentKind::Docx
            )
            .is_err()
        );
        assert!(convert(&docx_with_xml(&[0xFF]), DocumentKind::Docx).is_err());
    }

    #[test]
    fn rejects_missing_wrong_or_external_main_document_parts() {
        let xml = word_xml("<w:p><w:r><w:t>text</w:t></w:r></w:p>");
        let no_rels = zip_fixture(
            &[("word/document.xml", xml.as_bytes())],
            CompressionMethod::Stored,
        );
        assert!(convert(&no_rels, DocumentKind::Docx).is_err());
        for target in [
            "../../outside.xml",
            "file:///etc/passwd",
            "//example.test/document.xml",
            "%2e%2e/outside.xml",
            "word%2Fdocument.xml",
        ] {
            let rels = format!(
                "<Relationships xmlns='{RELATIONSHIPS_NS}'><Relationship Id='main' Type='{OFFICE_REL_NS}/officeDocument' Target='{target}'/></Relationships>"
            );
            let archive = zip_fixture(
                &[
                    ("_rels/.rels", rels.as_bytes()),
                    ("word/document.xml", xml.as_bytes()),
                ],
                CompressionMethod::Stored,
            );
            assert!(
                convert(&archive, DocumentKind::Docx).is_err(),
                "accepted target {target}"
            );
        }
        let external = format!(
            "<Relationships xmlns='{RELATIONSHIPS_NS}'><Relationship Id='main' Type='{OFFICE_REL_NS}/officeDocument' Target='https://example.test/document.xml' TargetMode='External'/></Relationships>"
        );
        let archive = zip_fixture(
            &[("_rels/.rels", external.as_bytes())],
            CompressionMethod::Stored,
        );
        assert!(convert(&archive, DocumentKind::Docx).is_err());
        assert!(convert(&docx_with_xml(xml.as_bytes()), DocumentKind::Pptx).is_err());
        assert!(
            convert(
                b"\xD0\xCF\x11\xE0encrypted Office container",
                DocumentKind::Docx
            )
            .is_err()
        );
    }

    #[test]
    fn pptx_rejects_missing_external_and_duplicate_slide_relationships() {
        let slide = slide_xml("<a:p><a:r><a:t>text</a:t></a:r></a:p>");
        let presentation = presentation_xml("<p:sldId id='256' r:id='one'/>");
        for rels in [
            slide_relationships(""),
            slide_relationships(&format!(
                "<Relationship Id='one' Type='{OFFICE_REL_NS}/slide' Target='slides/missing.xml'/>"
            )),
            slide_relationships(&format!(
                "<Relationship Id='one' Type='{OFFICE_REL_NS}/image' Target='slides/slide1.xml'/>"
            )),
            slide_relationships(&format!(
                "<Relationship Id='one' Type='{OFFICE_REL_NS}/slide' Target='https://example.test/slide.xml' TargetMode='External'/>"
            )),
            slide_relationships(&format!(
                "<Relationship Id='one' Type='{OFFICE_REL_NS}/slide' Target='slides/slide1.xml'/><Relationship Id='one' Type='{OFFICE_REL_NS}/slide' Target='slides/slide1.xml'/>"
            )),
        ] {
            let archive = pptx_fixture(
                &presentation,
                &rels,
                &[("ppt/slides/slide1.xml", slide.as_bytes())],
            );
            assert!(convert(&archive, DocumentKind::Pptx).is_err());
        }
        let rels = slide_relationships(&format!(
            "<Relationship Id='one' Type='{OFFICE_REL_NS}/slide' Target='slides/slide1.xml'/><Relationship Id='two' Type='{OFFICE_REL_NS}/slide' Target='slides/slide1.xml'/>"
        ));
        for ids in [
            "<p:sldId id='256' r:id='one'/><p:sldId id='257' r:id='one'/>",
            "<p:sldId id='256' r:id='one'/><p:sldId id='0256' r:id='two'/>",
            "<p:sldId id='256' r:id='one'/><p:sldId id='257' r:id='two'/>",
        ] {
            let archive = pptx_fixture(
                &presentation_xml(ids),
                &rels,
                &[("ppt/slides/slide1.xml", slide.as_bytes())],
            );
            assert!(convert(&archive, DocumentKind::Pptx).is_err());
        }
    }

    #[test]
    fn document_limits_reject_deep_xml_and_large_expansion() {
        let nested = format!(
            "{}text{}",
            "<w:r>".repeat(MAX_XML_DEPTH),
            "</w:r>".repeat(MAX_XML_DEPTH)
        );
        assert!(
            convert(
                &docx_with_xml(word_xml(&nested).as_bytes()),
                DocumentKind::Docx
            )
            .is_err()
        );
        let oversized = word_xml(&format!(
            "<w:p><w:r><w:t>{}</w:t></w:r></w:p>",
            "x".repeat(MAX_OUTPUT_BYTES + 1)
        ));
        let archive = docx_with_xml(oversized.as_bytes());
        assert!(archive.len() < MAX_DOCUMENT_BYTES);
        assert!(
            convert(&archive, DocumentKind::Docx)
                .expect_err("output bound")
                .to_string()
                .contains("8 MiB output")
        );
        let too_large_part =
            vec![b'x'; usize::try_from(MAX_PART_BYTES).expect("part limit") + 1];
        assert!(
            convert(&docx_with_xml(&too_large_part), DocumentKind::Docx).is_err()
        );
    }

    #[test]
    fn document_zip_validation_rejects_unsafe_members_and_truncated_packages() {
        let xml = word_xml("<w:p><w:r><w:t>text</w:t></w:r></w:p>");
        let unsafe_archive = zip_fixture(
            &[
                ("_rels/.rels", DOCX_RELS.as_bytes()),
                ("word/document.xml", xml.as_bytes()),
                ("../outside", b"payload"),
            ],
            CompressionMethod::Stored,
        );
        assert!(convert(&unsafe_archive, DocumentKind::Docx).is_err());
        let mut truncated = docx_with_xml(xml.as_bytes());
        truncated.truncate(truncated.len() - 12);
        assert!(convert(&truncated, DocumentKind::Docx).is_err());
        let empty = word_xml("");
        assert_eq!(
            convert(&docx_with_xml(empty.as_bytes()), DocumentKind::Docx)
                .expect("empty document"),
            ""
        );
    }
}

#[cfg(test)]
mod xml_name_limit_tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    #[test]
    fn rejects_namespace_amplification_and_oversized_xml_names() {
        let attributes = (0..120)
            .map(|index| format!("e:a{index}=\"value\""))
            .collect::<Vec<_>>()
            .join(" ");
        let oversized = "x".repeat(MAX_XML_NAME_BYTES + 1);
        let relationships = format!(
            r#"<Relationships xmlns="{RELATIONSHIPS_NS}"><Relationship Id="main" Type="{OFFICE_REL_NS}/officeDocument" Target="word/document.xml"/></Relationships>"#
        );
        for body in [
            format!(r#"<w:p {attributes} xmlns:e="{oversized}"/>"#),
            format!("<{oversized}/>"),
            format!(r#"<w:p {oversized}="value"/>"#),
        ] {
            let xml = format!(
                r#"<w:document xmlns:w="{WORD_NS}"><w:body>{body}</w:body></w:document>"#
            );
            let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
            let options = SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            for (name, contents) in [
                ("_rels/.rels", relationships.as_bytes()),
                ("word/document.xml", xml.as_bytes()),
            ] {
                writer.start_file(name, options).unwrap();
                writer.write_all(contents).unwrap();
            }
            let bytes = writer.finish().unwrap().into_inner();
            let error = convert(&bytes, DocumentKind::Docx).unwrap_err().to_string();
            assert!(error.contains("exceeds 4096 bytes"), "{error}");
        }
    }
}
