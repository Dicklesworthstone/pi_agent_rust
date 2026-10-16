//! Read saved notebook results without running a kernel, MIME hooks or widgets.
//! The projection is bounded and explicit about material it cannot render.

use std::borrow::Cow;
use std::io::Write as _;

use serde::Deserialize;
use serde_json::Value;

use crate::error::{Error, Result};

const MAX_CELLS: usize = 4096;
const MAX_OUTPUTS: usize = 8192;
const MAX_MIMES: usize = 32;
const MAX_RENDER_BYTES: usize = 16 * 1024 * 1024;

fn error(message: &str) -> Error {
    Error::tool("read", format!("[NOTEBOOK_INVALID] {message}"))
}

fn limit() -> Error {
    Error::tool("read", "[NOTEBOOK_LIMIT] notebook exceeds its input, structure or rendered-text budget")
}

fn source(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes)
}

pub(super) fn recognizes(bytes: &[u8]) -> bool {
    // IgnoredAny traverses without allocating another notebook-sized tree.
    // A string containing the word nbformat is not a notebook declaration.
    #[derive(Deserialize)]
    struct Header {
        nbformat: u64,
        cells: serde::de::IgnoredAny,
    }
    bytes.len() <= super::MAX_DOWNLOAD_BYTES
        && serde_json::from_slice::<Header>(source(bytes))
            .is_ok_and(|header| { let _ = header.cells; header.nbformat > 0 })
}

fn multiline(value: &Value) -> Result<Cow<'_, str>> {
    match value {
        Value::String(text) => Ok(Cow::Borrowed(text)),
        Value::Array(parts) => {
            let mut text = String::new();
            for part in parts {
                let part = part.as_str().ok_or_else(|| error("multiline arrays must contain only strings"))?;
                if part.len() > MAX_RENDER_BYTES.saturating_sub(text.len()) { return Err(limit()); }
                text.push_str(part);
            }
            Ok(Cow::Owned(text))
        }
        _ => Err(error("source and text fields must be a string or string array")),
    }
}

struct Render {
    bytes: Vec<u8>,
    limit: usize,
}

impl std::io::Write for Render {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::other("notebook render budget exhausted"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

impl Render {
    fn push(&mut self, text: &str) -> Result<()> {
        self.write_all(text.as_bytes()).map_err(|_| limit())
    }

    fn text(&mut self, text: &str) -> Result<()> {
        // Tracebacks routinely contain ANSI sequences. Keep their information,
        // not executable terminal controls. Newlines, CRLF and tabs remain text.
        let mut start = 0;
        for (offset, ch) in text.char_indices() {
            if ch.is_control() && !matches!(ch, '\n' | '\r' | '\t') {
                self.push(&text[start..offset])?;
                self.push(&format!("\\u{{{:04x}}}", u32::from(ch)))?;
                start = offset + ch.len_utf8();
            }
        }
        self.push(&text[start..])
    }

    fn fence(&mut self, text: &str, language: &str) -> Result<()> {
        let longest = |marker: char| {
            let (mut longest, mut run) = (0usize, 0usize);
            for ch in text.chars() {
                if ch == marker { run += 1; longest = longest.max(run); } else { run = 0; }
            }
            longest
        };
        let backticks = longest('`');
        let tildes = longest('~');
        let (marker, run) = if backticks <= tildes { ("`", backticks) } else { ("~", tildes) };
        let length = run.saturating_add(1).max(3);
        if length > 256 { return Err(limit()); }
        let fence = marker.repeat(length);
        self.push(&fence)?;
        self.push(language)?;
        self.push("\n")?;
        self.text(text)?;
        if !text.ends_with('\n') { self.push("\n")?; }
        self.push(&fence)?;
        self.push("\n")
    }

    fn json(&mut self, value: &Value) -> Result<()> {
        self.push("```json\n")?;
        // Stream the serializer directly into the shared bounded output. Do
        // not allocate a potentially much larger pretty-printed intermediate.
        serde_json::to_writer_pretty(&mut *self, value).map_err(|_| limit())?;
        self.push("\n```\n")
    }

    fn mime_bundle(&mut self, value: &Value) -> Result<()> {
        let bundle = value.as_object().ok_or_else(|| error("display output needs a MIME object"))?;
        if bundle.len() > MAX_MIMES { return Err(limit()); }
        let selected = ["application/json", "text/plain", "text/markdown", "text/html"]
            .into_iter().find(|mime| bundle.contains_key(*mime));
        if let Some(mime) = selected {
            self.push(&format!("_representation: {mime}_\n\n"))?;
            let value = &bundle[mime];
            match mime {
                "application/json" => self.json(value)?,
                "text/plain" => self.fence(&multiline(value)?, "text")?,
                "text/markdown" => { self.text(&multiline(value)?)?; self.push("\n")?; }
                "text/html" => {
                    // Existing static reader: no DOM execution or remote loads.
                    let html = multiline(value)?;
                    let markdown = super::generic_html_to_markdown(&html);
                    self.text(&markdown)?;
                    self.push("\n")?;
                }
                _ => unreachable!(),
            }
        } else {
            self.push("[No supported text representation; binary/widget output was not rendered.]\n")?;
        }
        if bundle.len() > usize::from(selected.is_some()) {
            self.push("_Other saved MIME representations are not rendered:_ ")?;
            let mut first = true;
            for key in bundle.keys().filter(|key| Some(key.as_str()) != selected) {
                if key.len() > 256 { return Err(limit()); }
                if !first { self.push(", ")?; }
                first = false;
                // Quote names as data, never interpolate an arbitrary MIME key
                // as a Markdown URL or HTML attribute.
                let key = serde_json::to_string(key).map_err(|_| error("invalid MIME name"))?;
                self.text(&key)?;
            }
            self.push("\n")?;
        }
        Ok(())
    }

    fn output(&mut self, output: &Value, index: usize) -> Result<()> {
        let kind = output.get("output_type").and_then(Value::as_str)
            .ok_or_else(|| error("output_type must be a string"))?;
        self.push(&format!("\n#### output {}\n\n", index + 1))?;
        match kind {
            "stream" => {
                let name = match output.get("name") {
                    Some(Value::String(name)) if matches!(name.as_str(), "stdout" | "stderr") => name.as_str(),
                    None => "stream", // Some older saved notebooks omit the name.
                    _ => return Err(error("stream name must be stdout or stderr")),
                };
                self.push(&format!("_saved {name}_\n\n"))?;
                self.fence(&multiline(output.get("text").ok_or_else(|| error("stream has no text"))?)?, "text")?;
            }
            "display_data" | "execute_result" => {
                if kind == "execute_result" { self.execution(output)?; }
                self.mime_bundle(output.get("data").ok_or_else(|| error("display has no data"))?)?;
            }
            "error" => {
                self.push("_saved execution error_\n\n")?;
                let name = output.get("ename").and_then(Value::as_str).ok_or_else(|| error("error has no ename"))?;
                let value = output.get("evalue").and_then(Value::as_str).ok_or_else(|| error("error has no evalue"))?;
                self.fence(&format!("{name}: {value}"), "text")?;
                if let Some(traceback) = output.get("traceback") {
                    let frames = traceback.as_array().ok_or_else(|| error("traceback must be a string array"))?;
                    for frame in frames {
                        self.fence(frame.as_str().ok_or_else(|| error("traceback frame must be a string"))?, "text")?;
                    }
                }
            }
            _ => {
                self.push("[Unsupported saved output type: ")?;
                self.text(&serde_json::to_string(kind).map_err(|_| error("invalid output type"))?)?;
                self.push("; payload not rendered.]\n")?;
            }
        }
        Ok(())
    }

    fn execution(&mut self, object: &Value) -> Result<()> {
        match object.get("execution_count") {
            Some(Value::Null) => self.push("_execution_count: null (not recorded)_\n\n"),
            Some(value) => {
                let count = value.as_u64().ok_or_else(|| error("execution_count must be nonnegative integer or null"))?;
                self.push(&format!("_execution_count: {count}_\n\n"))
            }
            None => Ok(()),
        }
    }
}

pub(super) fn render(bytes: &[u8]) -> Result<String> {
    if bytes.len() > super::MAX_DOWNLOAD_BYTES { return Err(limit()); }
    let notebook: Value = serde_json::from_slice(source(bytes))
        .map_err(|_| error("malformed notebook JSON"))?;
    if notebook.get("nbformat").and_then(Value::as_u64) != Some(4) {
        return Err(error("only nbformat 4 is supported"));
    }
    let cells = notebook.get("cells").and_then(Value::as_array)
        .ok_or_else(|| error("notebook needs a cells array"))?;
    if cells.len() > MAX_CELLS { return Err(limit()); }
    let mut out = Render { bytes: Vec::new(), limit: MAX_RENDER_BYTES };
    let mut total_outputs = 0usize;
    let language = notebook.pointer("/metadata/language_info/name").and_then(Value::as_str)
        .filter(|name| name.len() <= 32 && name.bytes().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'_' | b'+' | b'-')))
        .unwrap_or("");
    out.push("# Notebook\n\nSaved notebook content and outputs; code was not executed by this read.\n")?;
    for (index, cell) in cells.iter().enumerate() {
        let kind = cell.get("cell_type").and_then(Value::as_str).ok_or_else(|| error("cell_type must be a string"))?;
        let known = matches!(kind, "code" | "markdown" | "raw");
        out.push(&format!("\n### cell {} ({})\n", index + 1, if known { kind } else { "unsupported" }))?;
        let text = multiline(cell.get("source").ok_or_else(|| error("cell has no source"))?)?;
        match kind {
            "code" => { out.execution(cell)?; out.fence(&text, language)?; }
            "markdown" => { out.text(&text)?; out.push("\n")?; }
            _ => { out.fence(&text, "text")?; }
        }
        if !known { out.push("[Unknown cell type; only its source is shown.]\n")?; }
        if let Some(attachments) = cell.get("attachments") {
            let attachments = attachments.as_object().ok_or_else(|| error("attachments must be an object"))?;
            if !attachments.is_empty() {
                out.push(&format!("\n[{} saved attachments not rendered or fetched.]\n", attachments.len()))?;
            }
        }
        if let Some(outputs) = cell.get("outputs") {
            let outputs = outputs.as_array().ok_or_else(|| error("outputs must be an array"))?;
            total_outputs = total_outputs.checked_add(outputs.len()).ok_or_else(limit)?;
            if total_outputs > MAX_OUTPUTS { return Err(limit()); }
            if !outputs.is_empty() {
                out.push(&format!("\n_outputs: {} present_\n", outputs.len()))?;
                for (index, output) in outputs.iter().enumerate() { out.output(output, index)?; }
            }
        }
    }
    String::from_utf8(out.bytes).map_err(|_| error("rendered notebook is not UTF-8"))
}

#[cfg(test)]
mod tests;
