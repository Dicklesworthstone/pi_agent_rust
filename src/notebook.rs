//! Reversible notebook source views for native file tools.
//!
//! Cell markers carry the original zero-based cell position. Edits preserve
//! the notebook document and reuse each referenced cell at most once. This
//! path never executes code or refreshes saved outputs.

use crate::error::{Error, Result};
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::io::Write;
use std::path::Path;

pub(crate) const MAX_NOTEBOOK_BYTES: usize = 10 * 1024 * 1024;
const MAX_CELLS: usize = 10_000;

pub(crate) fn is_notebook_path(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("ipynb"))
}

fn invalid(display: &str, reason: &str) -> Error {
    Error::validation(format!("[NOTEBOOK_EDIT] {display}: {reason}"))
}

fn normalize_source(source: &str) -> String {
    source.replace("\r\n", "\n").replace('\r', "\n")
}

fn source_text(cell: &Map<String, Value>, display: &str) -> Result<String> {
    match cell.get("source") {
        None => Ok(String::new()),
        Some(Value::String(source)) => Ok(source.clone()),
        Some(Value::Array(lines)) => {
            let mut source = String::new();
            for line in lines {
                source.push_str(line.as_str().ok_or_else(|| {
                    invalid(display, "cell source arrays must contain only strings")
                })?);
            }
            Ok(source)
        }
        Some(_) => Err(invalid(
            display,
            "cell source must be a string or string array",
        )),
    }
}

/// A marker-shaped line, including escaped markers. The percentage count
/// distinguishes a real delimiter (two) from literal source (three or more).
fn marker_parts(line: &str) -> Option<(&str, Option<&str>, usize)> {
    let rest = line.strip_prefix("# ")?;
    let percentages = rest.bytes().take_while(|byte| *byte == b'%').count();
    if percentages < 2 {
        return None;
    }
    let (kind, suffix) = rest
        .get(percentages..)?
        .strip_prefix(" [")?
        .split_once(']')?;
    if !matches!(kind, "code" | "markdown" | "raw") {
        return None;
    }
    let index = if suffix.is_empty() {
        None
    } else {
        let digits = suffix.strip_prefix(" cell:")?;
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        Some(digits)
    };
    Some((kind, index, percentages))
}

fn escaped_source(source: &str) -> String {
    let source = normalize_source(source);
    let mut escaped = String::with_capacity(source.len());
    for (index, line) in source.split('\n').enumerate() {
        if index > 0 {
            escaped.push('\n');
        }
        if marker_parts(line).is_some() {
            escaped.push_str(&line.replacen("# %", "# %%", 1));
        } else {
            escaped.push_str(line);
        }
    }
    escaped
}

struct EditedCell {
    kind: String,
    original_index: Option<usize>,
    source: String,
    has_source_line: bool,
}

fn parse_editable(text: &str, display: &str) -> Result<Vec<EditedCell>> {
    if text.len() > MAX_NOTEBOOK_BYTES {
        return Err(invalid(
            display,
            "editable notebook exceeds the 10 MiB limit",
        ));
    }
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let text = normalize_source(text);
    let mut cells: Vec<EditedCell> = Vec::new();
    for line in text.split('\n') {
        if let Some((kind, index, 2)) = marker_parts(line) {
            if cells.len() >= MAX_CELLS {
                return Err(invalid(display, "notebook exceeds the 10000-cell limit"));
            }
            let original_index = index
                .map(|index| index.parse::<usize>())
                .transpose()
                .map_err(|_| invalid(display, "cell index is too large"))?;
            cells.push(EditedCell {
                kind: kind.to_string(),
                original_index,
                source: String::new(),
                has_source_line: false,
            });
        } else {
            let cell = cells.last_mut().ok_or_else(|| {
                invalid(
                    display,
                    "editable text must start with # %% [code], # %% [markdown], or # %% [raw]; use raw=true to edit notebook JSON",
                )
            })?;
            let source = if marker_parts(line).is_some_and(|(_, _, count)| count > 2) {
                line.replacen("# %%", "# %", 1)
            } else {
                line.to_string()
            };
            if cell.has_source_line {
                cell.source.push('\n');
            }
            cell.source.push_str(&source);
            cell.has_source_line = true;
        }
    }
    Ok(cells)
}

pub(crate) struct Notebook {
    document: Value,
    display: String,
}

impl Notebook {
    pub(crate) fn parse(source: &str, display: &str) -> Result<Self> {
        if source.len() > MAX_NOTEBOOK_BYTES {
            return Err(invalid(display, "notebook exceeds the 10 MiB limit"));
        }
        let source = source.strip_prefix('\u{feff}').unwrap_or(source);
        let document: Value =
            serde_json::from_str(source).map_err(|_| invalid(display, "invalid notebook JSON"))?;
        if document.get("nbformat").and_then(Value::as_u64) != Some(4) {
            return Err(invalid(display, "only nbformat 4 is supported"));
        }
        let cells = document
            .get("cells")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid(display, "notebook needs a cells array"))?;
        if cells.len() > MAX_CELLS {
            return Err(invalid(display, "notebook exceeds the 10000-cell limit"));
        }
        for cell in cells {
            let cell = cell
                .as_object()
                .ok_or_else(|| invalid(display, "each cell must be an object"))?;
            if !matches!(
                cell.get("cell_type").and_then(Value::as_str),
                Some("code" | "markdown" | "raw")
            ) {
                return Err(invalid(display, "unsupported cell type"));
            }
            source_text(cell, display)?;
        }
        Ok(Self {
            document,
            display: display.to_string(),
        })
    }

    pub(crate) fn editable_text(&self) -> Result<String> {
        let cells = self.cells()?;
        let mut text = String::new();
        for (index, cell) in cells.iter().enumerate() {
            let cell = cell
                .as_object()
                .ok_or_else(|| invalid(&self.display, "invalid cell object"))?;
            let kind = cell
                .get("cell_type")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid(&self.display, "missing cell type"))?;
            if index > 0 {
                text.push('\n');
            }
            text.push_str(&format!("# %% [{kind}] cell:{index}"));
            let source = escaped_source(&source_text(cell, &self.display)?);
            if !source.is_empty() {
                text.push('\n');
                text.push_str(&source);
            }
            if text.len() > MAX_NOTEBOOK_BYTES {
                return Err(invalid(
                    &self.display,
                    "editable notebook exceeds the 10 MiB limit",
                ));
            }
        }
        Ok(text)
    }

    fn cells(&self) -> Result<&[Value]> {
        self.document
            .get("cells")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .ok_or_else(|| invalid(&self.display, "notebook needs a cells array"))
    }

    /// Reused cells retain IDs, outputs, attachments and unknown fields. An
    /// unchanged source retains even its original string/array representation
    /// and CRLF bytes. Changed code deliberately retains saved outputs, which
    /// may be stale; file editing never claims to have executed a cell.
    pub(crate) fn serialize_edited(&self, text: &str) -> Result<String> {
        let original = self.cells()?;
        let parsed = parse_editable(text, &self.display)?;
        let mut used = HashSet::new();
        let mut cells = Vec::with_capacity(parsed.len());
        for parsed in parsed {
            let reused = parsed
                .original_index
                .filter(|index| *index < original.len() && used.insert(*index))
                .and_then(|index| original.get(index))
                .and_then(Value::as_object);
            let mut cell = reused.cloned().unwrap_or_else(|| {
                Map::from_iter([
                    (
                        "id".to_string(),
                        Value::String(uuid::Uuid::new_v4().simple().to_string()),
                    ),
                    ("metadata".to_string(), Value::Object(Map::new())),
                ])
            });
            let source = parsed.source;
            if reused.is_none() || normalize_source(&source_text(&cell, &self.display)?) != source {
                cell.insert(
                    "source".to_string(),
                    Value::Array(if source.is_empty() {
                        Vec::new()
                    } else {
                        source
                            .split_inclusive('\n')
                            .map(|line| Value::String(line.to_string()))
                            .collect()
                    }),
                );
            }
            let is_code = parsed.kind == "code";
            cell.insert("cell_type".to_string(), Value::String(parsed.kind));
            if is_code {
                cell.entry("execution_count".to_string())
                    .or_insert(Value::Null);
                if cell.get("outputs").is_none_or(Value::is_null) {
                    cell.insert("outputs".to_string(), Value::Array(Vec::new()));
                }
            } else {
                cell.remove("execution_count");
                cell.remove("outputs");
            }
            cells.push(Value::Object(cell));
        }
        let mut document = self.document.clone();
        document["cells"] = Value::Array(cells);
        let mut output = BoundedJson(Vec::new());
        let formatter = serde_json::ser::PrettyFormatter::with_indent(b" ");
        document
            .serialize(&mut serde_json::Serializer::with_formatter(
                &mut output,
                formatter,
            ))
            .map_err(|_| {
                invalid(
                    &self.display,
                    "serialized notebook exceeds the 10 MiB limit",
                )
            })?;
        String::from_utf8(output.0).map_err(|_| {
            invalid(
                &self.display,
                "notebook serialization produced invalid text",
            )
        })
    }
}

struct BoundedJson(Vec<u8>);

impl Write for BoundedJson {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_NOTEBOOK_BYTES.saturating_sub(self.0.len()) {
            return Err(std::io::Error::other(
                "notebook serialization limit exceeded",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
