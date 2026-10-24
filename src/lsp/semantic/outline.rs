//! Complete, source-checked document outlines, including streamed hierarchies.
//!
//! Validate the whole response before selecting a bounded display prefix. Flat
//! SymbolInformation is not a hierarchy: containerName never invents parents.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::{Value, json};

use super::{Budget, LspInput, LspTool, MAX_RESPONSE_BYTES, Source, bounded_size, malformed, uint};
use crate::error::Result;
use crate::lsp::client::uri_to_path;
use crate::lsp::text::{Position, Range, position_to_offset_exact};
use crate::lsp::{MAX_PAYLOAD_BYTES, display_path, text_output, tool_err};
use crate::tools::ToolOutput;

mod source_text;

const MAX_SYMBOLS: usize = 8192;
const MAX_DEPTH: usize = 32;
const MAX_DISPLAY: usize = 1000;

#[derive(Debug)]
struct Symbol<'a> {
    value: &'a Value,
    range: Range,
    selection: Option<Range>,
    parent: Option<usize>,
    depth: usize,
}

#[derive(Debug)]
struct Outline<'a> {
    symbols: Vec<Symbol<'a>>,
    shape: &'static str,
    offsets: BTreeMap<Position, usize>,
}

/// Name matching addresses semantic definitions, never arbitrary source hits.
/// A line narrows definitions by their full half-open range, not their name.
#[derive(Debug, Default)]
struct Filter {
    name: Option<String>,
    occurrence: Option<usize>,
    line: Option<u32>, // zero-based after validating the one-based input
}

impl Filter {
    fn parse(input: &LspInput) -> Result<Self> {
        if input.position.is_some()
            || input.range.is_some()
            || input.resolve.is_some()
            || input.query.is_some()
            || input.apply.is_some()
            || input.action_id.is_some()
            || input.refactor_id.is_some()
            || input.completion_id.is_some()
            || input.snippet_values.is_some()
            || input.hierarchy_id.is_some()
            || input.new_name.is_some()
            || input.new_file.is_some()
            || input.format_options.is_some()
            || input.method.is_some()
            || input.payload.is_some()
            || input.only.is_some()
            || input.after.is_some()
            || input.limit == Some(0)
            || input
                .file
                .as_ref()
                .is_none_or(|file| file.is_empty() || file.len() > 8192)
        {
            return Err(tool_err(
                "LSP_USAGE",
                "document symbols accept file, symbol, line, limit and timeout only",
            ));
        }
        let (name, occurrence) = match input.symbol.as_deref() {
            None => (None, None),
            Some(raw) if !raw.trim().is_empty() && raw.len() <= 1024 => {
                let (name, occurrence) = crate::lsp::parse_symbol_selector(raw);
                if name.trim().is_empty() {
                    return Err(tool_err(
                        "LSP_USAGE",
                        "document symbol name must not be empty",
                    ));
                }
                (Some(name), occurrence)
            }
            Some(_) => {
                return Err(tool_err(
                    "LSP_USAGE",
                    "document symbol must be a nonempty string within 1024 bytes",
                ));
            }
        };
        let line = input
            .line
            .map(|line| {
                line.checked_sub(1)
                    .ok_or_else(|| tool_err("LSP_USAGE", "document symbol line must be one-based"))
            })
            .transpose()?;
        Ok(Self {
            name,
            occurrence,
            line,
        })
    }

    fn validate_source(&self, text: &str) -> Result<()> {
        if self.line.is_some_and(|line| {
            position_to_offset_exact(text, Position { line, character: 0 }).is_none()
        }) {
            return Err(tool_err(
                "LSP_USAGE",
                "document symbol line is outside the source",
            ));
        }
        Ok(())
    }

    fn includes(&self, symbol: &Symbol<'_>) -> bool {
        self.name.as_ref().is_none_or(|name| {
            symbol.value["name"]
                .as_str()
                .is_some_and(|value| value.contains(name.as_str()))
        }) && self
            .line
            .is_none_or(|line| intersects_line(symbol.range, line))
    }
}

fn intersects_line(range: Range, line: u32) -> bool {
    range.start.line <= line
        && (range.end.line > line
            || (range.end.line == line && (range.end.character > 0 || range.start == range.end)))
}

struct Matches {
    indices: Vec<usize>,
    matched: usize,
}

fn select(outline: &Outline<'_>, filter: &Filter, budget: &Budget) -> Result<Matches> {
    let mut indices = Vec::new();
    for (index, symbol) in outline.symbols.iter().enumerate() {
        budget.remaining()?;
        if filter.includes(symbol) {
            indices.push(index);
        }
    }
    let matched = indices.len();
    if let Some(nth) = filter.occurrence {
        let index = nth.checked_sub(1).and_then(|index| indices.get(index)).copied()
            .ok_or_else(|| tool_err("LSP_SYMBOL_AMBIGUOUS", format!(
                "document symbol occurrence #{nth} is outside {matched} matching definitions; inspect the unnumbered search"
            )))?;
        indices = vec![index];
    }
    budget.remaining()?;
    Ok(Matches { indices, matched })
}

fn position(value: &Value) -> Result<Position> {
    let coordinate = |key: &str| -> Result<u32> {
        u32::try_from(uint(&value[key])?)
            .map_err(|_| malformed("document symbol coordinate exceeds the LSP integer range"))
    };
    Ok(Position {
        line: coordinate("line")?,
        character: coordinate("character")?,
    })
}

fn range(value: &Value, boundaries: &mut BTreeSet<Position>) -> Result<Range> {
    let range = Range {
        start: position(&value["start"])?,
        end: position(&value["end"])?,
    };
    if range.end < range.start {
        return Err(malformed("document symbol range is reversed"));
    }
    boundaries.extend([range.start, range.end]);
    Ok(range)
}

fn optional_text(value: &Value, key: &str, limit: usize) -> Result<()> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(text)) if text.len() <= limit => Ok(()),
        _ => Err(malformed(
            "document symbol metadata must be a bounded string",
        )),
    }
}

fn metadata(value: &Value) -> Result<()> {
    if !value["name"]
        .as_str()
        .is_some_and(|name| !name.trim().is_empty() && name.len() <= 4096)
        || !(1..=26).contains(&uint(&value["kind"])?)
    {
        return Err(malformed(
            "document symbol needs a nonempty name and valid kind",
        ));
    }
    optional_text(value, "detail", 16 * 1024)?;
    optional_text(value, "containerName", 4096)?;
    if value
        .get("deprecated")
        .is_some_and(|value| !value.is_null() && !value.is_boolean())
    {
        return Err(malformed("document symbol deprecated must be boolean"));
    }
    match value.get("tags") {
        None | Some(Value::Null) => {}
        Some(Value::Array(tags)) if tags.len() <= 32 => {
            for tag in tags {
                if uint(tag)? == 0 {
                    return Err(malformed("document symbol tag must be positive"));
                }
            }
        }
        Some(_) => {
            return Err(malformed(
                "document symbol tags must be a bounded integer array",
            ));
        }
    }
    Ok(())
}

/// Check all distinct endpoints in one source traversal rather than rescanning
/// a multi-megabyte file for every symbol. CR, LF and CRLF each count as one
/// line break; surrogate interiors and past-end columns are never rounded.
fn validate_boundaries(
    text: &str,
    boundaries: &BTreeSet<Position>,
    budget: &Budget,
) -> Result<BTreeMap<Position, usize>> {
    budget.remaining()?;
    let mut offsets = BTreeMap::new();
    let mut wanted = boundaries.iter().copied().peekable();
    let mut chars = text.char_indices().peekable();
    let mut current = Position {
        line: 0,
        character: 0,
    };
    let mut checkpoint = 0;
    loop {
        if wanted.peek() == Some(&current) {
            let offset = chars.peek().map_or(text.len(), |(offset, _)| *offset);
            offsets.insert(current, offset);
            wanted.next();
        }
        match wanted.peek() {
            None => return budget.remaining().map(|_| offsets),
            Some(next) if *next < current => {
                return Err(malformed(
                    "document symbol endpoint is not an exact source boundary",
                ));
            }
            Some(_) => {}
        }
        let Some((offset, ch)) = chars.next() else {
            return Err(malformed("document symbol endpoint is outside the source"));
        };
        if offset.saturating_sub(checkpoint) >= 4096 {
            budget.remaining()?;
            checkpoint = offset;
        }
        if matches!(ch, '\r' | '\n') {
            if ch == '\r' && chars.peek().is_some_and(|(_, ch)| *ch == '\n') {
                chars.next();
            }
            current.line = current
                .line
                .checked_add(1)
                .ok_or_else(|| malformed("document symbol line count overflow"))?;
            current.character = 0;
        } else {
            current.character = current
                .character
                .checked_add(u32::try_from(ch.len_utf16()).unwrap_or(2))
                .ok_or_else(|| malformed("document symbol column count overflow"))?;
        }
    }
}

#[allow(clippy::too_many_lines)]
fn parse<'a>(raw: &'a Value, path: &Path, text: &str, budget: &Budget) -> Result<Outline<'a>> {
    budget.remaining()?;
    bounded_size(raw, MAX_RESPONSE_BYTES)?;
    let roots = match raw {
        Value::Null => &[][..],
        Value::Array(items) => items.as_slice(),
        _ => return Err(malformed("document symbols must be null or an array")),
    };
    let flat = roots
        .first()
        .is_some_and(|value| value.get("location").is_some());
    let mut symbols = Vec::new();
    let mut boundaries = BTreeSet::new();
    let mut stack = vec![(roots.iter(), None, 0_usize)];
    while let Some((siblings, parent, depth)) = stack.last_mut() {
        let Some(value) = siblings.next() else {
            stack.pop();
            continue;
        };
        let (parent, depth) = (*parent, *depth);
        budget.remaining()?;
        if symbols.len() >= MAX_SYMBOLS || depth >= MAX_DEPTH {
            return Err(tool_err(
                "LSP_SEMANTIC_LIMIT",
                "document outline exceeds 8192 symbols or 32 levels",
            ));
        }
        if !value.is_object() || value.get("location").is_some() != flat {
            return Err(malformed("document symbol forms must not be mixed"));
        }
        metadata(value)?;
        let (range, selection) = if flat {
            if value.get("children").is_some()
                || value.get("range").is_some()
                || value.get("selectionRange").is_some()
            {
                return Err(malformed(
                    "flat document symbols cannot contain hierarchical fields",
                ));
            }
            let uri = value["location"]["uri"]
                .as_str()
                .filter(|uri| uri.len() <= 8192)
                .ok_or_else(|| malformed("flat document symbol needs a bounded URI"))?;
            // URI normalization is lexical. Do not open a server-supplied
            // target or validate its ranges against the wrong document.
            if uri_to_path(uri).as_deref() != Some(path) {
                return Err(tool_err(
                    "LSP_SEMANTIC_SCOPE",
                    "document symbol refers to a different source",
                ));
            }
            (range(&value["location"]["range"], &mut boundaries)?, None)
        } else {
            let outer = range(&value["range"], &mut boundaries)?;
            let selected = range(&value["selectionRange"], &mut boundaries)?;
            if selected.start < outer.start || selected.end > outer.end {
                return Err(malformed(
                    "document symbol selectionRange must be inside range",
                ));
            }
            (outer, Some(selected))
        };
        let index = symbols.len();
        symbols.push(Symbol {
            value,
            range,
            selection,
            parent,
            depth,
        });
        if !flat {
            match value.get("children") {
                None | Some(Value::Null) => {}
                Some(Value::Array(children)) => {
                    stack.push((children.iter(), Some(index), depth + 1));
                }
                Some(_) => return Err(malformed("document symbol children must be an array")),
            }
        }
    }
    let offsets = validate_boundaries(text, &boundaries, budget)?;
    Ok(Outline {
        symbols,
        offsets,
        shape: if roots.is_empty() {
            "empty"
        } else if flat {
            "flat"
        } else {
            "hierarchical"
        },
    })
}

fn summary(index: usize, symbol: &Symbol<'_>) -> Value {
    json!({
        "index":index,"parentIndex":symbol.parent,"depth":symbol.depth,
        "name":symbol.value["name"],"kind":symbol.value["kind"],
        "detail":symbol.value.get("detail"),"tags":symbol.value.get("tags"),
        "deprecated":symbol.value.get("deprecated"),"containerName":symbol.value.get("containerName"),
        "range":symbol.range,"selectionRange":symbol.selection,
        "location":symbol.value.get("location")
    })
}

#[cfg(test)]
fn displayed(
    outline: &Outline<'_>,
    indices: impl Iterator<Item = usize>,
    limit: usize,
    budget: &Budget,
) -> Result<Vec<Value>> {
    displayed_with_source(outline, indices, limit, None, budget)
}

fn displayed_with_source(
    outline: &Outline<'_>,
    indices: impl Iterator<Item = usize>,
    limit: usize,
    source: Option<(&str, Option<u32>)>,
    budget: &Budget,
) -> Result<Vec<Value>> {
    let source = source
        .map(|(text, line)| source_text::SourceText::new(text, line, budget))
        .transpose()?;
    let mut rows = Vec::new();
    let mut bytes = 2_usize; // array brackets
    for index in indices.take(limit.min(MAX_DISPLAY)) {
        budget.remaining()?;
        let symbol = &outline.symbols[index];
        let mut row = summary(index, symbol);
        // Filtering can omit a containing class. Preserve its semantic path
        // on the result instead of inventing a new parent or losing context.
        let mut ancestors = Vec::new();
        let mut parent = symbol.parent;
        while let Some(index) = parent {
            budget.remaining()?;
            let ancestor = &outline.symbols[index];
            ancestors.push(json!({
                "index":index,"name":ancestor.value["name"],"kind":ancestor.value["kind"],
                "range":ancestor.range,"selectionRange":ancestor.selection
            }));
            parent = ancestor.parent;
        }
        ancestors.reverse();
        row["ancestors"] = json!(ancestors);
        if let Some(source) = &source {
            row["source"] = source.excerpt(outline, symbol, budget)?;
        }
        let size = bounded_size(&row, MAX_RESPONSE_BYTES)? + usize::from(!rows.is_empty());
        if size > (MAX_PAYLOAD_BYTES - 8192).saturating_sub(bytes) {
            break;
        }
        bytes += size;
        rows.push(row);
    }
    Ok(rows)
}

impl LspTool {
    pub(in crate::lsp) async fn run_document_symbols(
        &self,
        input: &LspInput,
    ) -> Result<ToolOutput> {
        let filter = Filter::parse(input)?;
        let budget = Budget::new(self.request_timeout(input));
        let source =
            Source::open(self, input, &budget, |text| filter.validate_source(text)).await?;
        match source
            .entry
            .client
            .capabilities()
            .raw
            .get("documentSymbolProvider")
        {
            Some(Value::Bool(true) | Value::Object(_)) => {}
            None | Some(Value::Bool(false)) => {
                return Err(tool_err(
                    "LSP_UNSUPPORTED",
                    "server did not advertise documentSymbolProvider",
                ));
            }
            Some(_) => return Err(malformed("invalid documentSymbolProvider capability")),
        }
        let raw = source
            .entry
            .client
            .call_with_array_partial_results(
                "textDocument/documentSymbol",
                json!({"textDocument":{"uri":source.uri}}),
                budget.remaining()?,
                MAX_RESPONSE_BYTES,
                MAX_SYMBOLS,
            )
            .await?;
        source.verify(&budget)?;
        let outline = parse(&raw, &source.path, &source.text, &budget)?;
        let matching = select(&outline, &filter, &budget)?;
        // Selecting definitions also reads their code from the exact immutable
        // source used to validate the outline. The unfiltered outline remains
        // metadata-only; no returned URI is opened and no second LSP call runs.
        let include_source = filter.name.is_some() || filter.line.is_some();
        let rows = displayed_with_source(
            &outline,
            matching.indices.iter().copied(),
            input.limit.unwrap_or(100),
            include_source.then_some((source.text.as_ref(), filter.line)),
            &budget,
        )?;
        let source_sha256 = include_source
            .then(|| source_text::fingerprint(&source.text, &budget))
            .transpose()?;
        let source_truncated = rows.iter().any(|row| row["source"]["truncated"] == true);
        let total = outline.symbols.len();
        let truncated = rows.len() < matching.indices.len();
        let payload = json!({
            "schema":"pi.lsp.document_symbols.v1","action":"symbols",
            "file":display_path(&source.path, &self.cwd),"server":source.entry.spec_name,
            "shape":outline.shape,"total":total,"matched":matching.matched,
            "selectedCount":matching.indices.len(),"count":rows.len(),
            "filter":{"symbol":input.symbol,"name":filter.name,"occurrence":filter.occurrence,"line":input.line},
            "truncated":truncated,"responseComplete":true,"symbols":rows,
            "sourceIncluded":include_source,"sourceSha256":source_sha256,
            "sourceTruncated":source_truncated,
            "note":"The complete outline is validated before filtering. symbol matches case-sensitive definition names, not source text; optional #N selects the Nth match in server preorder after line filtering. One-based line intersects full half-open definition ranges. total counts every symbol, matched counts name/line matches, selectedCount applies #N, and count is the displayed subset. index/parentIndex refer to the whole outline; ancestors retain omitted containing symbols. Flat containerName never implies a parent. Name or line selection includes exact source excerpts, capped at 16 KiB per displayed definition and sharing the response budget. source.range and source.byteRange describe only the returned text; the symbol range remains the full server extent. source.truncated and sourceTruncated report omitted code independently of omitted symbols. With line, large extents show a bounded window around that requested line; omittedBeforeBytes and omittedAfterBytes count excluded code on each side. A flat location may cover only an identifier, not a body. sourceSha256 identifies the checked source text, not an approval handle or an atomic workspace snapshot. Exact zero-based UTF-16 ranges can be used with navigation or refactor tools, but are not approval handles."
        });
        let text = payload.to_string();
        let details = json!({"truncated":truncated,"payload":payload});
        bounded_size(&details, MAX_PAYLOAD_BYTES)?;
        source.verify(&budget)?;
        Ok(text_output(text, details))
    }
}

#[cfg(test)]
mod tests;
