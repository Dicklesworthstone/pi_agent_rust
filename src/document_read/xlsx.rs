//! Excel workbook text extraction using the shared bounded OPC/XML reader.
//!
//! Cells keep their original row/column coordinates, even when the XML omits
//! empty cells. Only saved values are read; formulas and external links are
//! never evaluated. Wide or very sparse sheets use a cell/value table to avoid
//! materializing an enormous rectangular grid.

use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Cursor;

use super::{
    ArchiveReader, MAX_OUTPUT_BYTES, Properties, Relationship, Result, Tag, TextOutput, XmlBudget,
    XmlEvent, document_error, is_relationship, read_relationships, relationship_part, visit_xml,
};

const MAX_SHEETS: usize = 1024;
const MAX_SHARED_STRINGS: usize = 100_000;
const MAX_SHEET_ROWS: usize = 100_000;
const MAX_SHEET_CELLS: usize = 250_000;
const MAX_CELL_BYTES: usize = 256 * 1024;
const MAX_EXCEL_ROW: usize = 1_048_576;
const MAX_EXCEL_COLUMN: usize = 16_384;
const MAX_TABLE_COLUMNS: usize = 64;
const MAX_TABLE_CELLS: usize = 100_000;

struct Sheet {
    name: String,
    relationship_id: String,
    state: Option<String>,
}

pub(super) fn render(
    bytes: &[u8],
    main_part: &str,
    archive: &mut ArchiveReader<Cursor<&[u8]>>,
    budget: &mut XmlBudget,
    output: &mut TextOutput,
) -> Result<()> {
    let sheets = workbook_sheets(bytes)?;
    let relationships = read_relationships(&budget.read(archive, &relationship_part(main_part))?)?;
    let shared = read_shared_strings(archive, budget, &relationships, main_part)?;
    let mut visited = HashSet::new();
    let mut limits = SheetLimits::default();
    for sheet in sheets {
        let relationship = relationships
            .get(&sheet.relationship_id)
            .ok_or_else(|| document_error("worksheet relationship is missing"))?;
        let chart = is_relationship(&relationship.kind, "chartsheet");
        if !chart && !is_relationship(&relationship.kind, "worksheet") {
            return Err(document_error(
                "sheet ID refers to an unsupported sheet relationship",
            ));
        }
        let target = relationship.internal_target(main_part)?;
        if !visited.insert(target.clone()) {
            return Err(document_error("multiple sheets refer to the same part"));
        }
        let xml = budget.read(archive, &target)?;
        output.append("## ")?;
        output.content(&sheet.name, true)?;
        if let Some(state) = sheet.state {
            output.append(&format!(" ({state})"))?;
        }
        output.append("\n\n")?;
        if chart {
            visit_xml(&xml, Tag::ChartSheet, |_, _| Ok(()))?;
            output.append("[Chart sheet: no worksheet cell data]\n\n")?;
        } else {
            let rows = worksheet_rows(&xml, &shared, &mut limits)?;
            render_rows(&rows, output)?;
            output.separate("\n\n")?;
        }
    }
    Ok(())
}

fn workbook_sheets(bytes: &[u8]) -> Result<Vec<Sheet>> {
    let mut sheets = Vec::new();
    let mut ids = HashSet::new();
    let mut relationships = HashSet::new();
    let mut names = HashSet::new();
    let mut has_list = false;
    visit_xml(bytes, Tag::Workbook, |event, parents| {
        match event {
            XmlEvent::Start(Tag::WorkbookSheets, _) => {
                if parents != [Tag::Workbook] || has_list {
                    return Err(document_error("invalid or duplicate workbook sheet list"));
                }
                has_list = true;
            }
            XmlEvent::Start(Tag::WorkbookSheet, attributes) => {
                if parents != [Tag::Workbook, Tag::WorkbookSheets] || sheets.len() >= MAX_SHEETS {
                    return Err(document_error(
                        "invalid sheet list or more than 1024 sheets",
                    ));
                }
                let id = attributes
                    .sheet_id
                    .ok_or_else(|| document_error("sheet is missing its sheetId"))?
                    .parse::<u32>()
                    .map_err(document_error)?;
                let relationship_id = attributes
                    .relationship_id
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| document_error("sheet is missing its relationship ID"))?;
                let name = attributes
                    .name
                    .filter(|name| !name.is_empty())
                    .ok_or_else(|| document_error("sheet is missing its name"))?;
                if id == 0
                    || !ids.insert(id)
                    || !relationships.insert(relationship_id.clone())
                    || !names.insert(name.to_lowercase())
                {
                    return Err(document_error(
                        "invalid or duplicate workbook sheet identity",
                    ));
                }
                let state = match attributes.sheet_state.as_deref() {
                    None | Some("visible") => None,
                    Some("hidden" | "veryHidden") => attributes.sheet_state,
                    _ => return Err(document_error("invalid sheet visibility state")),
                };
                sheets.push(Sheet {
                    name,
                    relationship_id,
                    state,
                });
            }
            _ => {}
        }
        Ok(())
    })?;
    if !has_list || sheets.is_empty() {
        return Err(document_error("workbook sheet list is missing or empty"));
    }
    Ok(sheets)
}

fn read_shared_strings(
    archive: &mut ArchiveReader<Cursor<&[u8]>>,
    budget: &mut XmlBudget,
    relationships: &HashMap<String, Relationship>,
    main_part: &str,
) -> Result<Vec<String>> {
    let mut shared_part = None;
    for relationship in relationships.values() {
        if is_relationship(&relationship.kind, "sharedStrings")
            && shared_part
                .replace(relationship.internal_target(main_part)?)
                .is_some()
        {
            return Err(document_error("multiple shared-string relationships"));
        }
    }
    let Some(part) = shared_part else {
        return Ok(Vec::new());
    };
    let bytes = budget.read(archive, &part)?;
    shared_strings(&bytes)
}

fn shared_strings(bytes: &[u8]) -> Result<Vec<String>> {
    let mut strings = Vec::new();
    let mut current = None;
    let mut total_bytes: usize = 0;
    visit_xml(bytes, Tag::SharedStrings, |event, parents| {
        match event {
            XmlEvent::Start(Tag::SharedString, _) => {
                if parents != [Tag::SharedStrings] || strings.len() >= MAX_SHARED_STRINGS {
                    return Err(document_error(
                        "invalid shared-string list or more than 100000 strings",
                    ));
                }
                current = Some(String::new());
            }
            XmlEvent::Start(_, _) if parents.last() == Some(&Tag::SpreadsheetText) => {
                return Err(document_error("nested element in spreadsheet text"));
            }
            XmlEvent::Text(text)
                if rich_text_path(parents, &[Tag::SharedStrings, Tag::SharedString]) =>
            {
                total_bytes = total_bytes.saturating_add(text.len());
                if total_bytes > MAX_OUTPUT_BYTES {
                    return Err(document_error(
                        "shared strings exceed the 8 MiB text budget",
                    ));
                }
                append_cell_text(
                    current
                        .as_mut()
                        .ok_or_else(|| document_error("shared text outside a string item"))?,
                    text,
                )?;
            }
            XmlEvent::End(Tag::SharedString) => {
                strings.push(
                    current
                        .take()
                        .ok_or_else(|| document_error("unmatched shared-string item"))?,
                );
            }
            _ => {}
        }
        Ok(())
    })?;
    Ok(strings)
}

fn rich_text_path(parents: &[Tag], prefix: &[Tag]) -> bool {
    parents.strip_prefix(prefix).is_some_and(|path| {
        path == [Tag::SpreadsheetText] || path == [Tag::SpreadsheetRun, Tag::SpreadsheetText]
    })
}

fn append_cell_text(destination: &mut String, text: &str) -> Result<()> {
    if destination.len().saturating_add(text.len()) > MAX_CELL_BYTES {
        return Err(document_error("spreadsheet cell text exceeds 256 KiB"));
    }
    destination.push_str(text);
    Ok(())
}

#[derive(Default)]
struct SheetLimits {
    rows: usize,
    cells: usize,
}

struct Row {
    number: usize,
    cells: Vec<(usize, String)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CellKind {
    Number,
    Shared,
    String,
    Inline,
    Boolean,
    Error,
    Date,
}

struct Cell {
    column: usize,
    kind: CellKind,
    value: String,
    inline: String,
    has_value: bool,
    has_inline: bool,
    has_formula: bool,
}

impl Cell {
    fn new(attributes: Properties, row: usize, previous_column: usize) -> Result<Self> {
        let column = if let Some(reference) = attributes.reference {
            let (column, cell_row) = cell_reference(&reference)?;
            if cell_row != row {
                return Err(document_error(
                    "cell reference does not match its containing row",
                ));
            }
            column
        } else {
            previous_column + 1
        };
        if column <= previous_column || column > MAX_EXCEL_COLUMN {
            return Err(document_error(
                "duplicate, out-of-order, or out-of-range worksheet cell",
            ));
        }
        let kind = match attributes.cell_type.as_deref() {
            None | Some("n") => CellKind::Number,
            Some("s") => CellKind::Shared,
            Some("str") => CellKind::String,
            Some("inlineStr") => CellKind::Inline,
            Some("b") => CellKind::Boolean,
            Some("e") => CellKind::Error,
            Some("d") => CellKind::Date,
            _ => return Err(document_error("unsupported spreadsheet cell type")),
        };
        Ok(Self {
            column,
            kind,
            value: String::new(),
            inline: String::new(),
            has_value: false,
            has_inline: false,
            has_formula: false,
        })
    }

    fn start_content(&mut self, tag: Tag) -> Result<()> {
        let seen = match tag {
            Tag::CellValue => &mut self.has_value,
            Tag::CellInlineString => &mut self.has_inline,
            Tag::CellFormula => &mut self.has_formula,
            _ => return Err(document_error("invalid spreadsheet cell content")),
        };
        if std::mem::replace(seen, true) {
            return Err(document_error(
                "duplicate spreadsheet cell value or formula",
            ));
        }
        Ok(())
    }

    fn saved_value<'a>(&'a self, shared: &'a [String]) -> Result<Cow<'a, str>> {
        if self.kind == CellKind::Inline {
            if self.has_value || self.has_formula {
                return Err(document_error(
                    "inline string has a scalar value or formula",
                ));
            }
            return Ok(Cow::Borrowed(&self.inline));
        }
        if self.has_inline {
            return Err(document_error("non-inline cell contains an inline string"));
        }
        if self.kind == CellKind::String && self.has_value {
            return Ok(Cow::Borrowed(&self.value));
        }
        let value = self.value.trim();
        if !self.has_value || value.is_empty() {
            return Ok(Cow::Borrowed(if self.has_formula {
                "[Formula result unavailable: no cached value]"
            } else {
                ""
            }));
        }
        let value = match self.kind {
            CellKind::Shared => {
                let index = value.parse::<usize>().map_err(document_error)?;
                shared
                    .get(index)
                    .ok_or_else(|| document_error("shared-string index is out of range"))?
                    .as_str()
            }
            CellKind::Boolean => match value {
                "1" | "true" => "TRUE",
                "0" | "false" => "FALSE",
                _ => return Err(document_error("invalid cached spreadsheet boolean")),
            },
            CellKind::Number => {
                if !value.parse::<f64>().map_err(document_error)?.is_finite() {
                    return Err(document_error("non-finite spreadsheet numeric value"));
                }
                value
            }
            CellKind::String | CellKind::Error | CellKind::Date => value,
            CellKind::Inline => {
                return Err(document_error("invalid inline spreadsheet cell state"));
            }
        };
        Ok(Cow::Borrowed(value))
    }
}

fn cell_reference(reference: &str) -> Result<(usize, usize)> {
    let letters = reference
        .bytes()
        .take_while(u8::is_ascii_alphabetic)
        .count();
    if letters == 0 || letters > 3 || letters == reference.len() {
        return Err(document_error("invalid spreadsheet cell reference"));
    }
    let column = reference.as_bytes()[..letters]
        .iter()
        .fold(0_usize, |column, byte| {
            column * 26 + usize::from(byte.to_ascii_uppercase() - b'A' + 1)
        });
    let row_text = &reference[letters..];
    if row_text.starts_with('0') || !row_text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(document_error("invalid spreadsheet cell row reference"));
    }
    let row = row_text.parse::<usize>().map_err(document_error)?;
    if column > MAX_EXCEL_COLUMN || !(1..=MAX_EXCEL_ROW).contains(&row) {
        return Err(document_error(
            "spreadsheet cell reference exceeds Excel worksheet bounds",
        ));
    }
    Ok((column, row))
}

struct Worksheet<'a> {
    shared: &'a [String],
    limits: &'a mut SheetLimits,
    rows: Vec<Row>,
    row: Option<Row>,
    cell: Option<Cell>,
    previous_row: usize,
    previous_column: usize,
    text_bytes: usize,
    has_data: bool,
}

impl Worksheet<'_> {
    fn start(&mut self, tag: Tag, attributes: Properties, parents: &[Tag]) -> Result<()> {
        if matches!(
            parents.last(),
            Some(Tag::CellValue | Tag::CellFormula | Tag::SpreadsheetText)
        ) {
            return Err(document_error(
                "nested element in spreadsheet scalar content",
            ));
        }
        match tag {
            Tag::SheetData => {
                if parents != [Tag::Worksheet] || self.has_data {
                    return Err(document_error("invalid or duplicate worksheet cell data"));
                }
                self.has_data = true;
            }
            Tag::SheetRow => {
                if parents != [Tag::Worksheet, Tag::SheetData] {
                    return Err(document_error("worksheet row outside sheet data"));
                }
                self.start_row(attributes)?;
            }
            Tag::SheetCell => {
                if parents != [Tag::Worksheet, Tag::SheetData, Tag::SheetRow] {
                    return Err(document_error("worksheet cell outside a row"));
                }
                let row = self
                    .row
                    .as_ref()
                    .ok_or_else(|| document_error("worksheet cell is missing its row"))?;
                self.limits.cells += 1;
                if self.limits.cells > MAX_SHEET_CELLS {
                    return Err(document_error("workbook exceeds 250000 stored cells"));
                }
                let cell = Cell::new(attributes, row.number, self.previous_column)?;
                self.previous_column = cell.column;
                self.cell = Some(cell);
            }
            Tag::CellValue | Tag::CellFormula | Tag::CellInlineString => {
                if parents.last() != Some(&Tag::SheetCell) {
                    return Err(document_error("spreadsheet value outside a cell"));
                }
                self.cell
                    .as_mut()
                    .ok_or_else(|| document_error("spreadsheet value is missing its cell"))?
                    .start_content(tag)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn start_row(&mut self, attributes: Properties) -> Result<()> {
        self.limits.rows += 1;
        if self.limits.rows > MAX_SHEET_ROWS {
            return Err(document_error("workbook exceeds 100000 stored rows"));
        }
        let number = attributes.reference.map_or_else(
            || Ok(self.previous_row + 1),
            |reference| reference.parse::<usize>().map_err(document_error),
        )?;
        if number <= self.previous_row || number > MAX_EXCEL_ROW {
            return Err(document_error(
                "duplicate, out-of-order, or out-of-range worksheet row",
            ));
        }
        self.previous_column = 0;
        self.row = Some(Row {
            number,
            cells: Vec::new(),
        });
        Ok(())
    }

    fn text(&mut self, text: &str, parents: &[Tag]) -> Result<()> {
        if parents.last() == Some(&Tag::CellValue) {
            append_cell_text(
                &mut self
                    .cell
                    .as_mut()
                    .ok_or_else(|| document_error("cached value outside a cell"))?
                    .value,
                text,
            )?;
        } else if rich_text_path(
            parents,
            &[
                Tag::Worksheet,
                Tag::SheetData,
                Tag::SheetRow,
                Tag::SheetCell,
                Tag::CellInlineString,
            ],
        ) {
            append_cell_text(
                &mut self
                    .cell
                    .as_mut()
                    .ok_or_else(|| document_error("inline text outside a cell"))?
                    .inline,
                text,
            )?;
        }
        Ok(())
    }

    fn end(&mut self, tag: Tag) -> Result<()> {
        match tag {
            Tag::SheetCell => {
                let cell = self
                    .cell
                    .take()
                    .ok_or_else(|| document_error("unmatched worksheet cell"))?;
                let value = cell.saved_value(self.shared)?;
                self.text_bytes = self.text_bytes.saturating_add(value.len());
                if self.text_bytes > MAX_OUTPUT_BYTES {
                    return Err(document_error("expanded worksheet values exceed 8 MiB"));
                }
                if !value.is_empty() {
                    self.row
                        .as_mut()
                        .ok_or_else(|| document_error("worksheet cell is missing its row"))?
                        .cells
                        .push((cell.column, value.into_owned()));
                }
            }
            Tag::SheetRow => {
                let row = self
                    .row
                    .take()
                    .ok_or_else(|| document_error("unmatched worksheet row"))?;
                self.previous_row = row.number;
                if !row.cells.is_empty() {
                    self.rows.push(row);
                }
            }
            _ => {}
        }
        Ok(())
    }
}

fn worksheet_rows(bytes: &[u8], shared: &[String], limits: &mut SheetLimits) -> Result<Vec<Row>> {
    let mut worksheet = Worksheet {
        shared,
        limits,
        rows: Vec::new(),
        row: None,
        cell: None,
        previous_row: 0,
        previous_column: 0,
        text_bytes: 0,
        has_data: false,
    };
    visit_xml(bytes, Tag::Worksheet, |event, parents| match event {
        XmlEvent::Start(tag, attributes) => worksheet.start(tag, attributes, parents),
        XmlEvent::Text(text) => worksheet.text(text, parents),
        XmlEvent::End(tag) => worksheet.end(tag),
    })?;
    if !worksheet.has_data {
        return Err(document_error("worksheet is missing sheetData"));
    }
    Ok(worksheet.rows)
}

fn column_name(mut column: usize) -> String {
    let mut name = Vec::new();
    while column > 0 {
        column -= 1;
        name.push(char::from(b"ABCDEFGHIJKLMNOPQRSTUVWXYZ"[column % 26]));
        column /= 26;
    }
    name.into_iter().rev().collect()
}

fn render_rows(rows: &[Row], output: &mut TextOutput) -> Result<()> {
    if rows.is_empty() {
        return output.append("[No populated cells]\n");
    }
    let columns: BTreeSet<_> = rows
        .iter()
        .flat_map(|row| row.cells.iter().map(|(column, _)| *column))
        .collect();
    if columns.len() > MAX_TABLE_COLUMNS
        || columns.len().saturating_mul(rows.len()) > MAX_TABLE_CELLS
    {
        output.append("| Cell | Value |\n| --- | --- |\n")?;
        for row in rows {
            for (column, value) in &row.cells {
                output.append(&format!("| {}{} | ", column_name(*column), row.number))?;
                output.content(value, true)?;
                output.append(" |\n")?;
            }
        }
        return Ok(());
    }
    output.append("| Row |")?;
    for column in &columns {
        output.append(&format!(" {} |", column_name(*column)))?;
    }
    output.append("\n| --- |")?;
    for _ in &columns {
        output.append(" --- |")?;
    }
    output.append("\n")?;
    for row in rows {
        output.append(&format!("| {} |", row.number))?;
        let mut cells = row.cells.iter().peekable();
        for column in &columns {
            output.append(" ")?;
            if cells
                .peek()
                .is_some_and(|(cell_column, _)| cell_column == column)
                && let Some((_, value)) = cells.next()
            {
                output.content(value, true)?;
            }
            output.append(" |")?;
        }
        output.append("\n")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::{DocumentKind, OFFICE_REL_NS, RELATIONSHIPS_NS, SPREADSHEET_NS, convert};
    use super::*;
    use std::fmt::Write as _;
    use std::io::Write as _;
    use zip::CompressionMethod;
    use zip::write::SimpleFileOptions;

    fn xml(root: &str, body: &str) -> String {
        format!("<{root} xmlns=\"{SPREADSHEET_NS}\" xmlns:r=\"{OFFICE_REL_NS}\">{body}</{root}>")
    }

    fn relationships(body: &str) -> String {
        format!("<Relationships xmlns=\"{RELATIONSHIPS_NS}\">{body}</Relationships>")
    }

    fn relationship(id: &str, kind: &str, target: &str) -> String {
        format!("<Relationship Id=\"{id}\" Type=\"{OFFICE_REL_NS}/{kind}\" Target=\"{target}\"/>")
    }

    fn package(workbook: &str, rels: &str, parts: &[(&str, &[u8])]) -> Vec<u8> {
        let package_rels =
            relationships(&relationship("main", "officeDocument", "xl/workbook.xml"));
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        let mut entries = vec![
            ("_rels/.rels", package_rels.as_bytes()),
            ("xl/workbook.xml", workbook.as_bytes()),
            ("xl/_rels/workbook.xml.rels", rels.as_bytes()),
        ];
        entries.extend_from_slice(parts);
        for (name, contents) in entries {
            writer
                .start_file(name, options)
                .expect("start XLSX fixture member");
            writer
                .write_all(contents)
                .expect("write XLSX fixture member");
        }
        writer.finish().expect("finish XLSX fixture").into_inner()
    }

    fn workbook_with_sheet(sheet: &str, shared: Option<&str>) -> Vec<u8> {
        let workbook = xml(
            "workbook",
            "<sheets><sheet name=\"Data\" sheetId=\"1\" r:id=\"sheet\"/></sheets>",
        );
        let mut rels = relationship("sheet", "worksheet", "worksheets/data.xml");
        let mut parts = vec![("xl/worksheets/data.xml", sheet.as_bytes())];
        if let Some(shared) = shared {
            rels.push_str(&relationship("strings", "sharedStrings", "strings.xml"));
            parts.push(("xl/strings.xml", shared.as_bytes()));
        }
        package(&workbook, &relationships(&rels), &parts)
    }

    fn sheet(rows: &str) -> String {
        xml("worksheet", &format!("<sheetData>{rows}</sheetData>"))
    }

    #[test]
    fn workbook_order_relationship_targets_and_sparse_cells_preserve_meaning() {
        let workbook = xml(
            "workbook",
            "<sheets><sheet name=\"West &amp; Co\" sheetId=\"8\" r:id=\"west\"/><sheet name=\"East\" sheetId=\"2\" r:id=\"east\" state=\"hidden\"/></sheets>",
        );
        let rels = relationships(&format!(
            "{}{}{}",
            relationship("east", "worksheet", "worksheets/sheet1.xml"),
            relationship("strings", "sharedStrings", "../text/shared.xml"),
            relationship("west", "worksheet", "../tables/sheet9.xml"),
        ));
        let shared = xml(
            "sst",
            "<si><t xml:space=\"preserve\"> 001 </t></si><si><r><t>Revenue</t></r><r><t> &amp; cost</t></r><rPh sb=\"0\" eb=\"1\"><t>phonetic only</t></rPh></si>",
        );
        let west = sheet(
            "<row r=\"3\"><c r=\"A3\" t=\"s\"><v>0</v></c><c r=\"C3\" t=\"s\"><v>1</v></c></row><row r=\"9\"><c r=\"C9\"><v>9007199254740993</v></c></row>",
        );
        let east = sheet("<row r=\"1\"><c r=\"B1\" t=\"b\"><v>1</v></c></row>");
        let bytes = package(
            &workbook,
            &rels,
            &[
                ("xl/worksheets/sheet1.xml", east.as_bytes()),
                ("tables/sheet9.xml", west.as_bytes()),
                ("text/shared.xml", shared.as_bytes()),
            ],
        );
        let result = convert(&bytes, DocumentKind::Xlsx).expect("convert XLSX workbook");
        assert_eq!(
            result,
            "## West & Co\n\n| Row | A | C |\n| --- | --- | --- |\n| 3 |  001  | Revenue & cost |\n| 9 |  | 9007199254740993 |\n\n## East (hidden)\n\n| Row | B |\n| --- | --- |\n| 1 | TRUE |"
        );
        assert!(!result.contains("phonetic"));
    }

    #[test]
    fn inline_strings_cached_formulas_dates_errors_and_booleans_are_saved_values() {
        let worksheet = sheet(
            "<row r=\"1\"><c r=\"A1\" t=\"inlineStr\"><is><r><t>foo|</t></r><r><t>bar&#10;baz</t></r></is></c><c r=\"B1\"><f>SUM(A2:A3)</f><v>1.25E+2</v></c><c r=\"C1\" t=\"b\"><v>false</v></c><c r=\"D1\" t=\"e\"><v>#DIV/0!</v></c><c r=\"E1\" t=\"d\"><v>2026-10-19T12:34:56Z</v></c><c r=\"F1\" t=\"str\"><f>\"0007\"</f><v>0007</v></c><c r=\"G1\"><f>HYPERLINK(\"https://example.invalid/never-fetch\",\"x\")</f><v/></c></row>",
        );
        let result = convert(&workbook_with_sheet(&worksheet, None), DocumentKind::Xlsx)
            .expect("convert cached values");
        assert!(result.contains(
            "| 1 | foo\\|bar<br>baz | 1.25E+2 | FALSE | #DIV/0! | 2026-10-19T12:34:56Z | 0007 | [Formula result unavailable: no cached value] |"
        ));
        assert!(!result.contains("HYPERLINK"));
        assert!(!result.contains("never-fetch"));
    }

    #[test]
    fn omitted_references_infer_sequential_positions_and_empty_cells_do_not_shift_values() {
        let worksheet = sheet(
            "<row><c t=\"inlineStr\"><is><t>first</t></is></c><c/><c t=\"str\"><v>third</v></c></row><row><c/><c><v>4</v></c></row>",
        );
        let result = convert(&workbook_with_sheet(&worksheet, None), DocumentKind::Xlsx)
            .expect("convert inferred positions");
        assert!(result.contains("| Row | A | B | C |"));
        assert!(result.contains("| 1 | first |  | third |"));
        assert!(result.contains("| 2 |  | 4 |  |"));
    }

    #[test]
    fn extreme_sparse_coordinates_do_not_expand_missing_rows_or_columns() {
        let worksheet = sheet(
            "<row r=\"1048576\"><c r=\"XFD1048576\" t=\"inlineStr\"><is><t>last cell</t></is></c></row>",
        );
        let result = convert(&workbook_with_sheet(&worksheet, None), DocumentKind::Xlsx)
            .expect("convert sparse corner");
        assert_eq!(
            result,
            "## Data\n\n| Row | XFD |\n| --- | --- |\n| 1048576 | last cell |"
        );
    }

    #[test]
    fn wide_sheets_use_cell_coordinates_without_rectangular_padding() {
        let cells = (1..=65).fold(String::new(), |mut cells, column| {
            let _ = write!(
                cells,
                "<c r=\"{}8\"><v>{column}</v></c>",
                column_name(column)
            );
            cells
        });
        let worksheet = sheet(&format!("<row r=\"8\">{cells}</row>"));
        let result = convert(&workbook_with_sheet(&worksheet, None), DocumentKind::Xlsx)
            .expect("convert wide sheet");
        assert!(result.starts_with("## Data\n\n| Cell | Value |\n| --- | --- |\n"));
        assert!(result.contains("| A8 | 1 |\n"));
        assert!(result.ends_with("| BM8 | 65 |"));
        assert_eq!(result.lines().count(), 69);
    }

    #[test]
    fn invalid_cell_positions_values_and_duplicate_content_fail_closed() {
        for rows in [
            "<row r=\"1\"><c r=\"A2\"><v>1</v></c></row>",
            "<row r=\"1\"><c r=\"A1\"/><c r=\"A1\"/></row>",
            "<row r=\"1\"><c r=\"C1\"/><c r=\"B1\"/></row>",
            "<row r=\"2\"/><row r=\"1\"/>",
            "<row r=\"0\"/>",
            "<row r=\"1048577\"/>",
            "<row r=\"1\"><c r=\"XFE1\"/></row>",
            "<row r=\"1\"><c r=\"A0\"/></row>",
            "<row r=\"1\"><c r=\"A01\"/></row>",
            "<row r=\"1\"><c r=\"A1\" t=\"s\"><v>99</v></c></row>",
            "<row r=\"1\"><c r=\"A1\" t=\"b\"><v>maybe</v></c></row>",
            "<row r=\"1\"><c r=\"A1\"><v>NaN</v></c></row>",
            "<row r=\"1\"><c r=\"A1\"><v>12</v><v>13</v></c></row>",
            "<row r=\"1\"><c r=\"A1\"><f>1</f><f>2</f><v>1</v></c></row>",
            "<row r=\"1\"><c r=\"A1\" t=\"inlineStr\"><is><t>x</t></is><v>1</v></c></row>",
            "<row r=\"1\"><c r=\"A1\"><v><x>1</x></v></c></row>",
        ] {
            let bytes = workbook_with_sheet(&sheet(rows), None);
            assert!(convert(&bytes, DocumentKind::Xlsx).is_err(), "{rows}");
        }
    }

    #[test]
    fn ambiguous_workbook_identity_and_external_sheet_targets_are_rejected() {
        let worksheet = sheet("");
        let rels = relationships(&relationship("sheet", "worksheet", "worksheets/data.xml"));
        for sheets in [
            "<sheet name=\"A\" sheetId=\"1\" r:id=\"sheet\"/><sheet name=\"B\" sheetId=\"1\" r:id=\"other\"/>",
            "<sheet name=\"A\" sheetId=\"1\" r:id=\"sheet\"/><sheet name=\"a\" sheetId=\"2\" r:id=\"other\"/>",
            "<sheet name=\"A\" sheetId=\"1\" r:id=\"sheet\"/><sheet name=\"B\" sheetId=\"2\" r:id=\"sheet\"/>",
            "<sheet name=\"A\" sheetId=\"0\" r:id=\"sheet\"/>",
            "<sheet name=\"A\" sheetId=\"1\" r:id=\"missing\"/>",
        ] {
            let workbook = xml("workbook", &format!("<sheets>{sheets}</sheets>"));
            let bytes = package(
                &workbook,
                &rels,
                &[("xl/worksheets/data.xml", worksheet.as_bytes())],
            );
            assert!(convert(&bytes, DocumentKind::Xlsx).is_err(), "{sheets}");
        }
        let workbook = xml(
            "workbook",
            "<sheets><sheet name=\"A\" sheetId=\"1\" r:id=\"sheet\"/></sheets>",
        );
        let external = relationships(&format!(
            "<Relationship Id=\"sheet\" Type=\"{OFFICE_REL_NS}/worksheet\" Target=\"https://example.invalid/sheet.xml\" TargetMode=\"External\"/>"
        ));
        assert!(convert(&package(&workbook, &external, &[]), DocumentKind::Xlsx).is_err());
    }

    #[test]
    fn shared_string_expansion_and_individual_cell_text_are_bounded() {
        let shared = xml("sst", &format!("<si><t>{}</t></si>", "x".repeat(32_000)));
        let rows = (1..=270).fold(String::new(), |mut rows, row| {
            let _ = write!(
                rows,
                "<row r=\"{row}\"><c r=\"A{row}\" t=\"s\"><v>0</v></c></row>"
            );
            rows
        });
        let bytes = workbook_with_sheet(&sheet(&rows), Some(&shared));
        let error = convert(&bytes, DocumentKind::Xlsx).expect_err("bound shared-string expansion");
        assert!(
            error
                .to_string()
                .contains("expanded worksheet values exceed 8 MiB")
        );

        let worksheet = sheet(&format!(
            "<row><c t=\"inlineStr\"><is><t>{}</t></is></c></row>",
            "x".repeat(MAX_CELL_BYTES + 1)
        ));
        let error = convert(&workbook_with_sheet(&worksheet, None), DocumentKind::Xlsx)
            .expect_err("bound individual cell text");
        assert!(error.to_string().contains("cell text exceeds 256 KiB"));
    }

    #[test]
    fn hidden_empty_sheets_and_chart_sheets_remain_visible() {
        let workbook = xml(
            "workbook",
            "<sheets><sheet name=\"Empty\" sheetId=\"1\" r:id=\"empty\" state=\"veryHidden\"/><sheet name=\"Chart\" sheetId=\"2\" r:id=\"chart\"/></sheets>",
        );
        let rels = relationships(&format!(
            "{}{}",
            relationship("empty", "worksheet", "worksheets/empty.xml"),
            relationship("chart", "chartsheet", "chartsheets/chart.xml"),
        ));
        let empty = sheet("");
        let chart = xml("chartsheet", "");
        let result = convert(
            &package(
                &workbook,
                &rels,
                &[
                    ("xl/worksheets/empty.xml", empty.as_bytes()),
                    ("xl/chartsheets/chart.xml", chart.as_bytes()),
                ],
            ),
            DocumentKind::Xlsx,
        )
        .expect("convert empty and chart sheets");
        assert_eq!(
            result,
            "## Empty (veryHidden)\n\n[No populated cells]\n\n## Chart\n\n[Chart sheet: no worksheet cell data]"
        );
    }

    #[test]
    fn strict_namespace_and_utf16_worksheet_text_are_supported() {
        let worksheet =
            sheet("<row r=\"1\"><c r=\"A1\" t=\"inlineStr\"><is><t>東京 Δ</t></is></c></row>")
                .replace(
                    SPREADSHEET_NS,
                    "http://purl.oclc.org/ooxml/spreadsheetml/main",
                );
        let mut utf16 = vec![0xFF, 0xFE];
        for unit in worksheet.encode_utf16() {
            utf16.extend_from_slice(&unit.to_le_bytes());
        }
        let workbook = xml(
            "workbook",
            "<sheets><sheet name=\"Unicode\" sheetId=\"1\" r:id=\"sheet\"/></sheets>",
        );
        let rels = relationships(&relationship("sheet", "worksheet", "worksheets/data.xml"));
        let result = convert(
            &package(&workbook, &rels, &[("xl/worksheets/data.xml", &utf16)]),
            DocumentKind::Xlsx,
        )
        .expect("convert UTF-16 strict worksheet");
        assert!(result.ends_with("| 1 | 東京 Δ |"));
    }
}
