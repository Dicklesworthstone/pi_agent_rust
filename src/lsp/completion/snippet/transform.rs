//! Bounded placeholder transforms for headless semantic completions.
//!
//! Only an explicitly checked ASCII, single-line subset of JavaScript regular
//! expressions is accepted. Do not silently reinterpret backreferences,
//! lookaround, Unicode, multiline anchors or Rust-only character-set syntax.
//! Captures are rendered structurally, never evaluated as code or re-expanded
//! as snippets. Pattern compilation, matching work and output are all bounded.

use regex::bytes::{Captures, Regex, RegexBuilder};

use super::{append, limit, malformed};
use crate::error::{Error, Result};
use crate::lsp::tool_err;

const MAX_PATTERN: usize = 1024;
const MAX_FORMAT: usize = 128;
const MAX_COMPILED: usize = 64 * 1024;
const MAX_CAPTURE: u32 = 65535;
pub(super) const MAX_TRANSFORMS: usize = 32;
pub(super) const SCAN_BUDGET: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Operation {
    Capture,
    Case(Case),
    If(String),
    Else(String),
    IfElse(String, String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Case {
    Upper,
    Lower,
    Capitalize,
    Camel,
    Pascal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Part {
    Text(String),
    Group(u32, Operation),
}

#[derive(Clone, Debug)]
pub(super) struct Transform {
    regex: Regex,
    insensitive: bool,
    global: bool,
    parts: Vec<Part>,
}

impl PartialEq for Transform {
    fn eq(&self, other: &Self) -> bool {
        self.regex.as_str() == other.regex.as_str()
            && self.insensitive == other.insensitive
            && self.global == other.global
            && self.parts == other.parts
    }
}

impl Eq for Transform {}

fn unsupported() -> Error {
    tool_err(
        "LSP_COMPLETION_UNSUPPORTED",
        "snippet transform requires a supported ASCII single-line pattern and value; only g/i flags, captures, conditionals and supported case modifiers are accepted",
    )
}

/// Validate the intersection with JavaScript syntax before using Rust's regex
/// engine. In particular, Rust character-set operators and inline flags must
/// not acquire meanings the server did not request.
fn validate_pattern(pattern: &str) -> Result<()> {
    if pattern.len() > MAX_PATTERN {
        return Err(limit());
    }
    if !pattern.is_ascii() || pattern.contains(['\r', '\n']) {
        return Err(unsupported());
    }
    let bytes = pattern.as_bytes();
    let mut index = 0;
    let mut in_class = false;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => {
                index += 1;
                let escaped = *bytes.get(index).ok_or_else(unsupported)?;
                if escaped == b'x' {
                    let hex = bytes.get(index + 1..index + 3).ok_or_else(unsupported)?;
                    if !hex.iter().all(u8::is_ascii_hexdigit) {
                        return Err(unsupported());
                    }
                    index += 2;
                } else if !(matches!(
                    escaped,
                    b'd' | b'D' | b'w' | b'W' | b's' | b'S' | b'n' | b'r' | b't' | b'f' | b'v'
                ) || (!in_class && matches!(escaped, b'b' | b'B'))
                    || b"\\^$.*+?()[]{}|/-".contains(&escaped))
                {
                    return Err(unsupported());
                }
            }
            b'[' => {
                if in_class {
                    return Err(unsupported());
                }
                in_class = true;
            }
            b']' => in_class = false,
            b'(' if !in_class && bytes.get(index + 1) == Some(&b'?') => {
                if bytes.get(index + 2) != Some(&b':') {
                    return Err(unsupported());
                }
            }
            b')' if !in_class => {
                // Repeated groups have different capture-reset semantics in
                // JavaScript and regex. Refuse that ambiguity rather than
                // retaining captures from an earlier repetition.
                if bytes
                    .get(index + 1)
                    .is_some_and(|byte| matches!(*byte, b'*' | b'+' | b'{'))
                {
                    return Err(unsupported());
                }
            }
            b'&' | b'-' | b'~' if in_class && bytes.get(index + 1) == Some(&bytes[index]) => {
                return Err(unsupported());
            }
            _ => {}
        }
        index += 1;
    }
    Ok(())
}

struct Parser<'a> {
    input: &'a str,
    offset: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<char> {
        self.input[self.offset..].chars().next()
    }

    fn take(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.offset += ch.len_utf8();
        Some(ch)
    }

    fn pattern(&mut self) -> Result<String> {
        let mut pattern = String::new();
        loop {
            match self.take() {
                Some('/') => break,
                Some('\\') => {
                    let ch = self
                        .take()
                        .ok_or_else(|| malformed("unterminated transform pattern"))?;
                    if ch != '/' {
                        pattern.push('\\');
                    }
                    pattern.push(ch);
                }
                Some(ch) => pattern.push(ch),
                None => return Err(malformed("unterminated transform pattern")),
            }
            if pattern.len() > MAX_PATTERN {
                return Err(limit());
            }
        }
        validate_pattern(&pattern)?;
        Ok(pattern)
    }

    fn index(&mut self) -> Result<u32> {
        let start = self.offset;
        while self.peek().is_some_and(|ch| ch.is_ascii_digit()) {
            let _ = self.take();
        }
        self.input[start..self.offset]
            .parse::<u32>()
            .ok()
            .filter(|index| *index <= MAX_CAPTURE)
            .ok_or_else(|| malformed("transform capture must be a bounded numeric index"))
    }

    /// Conditional arms are literal text, not nested snippet expressions.
    fn branch(&mut self, split_colon: bool) -> Result<(String, char)> {
        let mut text = String::new();
        loop {
            match self.take() {
                Some(ch @ ('}' | ':')) if ch == '}' || split_colon => return Ok((text, ch)),
                Some('\\') => match self.take() {
                    Some(ch @ ('\\' | '$' | '}' | '/' | ':')) => text.push(ch),
                    _ => return Err(malformed("invalid transform conditional escape")),
                },
                Some('$') if self.peek() == Some('{') => return Err(unsupported()),
                Some(ch) => text.push(ch),
                None => return Err(malformed("unterminated transform conditional")),
            }
        }
    }

    fn group(&mut self) -> Result<Part> {
        let braced = self.peek() == Some('{');
        if braced {
            let _ = self.take();
        }
        let index = self.index()?;
        if !braced {
            return Ok(Part::Group(index, Operation::Capture));
        }
        match self.take() {
            Some('}') => return Ok(Part::Group(index, Operation::Capture)),
            Some(':') => {}
            _ => return Err(malformed("invalid transform format")),
        }
        let operation = match self.peek() {
            Some('/') => {
                let _ = self.take();
                let (name, _) = self.branch(false)?;
                Operation::Case(match name.as_str() {
                    "upcase" => Case::Upper,
                    "downcase" => Case::Lower,
                    "capitalize" => Case::Capitalize,
                    "camelcase" => Case::Camel,
                    "pascalcase" => Case::Pascal,
                    _ => return Err(unsupported()),
                })
            }
            Some('+') => {
                let _ = self.take();
                Operation::If(self.branch(false)?.0)
            }
            Some('-') => {
                let _ = self.take();
                Operation::Else(self.branch(false)?.0)
            }
            Some('?') => {
                let _ = self.take();
                let (yes, delimiter) = self.branch(true)?;
                if delimiter != ':' {
                    return Err(malformed("transform conditional needs an else arm"));
                }
                Operation::IfElse(yes, self.branch(false)?.0)
            }
            _ => Operation::Else(self.branch(false)?.0),
        };
        Ok(Part::Group(index, operation))
    }

    fn replacement(&mut self) -> Result<Vec<Part>> {
        let mut parts = Vec::new();
        let mut text = String::new();
        loop {
            match self.take() {
                Some('/') => break,
                Some('\\') => match self.take() {
                    Some(ch @ ('\\' | '$' | '}' | '/')) => text.push(ch),
                    _ => return Err(malformed("invalid transform replacement escape")),
                },
                Some('$') => {
                    if !text.is_empty() {
                        parts.push(Part::Text(std::mem::take(&mut text)));
                    }
                    parts.push(self.group()?);
                }
                Some(ch) => text.push(ch),
                None => return Err(malformed("unterminated transform replacement")),
            }
            if parts.len() > MAX_FORMAT {
                return Err(limit());
            }
        }
        if !text.is_empty() {
            parts.push(Part::Text(text));
        }
        if parts.len() > MAX_FORMAT {
            return Err(limit());
        }
        Ok(parts)
    }
}

fn case(value: &str, mode: Case) -> String {
    match mode {
        Case::Upper => value.to_ascii_uppercase(),
        Case::Lower => value.to_ascii_lowercase(),
        Case::Capitalize => {
            let mut result = value.to_string();
            if let Some(first) = result.get_mut(..1) {
                first.make_ascii_uppercase();
            }
            result
        }
        Case::Camel | Case::Pascal => {
            let mut result = String::new();
            for (index, word) in value
                .split(|ch: char| !ch.is_ascii_alphanumeric())
                .filter(|word| !word.is_empty())
                .enumerate()
            {
                let (first, rest) = word.split_at(1);
                if mode == Case::Camel && index == 0 {
                    result.push_str(&first.to_ascii_lowercase());
                } else {
                    result.push_str(&first.to_ascii_uppercase());
                }
                result.push_str(rest);
            }
            if result.is_empty() {
                value.to_string()
            } else {
                result
            }
        }
    }
}

impl Transform {
    /// The opening slash was consumed by the numeric-placeholder parser.
    /// Commit the shared cursor only after the complete transform is valid.
    pub(super) fn parse(input: &str, offset: &mut usize) -> Result<Self> {
        let mut parser = Parser {
            input,
            offset: *offset,
        };
        let pattern = parser.pattern()?;
        let parts = parser.replacement()?;
        let mut global = false;
        let mut insensitive = false;
        loop {
            match parser.take() {
                Some('}') => break,
                Some('g') if !global => global = true,
                Some('i') if !insensitive => insensitive = true,
                None => return Err(malformed("unterminated snippet transform")),
                _ => return Err(unsupported()),
            }
        }
        let regex = RegexBuilder::new(&pattern)
            .unicode(false)
            .case_insensitive(insensitive)
            .size_limit(MAX_COMPILED)
            .dfa_size_limit(MAX_COMPILED)
            .nest_limit(32)
            .build()
            .map_err(|_| unsupported())?;
        if regex.captures_len() > 65 {
            return Err(limit());
        }
        *offset = parser.offset;
        Ok(Self {
            regex,
            insensitive,
            global,
            parts,
        })
    }

    fn format(&self, captures: Option<&Captures<'_>>, work: &mut usize) -> Result<String> {
        let mut output = String::new();
        for part in &self.parts {
            *work = work.checked_sub(1).ok_or_else(limit)?;
            match part {
                Part::Text(text) => append(&mut output, text)?,
                Part::Group(index, operation) => {
                    let value = captures
                        .and_then(|captures| {
                            captures.get(usize::try_from(*index).expect("bounded capture index"))
                        })
                        .map_or("", |found| {
                            // apply() accepts only ASCII subjects, including all captures.
                            std::str::from_utf8(found.as_bytes()).expect("ASCII capture")
                        });
                    match operation {
                        Operation::Capture => append(&mut output, value)?,
                        Operation::Case(mode) => append(&mut output, &case(value, *mode))?,
                        Operation::If(text) => {
                            if !value.is_empty() {
                                append(&mut output, text)?;
                            }
                        }
                        Operation::Else(text) => {
                            append(&mut output, if value.is_empty() { text } else { value })?;
                        }
                        Operation::IfElse(yes, no) => {
                            append(&mut output, if value.is_empty() { no } else { yes })?;
                        }
                    }
                }
            }
        }
        Ok(output)
    }

    pub(super) fn apply(&self, value: &str, work: &mut usize, scan: &mut usize) -> Result<String> {
        if !value.is_ascii() || value.contains(['\r', '\n']) {
            return Err(unsupported());
        }
        let mut output = String::new();
        let mut copied = 0;
        let mut search = 0;
        let mut matched = false;
        while search <= value.len() {
            // Repeated unanchored searches can otherwise make a global
            // replacement quadratic. Charge a conservative pattern x suffix
            // budget, shared across all occurrences in one expansion.
            let cost = (value.len() - search + 1)
                .checked_mul(self.regex.as_str().len().max(1))
                .ok_or_else(limit)?;
            *scan = scan.checked_sub(cost).ok_or_else(limit)?;
            *work = work.checked_sub(1).ok_or_else(limit)?;
            let Some(captures) = self.regex.captures_at(value.as_bytes(), search) else {
                break;
            };
            let found = captures.get(0).expect("full match");
            matched = true;
            append(&mut output, &value[copied..found.start()])?;
            append(&mut output, &self.format(Some(&captures), work)?)?;
            copied = found.end();
            if !self.global {
                break;
            }
            // JavaScript permits an empty match immediately after a nonempty
            // one. captures_iter suppresses that case, so advance explicitly.
            search = found.end() + usize::from(found.is_empty());
        }
        let has_else = self.parts.iter().any(|part| matches!(
            part,
            Part::Group(_, Operation::Else(text) | Operation::IfElse(_, text)) if !text.is_empty()
        ));
        if !matched && has_else {
            return self.format(None, work);
        }
        append(&mut output, &value[copied..])?;
        Ok(output)
    }
}

#[cfg(test)]
mod tests;
