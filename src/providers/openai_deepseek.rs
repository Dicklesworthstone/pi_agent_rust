//! Recovery of DeepSeek tool calls emitted in the text channel.
//!
//! Ordinary prose streams immediately. Once unquoted tool markup starts, its
//! tail is withheld until the response ends: a later native `tool_calls` delta
//! must remain authoritative, and incomplete markup must never enter replay.
//! Recovered calls use the same terminal validation and agent execution path as
//! native calls. No argument repair or tool execution happens in this module.

use serde_json::{Map, Value};

const MAX_CAPTURE_BYTES: usize = 1024 * 1024;
const MAX_RECOVERED_CALLS: usize = 128;
const MAX_MARKER_BYTES: usize = 96;

type RecoveryResult<T> = std::result::Result<T, &'static str>;

#[derive(Debug)]
pub(super) enum Part {
    Text(String),
    Call { name: String, arguments: Value },
}

/// Provider identity or a complete model path component must identify DeepSeek.
/// In particular, URLs and incidental substrings never enable interpretation.
pub(super) fn is_target(provider: &str, model: &str) -> bool {
    if provider.eq_ignore_ascii_case("deepseek") {
        return true;
    }
    model.split('/').any(|component| {
        let component = component.to_ascii_lowercase();
        component == "deepseek"
            || component.starts_with("deepseek-")
            || component.starts_with("deepseek_")
            || component.starts_with("deepseek:")
    })
}

#[derive(Default)]
pub(super) struct Recovery {
    pending: String,
    captured: Option<String>,
    code: CodeState,
    inline_scan: InlineScan,
    thinking: bool,
    failure: Option<&'static str>,
}

impl Recovery {
    pub(super) fn feed(&mut self, text: &str) -> RecoveryResult<String> {
        self.consume(text, false)
    }

    pub(super) fn finish(&mut self) -> RecoveryResult<Vec<Part>> {
        let text = self.consume("", true)?;
        let mut parts = Vec::new();
        if !text.is_empty() {
            parts.push(Part::Text(text));
        }
        if let Some(captured) = self.captured.take() {
            parts.extend(parse_tail(&captured)?);
        }
        Ok(parts)
    }

    fn fail<T>(&mut self, message: &'static str) -> RecoveryResult<T> {
        self.failure = Some(message);
        self.pending.clear();
        self.captured = None;
        Err(message)
    }

    fn consume(&mut self, text: &str, final_chunk: bool) -> RecoveryResult<String> {
        if let Some(message) = self.failure {
            return Err(message);
        }
        if let Some(captured) = &mut self.captured {
            if captured.len().saturating_add(text.len()) > MAX_CAPTURE_BYTES {
                return self.fail("DeepSeek text tool envelope exceeds the recovery byte limit");
            }
            captured.push_str(text);
            return Ok(String::new());
        }

        self.pending.push_str(text);
        let mut cursor = 0;
        while cursor < self.pending.len() {
            let rest = &self.pending[cursor..];
            if self.thinking {
                if rest.starts_with("</think>") {
                    self.thinking = false;
                    self.code = CodeState::default();
                    cursor += "</think>".len();
                } else if !final_chunk && "</think>".starts_with(rest) {
                    break;
                } else {
                    cursor += rest.chars().next().unwrap_or_default().len_utf8();
                }
                continue;
            }
            self.code
                .settle_before(rest.chars().next().unwrap_or_default());
            let marker = marker_at(rest);
            let opens_thinking = rest.starts_with("<think>");
            if self.code.fence < 3 && (marker.is_some() || opens_thinking) {
                if self.code.fence > 0 {
                    match self.inline_scan.advance(rest, self.code.fence, final_chunk) {
                        InlineSpan::Closed(end) => {
                            self.inline_scan = InlineScan::default();
                            for character in rest[..end].chars() {
                                self.code.advance(character);
                            }
                            cursor += end;
                            continue;
                        }
                        InlineSpan::Pending => break,
                        InlineSpan::Unclosed => {
                            self.inline_scan = InlineScan::default();
                            self.code = CodeState::default();
                        }
                    }
                }
                if opens_thinking {
                    // Reasoning examples are never tool requests. Preserve
                    // the text, including tags, without interpreting its body.
                    self.thinking = true;
                    self.code = CodeState::default();
                    cursor += "<think>".len();
                    continue;
                }
                return self.capture(cursor);
            }
            if self.code.fence < 3 && (could_be_marker(rest) || "<think>".starts_with(rest)) {
                if !final_chunk {
                    break;
                }
                if self.code.fence == 0
                    && (rest.contains("DSML") || rest.contains("tool_") || rest.contains("tool▁"))
                {
                    return self.capture(cursor);
                }
            }
            let character = rest.chars().next().unwrap_or_default();
            self.code.advance(character);
            cursor += character.len_utf8();
        }

        if self.pending.len().saturating_sub(cursor) > MAX_CAPTURE_BYTES {
            return self.fail("DeepSeek quoted tool markup exceeds the recovery byte limit");
        }
        // An unfinished, recognizable tag is failed instead of replayed as text.
        if final_chunk && cursor != self.pending.len() {
            return self.fail("DeepSeek text tool envelope is incomplete");
        }
        if cursor == 0 {
            return Ok(String::new());
        }
        let retained = self.pending.split_off(cursor);
        Ok(std::mem::replace(&mut self.pending, retained))
    }

    fn capture(&mut self, start: usize) -> RecoveryResult<String> {
        let captured = self.pending.split_off(start);
        let visible = std::mem::take(&mut self.pending);
        if captured.len() > MAX_CAPTURE_BYTES {
            return self.fail("DeepSeek text tool envelope exceeds the recovery byte limit");
        }
        self.captured = Some(captured);
        Ok(visible)
    }
}

/// Markdown quotation state. Only a run of three or more at the start of a
/// line opens a fenced block, which lasts until its closing fence. Any other
/// backtick run opens an inline span that ends with its line, so an unmatched
/// backtick in earlier prose cannot quote a call on a later line.
#[derive(Clone, Copy, Default)]
struct CodeState {
    fence: usize,
    marker: char,
    block: bool,
    run: usize,
    run_marker: char,
    run_at_line_start: bool,
    line_has_text: bool,
}

impl CodeState {
    const fn settle_before(&mut self, character: char) {
        if self.run == 0 || character == self.run_marker {
            return;
        }
        if self.fence == 0 {
            if self.run >= 3 && self.run_at_line_start {
                self.fence = self.run;
                self.marker = self.run_marker;
                self.block = true;
            } else if self.run_marker == '`' {
                self.fence = self.run;
                self.marker = self.run_marker;
            }
        } else if self.marker == self.run_marker
            && (self.run == self.fence || (self.block && self.run >= self.fence))
        {
            self.fence = 0;
            self.block = false;
        }
        self.run = 0;
    }

    const fn advance(&mut self, character: char) {
        self.settle_before(character);
        if character == '`' || character == '~' {
            if self.run == 0 {
                self.run_at_line_start = !self.line_has_text;
            }
            self.run += 1;
            self.run_marker = character;
            self.line_has_text = true;
        } else if character == '\n' {
            if !self.block {
                self.fence = 0;
            }
            self.line_has_text = false;
        } else if !character.is_whitespace() {
            self.line_has_text = true;
        }
    }
}

enum InlineSpan {
    Closed(usize),
    Unclosed,
    Pending,
}

/// Looks ahead for the run that closes an inline span on the same line; a
/// newline first means the span quotes nothing, as in `CodeState`.
#[derive(Default)]
struct InlineScan {
    scanned: usize,
    run: usize,
}

impl InlineScan {
    fn advance(&mut self, text: &str, fence: usize, final_chunk: bool) -> InlineSpan {
        for (relative, character) in text[self.scanned..].char_indices() {
            let index = self.scanned + relative;
            if character == '`' {
                self.run += 1;
                continue;
            }
            if self.run == fence {
                return InlineSpan::Closed(index);
            }
            self.run = 0;
            if character == '\n' {
                return InlineSpan::Unclosed;
            }
        }
        self.scanned = text.len();
        if final_chunk && self.run == fence {
            InlineSpan::Closed(text.len())
        } else if final_chunk {
            InlineSpan::Unclosed
        } else {
            InlineSpan::Pending
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Marker {
    SectionOpen,
    SectionClose,
    CallOpen,
    CallClose,
    Separator,
    DsmlSectionOpen,
    DsmlSectionClose,
    DsmlInvokeOpen,
    DsmlInvokeClose,
    DsmlParameterOpen,
    DsmlParameterClose,
}

const TEMPLATE_NAMES: &[(&str, Marker)] = &[
    ("tool_calls_begin", Marker::SectionOpen),
    ("tool_calls_end", Marker::SectionClose),
    ("tool_call_begin", Marker::CallOpen),
    ("tool_call_end", Marker::CallClose),
    ("tool_sep", Marker::Separator),
];

const DSML_NAMES: &[(&str, Marker)] = &[
    ("tool_calls", Marker::DsmlSectionOpen),
    ("/tool_calls", Marker::DsmlSectionClose),
    ("invoke", Marker::DsmlInvokeOpen),
    ("/invoke", Marker::DsmlInvokeClose),
    ("parameter", Marker::DsmlParameterOpen),
    ("/parameter", Marker::DsmlParameterClose),
];

/// Returns the marker and its byte length. DSML openers with attributes are
/// recognized by their prefix; the strict attribute parser consumes the tag.
fn marker_at(text: &str) -> Option<(Marker, usize)> {
    if !text.starts_with('<') {
        return None;
    }
    for (name, marker) in DSML_NAMES {
        let (closing, name) = name
            .strip_prefix('/')
            .map_or(("", *name), |name| ("/", name));
        for bar in ["｜", "|"] {
            let prefix = format!("<{closing}{bar}DSML{bar}{name}");
            if let Some(tail) = text.strip_prefix(&prefix) {
                if tail.starts_with('>') {
                    return Some((*marker, prefix.len() + 1));
                }
                if matches!(marker, Marker::DsmlInvokeOpen | Marker::DsmlParameterOpen)
                    && tail.starts_with(char::is_whitespace)
                {
                    return Some((*marker, prefix.len()));
                }
            }
        }
    }
    let end = text
        .bytes()
        .take(MAX_MARKER_BYTES + 1)
        .position(|byte| byte == b'>')?;
    let body = text.strip_prefix('<')?.get(..end - 1)?;
    let has_bar = body.starts_with('｜') || body.ends_with('｜');
    if !has_bar {
        return None;
    }
    let name = body.strip_prefix('｜').unwrap_or(body);
    let name = name.strip_suffix('｜').unwrap_or(name);
    if name.starts_with('｜') || name.ends_with('｜') {
        return None;
    }
    let normalized = name.replace('▁', "_");
    TEMPLATE_NAMES
        .iter()
        .find(|(name, _)| *name == normalized)
        .map(|(_, marker)| (*marker, end + 1))
}

fn could_be_marker(text: &str) -> bool {
    if text.len() > MAX_MARKER_BYTES || !text.starts_with('<') || text.contains('>') {
        return false;
    }
    for (name, _) in DSML_NAMES {
        let (closing, name) = name
            .strip_prefix('/')
            .map_or(("", *name), |name| ("/", name));
        for bar in ["｜", "|"] {
            if format!("<{closing}{bar}DSML{bar}{name}>").starts_with(text) {
                return true;
            }
        }
    }
    let body = &text[1..];
    let body = body.strip_prefix('｜').unwrap_or(body);
    let normalized = body.trim_end_matches('｜').replace('▁', "_");
    TEMPLATE_NAMES
        .iter()
        .any(|(name, _)| name.starts_with(&normalized))
}

fn parse_tail(text: &str) -> RecoveryResult<Vec<Part>> {
    let mut parts = Vec::new();
    let mut cursor = 0;
    let mut visible_start = 0;
    let mut code = CodeState::default();
    let mut calls = 0;
    while cursor < text.len() {
        let rest = &text[cursor..];
        let character = rest.chars().next().unwrap_or_default();
        code.settle_before(character);
        let marker = marker_at(rest);
        let opens_thinking = rest.starts_with("<think>");
        if code.fence > 0 && code.fence < 3 && (marker.is_some() || opens_thinking) {
            match InlineScan::default().advance(rest, code.fence, true) {
                InlineSpan::Closed(end) => {
                    for character in rest[..end].chars() {
                        code.advance(character);
                    }
                    cursor += end;
                    continue;
                }
                InlineSpan::Unclosed | InlineSpan::Pending => code = CodeState::default(),
            }
        }
        if code.fence == 0 {
            if opens_thinking {
                // Preserve an unclosed reasoning envelope through the end.
                cursor += rest
                    .find("</think>")
                    .map_or(rest.len(), |end| end + "</think>".len());
                code = CodeState::default();
                continue;
            }
            if let Some((marker, length)) = marker {
                push_text(&mut parts, &text[visible_start..cursor]);
                let consumed = match marker {
                    Marker::SectionOpen => {
                        length + parse_template_section(&rest[length..], &mut parts, &mut calls)?
                    }
                    Marker::CallOpen => {
                        let (call, consumed) = parse_template_call(&rest[length..])?;
                        push_call(&mut parts, &mut calls, call)?;
                        length + consumed
                    }
                    Marker::DsmlSectionOpen => {
                        length + parse_dsml_section(&rest[length..], &mut parts, &mut calls)?
                    }
                    // Hosts sometimes duplicate a section close. Orphan DSML
                    // closers are likewise template noise, never replay text.
                    Marker::SectionClose
                    | Marker::CallClose
                    | Marker::DsmlSectionClose
                    | Marker::DsmlInvokeClose
                    | Marker::DsmlParameterClose => length,
                    Marker::Separator | Marker::DsmlInvokeOpen | Marker::DsmlParameterOpen => {
                        return Err("DeepSeek text tool markup is missing its enclosing call");
                    }
                };
                cursor += consumed;
                visible_start = cursor;
                continue;
            }
            if could_be_marker(rest)
                && (rest.contains("DSML") || rest.contains("tool_") || rest.contains("tool▁"))
            {
                return Err("DeepSeek text tool envelope is incomplete");
            }
        }
        code.advance(character);
        cursor += character.len_utf8();
    }
    push_text(&mut parts, &text[visible_start..]);
    Ok(parts)
}

fn push_text(parts: &mut Vec<Part>, text: &str) {
    if !text.is_empty() {
        if let Some(Part::Text(previous)) = parts.last_mut() {
            previous.push_str(text);
        } else {
            parts.push(Part::Text(text.to_string()));
        }
    }
}

fn push_call(parts: &mut Vec<Part>, count: &mut usize, call: Part) -> RecoveryResult<()> {
    if *count >= MAX_RECOVERED_CALLS {
        return Err("DeepSeek text tool envelope exceeds the recovery call limit");
    }
    *count += 1;
    parts.push(call);
    Ok(())
}

fn skip_space(text: &str) -> usize {
    text.len() - text.trim_start().len()
}

fn parse_template_section(
    text: &str,
    parts: &mut Vec<Part>,
    count: &mut usize,
) -> RecoveryResult<usize> {
    let initial_count = *count;
    let mut cursor = 0;
    loop {
        cursor += skip_space(&text[cursor..]);
        let Some((marker, length)) = marker_at(&text[cursor..]) else {
            return Err("DeepSeek text tool section is incomplete or malformed");
        };
        match marker {
            Marker::SectionClose if *count > initial_count => return Ok(cursor + length),
            Marker::CallOpen => {
                let (call, consumed) = parse_template_call(&text[cursor + length..])?;
                push_call(parts, count, call)?;
                cursor += length + consumed;
            }
            _ => return Err("DeepSeek text tool section contains unexpected markup"),
        }
    }
}

fn parse_template_call(text: &str) -> RecoveryResult<(Part, usize)> {
    let separator = text
        .char_indices()
        .take_while(|(index, _)| *index <= 512)
        .find_map(|(index, _)| {
            marker_at(&text[index..])
                .filter(|(marker, _)| *marker == Marker::Separator)
                .map(|(_, length)| (index, length))
        });
    let Some((separator, separator_length)) = separator else {
        return Err("DeepSeek text tool call is missing its argument separator");
    };
    let mut name = text[..separator].trim();
    let mut cursor = separator + separator_length;
    let legacy = name == "function";
    if legacy {
        let Some(fence) = text[cursor..].find("```json") else {
            return Err("DeepSeek legacy tool call is missing its JSON fence");
        };
        name = text[cursor..cursor + fence].trim();
        cursor += fence + "```json".len();
    }
    validate_name(name)?;
    cursor += skip_space(&text[cursor..]);
    let (arguments, consumed) = parse_object_prefix(&text[cursor..])?;
    cursor += consumed;
    cursor += skip_space(&text[cursor..]);
    if legacy {
        let Some(after) = text[cursor..].strip_prefix("```") else {
            return Err("DeepSeek legacy tool call has an unclosed JSON fence");
        };
        cursor = text.len() - after.len();
        cursor += skip_space(&text[cursor..]);
    }
    let Some((marker, length)) = marker_at(&text[cursor..]) else {
        return Err("DeepSeek text tool call is missing its closing marker");
    };
    match marker {
        Marker::CallClose => cursor += length,
        // Complete JSON makes these safe fallback boundaries. A marker inside
        // a JSON string was already consumed by the JSON parser above.
        Marker::SectionClose | Marker::CallOpen => {}
        _ => return Err("DeepSeek text tool call has an invalid closing marker"),
    }
    Ok((
        Part::Call {
            name: name.to_string(),
            arguments,
        },
        cursor,
    ))
}

fn parse_object_prefix(text: &str) -> RecoveryResult<(Value, usize)> {
    let mut values = serde_json::Deserializer::from_str(text).into_iter::<Value>();
    let value = values
        .next()
        .ok_or("DeepSeek text tool arguments are missing")?
        .map_err(|_| "DeepSeek text tool arguments are not complete JSON")?;
    if !value.is_object() {
        return Err("DeepSeek text tool arguments must be a JSON object");
    }
    let consumed = values.byte_offset();
    let value = super::parse_final_tool_arguments(&text[..consumed])?;
    Ok((value, consumed))
}

fn validate_name(name: &str) -> RecoveryResult<()> {
    let mut characters = name.chars();
    if name.len() > 128
        || !characters
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        || !characters.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return Err("DeepSeek text tool function name is invalid");
    }
    Ok(())
}

fn parse_dsml_section(
    text: &str,
    parts: &mut Vec<Part>,
    count: &mut usize,
) -> RecoveryResult<usize> {
    let initial_count = *count;
    let mut cursor = 0;
    loop {
        cursor += skip_space(&text[cursor..]);
        match marker_at(&text[cursor..]) {
            Some((Marker::DsmlSectionClose, length)) if *count > initial_count => {
                return Ok(cursor + length);
            }
            Some((Marker::DsmlInvokeOpen, _)) => {
                let (call, consumed) = parse_dsml_invoke(&text[cursor..])?;
                push_call(parts, count, call)?;
                cursor += consumed;
            }
            _ => return Err("DeepSeek DSML tool section is incomplete or malformed"),
        }
    }
}

struct Attributes {
    name: String,
    string_value: bool,
}

fn parse_dsml_open(text: &str, kind: &str) -> RecoveryResult<(Attributes, usize)> {
    let mut cursor = ["｜", "|"]
        .into_iter()
        .find_map(|bar| {
            let prefix = format!("<{bar}DSML{bar}{kind}");
            text.strip_prefix(&prefix).map(|_| prefix.len())
        })
        .ok_or("DeepSeek DSML tag is invalid")?;
    let mut name = None;
    let mut string_value = None;
    loop {
        let whitespace = skip_space(&text[cursor..]);
        cursor += whitespace;
        if text[cursor..].starts_with('>') {
            let name = name.ok_or("DeepSeek DSML tag has no name")?;
            return Ok((
                Attributes {
                    name,
                    string_value: string_value.unwrap_or(true),
                },
                cursor + 1,
            ));
        }
        if whitespace == 0 {
            return Err("DeepSeek DSML attributes must be separated by whitespace");
        }
        let start = cursor;
        while text
            .as_bytes()
            .get(cursor)
            .is_some_and(u8::is_ascii_alphabetic)
        {
            cursor += 1;
        }
        let key = &text[start..cursor];
        cursor += skip_space(&text[cursor..]);
        if !text[cursor..].starts_with('=') {
            return Err("DeepSeek DSML attribute has no value");
        }
        cursor += 1;
        cursor += skip_space(&text[cursor..]);
        let quote = match text.as_bytes().get(cursor) {
            Some(b'"') => '"',
            Some(b'\'') => '\'',
            _ => return Err("DeepSeek DSML attribute value is not quoted"),
        };
        cursor += 1;
        let Some(end) = text[cursor..].find(quote) else {
            return Err("DeepSeek DSML attribute value is incomplete");
        };
        let value = &text[cursor..cursor + end];
        cursor += end + 1;
        match key {
            "name" if name.is_none() => {
                if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
                    return Err("DeepSeek DSML name attribute is invalid");
                }
                name = Some(value.to_string());
            }
            "string" if kind == "parameter" && string_value.is_none() => {
                string_value = Some(match value {
                    "true" => true,
                    "false" => false,
                    _ => return Err("DeepSeek DSML string attribute must be true or false"),
                });
            }
            _ => return Err("DeepSeek DSML tag contains an unknown or repeated attribute"),
        }
    }
}

fn parse_dsml_invoke(text: &str) -> RecoveryResult<(Part, usize)> {
    let (invoke, mut cursor) = parse_dsml_open(text, "invoke")?;
    validate_name(&invoke.name)?;
    let mut arguments = Map::new();
    loop {
        cursor += skip_space(&text[cursor..]);
        match marker_at(&text[cursor..]) {
            Some((Marker::DsmlInvokeClose, length)) => {
                return Ok((
                    Part::Call {
                        name: invoke.name,
                        arguments: Value::Object(arguments),
                    },
                    cursor + length,
                ));
            }
            Some((Marker::DsmlParameterOpen, _)) => {
                let (parameter, opened) = parse_dsml_open(&text[cursor..], "parameter")?;
                cursor += opened;
                let end = text[cursor..]
                    .match_indices('<')
                    .find_map(|(index, _)| {
                        ["</｜DSML｜parameter>", "</|DSML|parameter>"]
                            .into_iter()
                            .find(|close| text[cursor + index..].starts_with(*close))
                            .map(|close| (index, close.len()))
                    })
                    .ok_or("DeepSeek DSML parameter is incomplete")?;
                let raw = &text[cursor..cursor + end.0];
                let value = if parameter.string_value {
                    Value::String(raw.to_string())
                } else {
                    let value = serde_json::from_str::<Value>(raw)
                        .map_err(|_| "DeepSeek DSML typed parameter is not complete JSON")?;
                    serde_json::from_str::<super::UniqueToolArgumentKeys>(raw).map_err(
                        |_| "DeepSeek DSML typed parameter contains duplicate JSON keys",
                    )?;
                    value
                };
                if arguments.insert(parameter.name, value).is_some() {
                    return Err("DeepSeek DSML tool call contains repeated parameters");
                }
                cursor += end.0 + end.1;
            }
            _ => return Err("DeepSeek DSML invocation is incomplete or malformed"),
        }
    }
}
