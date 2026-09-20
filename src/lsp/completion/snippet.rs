//! Deterministic numeric snippet expansion; never read variables or run code.
//!
//! Parse the whole snippet before applying any caller substitutions. Defaults
//! may nest and references may precede their definition. Values are literal
//! text, never reparsed as snippets. The server's cached item stays immutable.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use super::{MAX_ITEM_BYTES, bounded_size, malformed};
use crate::error::Result;
use crate::lsp::tool_err;

pub(super) type Values = BTreeMap<String, String>;
const MAX_FIELDS: usize = 64;
const MAX_DEPTH: usize = 32;
const MAX_NODES: usize = 2048;
const MAX_WORK: usize = 32_768;
const MAX_TEXT: usize = 64 * 1024;
const MAX_MEMO: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Node {
    Text(String),
    Field(u32),
}

#[derive(Debug, PartialEq, Eq)]
enum DefaultValue {
    Nodes(Vec<Node>),
    Choices(Vec<String>),
}

struct Parser<'a> {
    input: &'a str,
    offset: usize,
    nodes: usize,
    ids: BTreeSet<u32>,
    defaults: BTreeMap<u32, DefaultValue>,
}

fn limit() -> crate::error::Error {
    tool_err(
        "LSP_COMPLETION_LIMIT",
        "snippet exceeds its nesting, expansion or field limit",
    )
}

pub(super) fn validate_values(values: &Values) -> Result<()> {
    if values.len() > MAX_FIELDS {
        return Err(limit());
    }
    let mut bytes = 0usize;
    for (key, value) in values {
        let index = key.parse::<u32>().ok().filter(|index| *index <= 65535);
        if index.is_none_or(|index| index.to_string() != *key) {
            return Err(tool_err(
                "LSP_USAGE",
                "snippetValues keys must be canonical decimal indices from 0 to 65535",
            ));
        }
        bytes = bytes.saturating_add(value.len());
        if value.len() > 16 * 1024 || bytes > MAX_TEXT {
            return Err(limit());
        }
    }
    Ok(())
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Result<Self> {
        if input.len() > MAX_TEXT {
            return Err(limit());
        }
        Ok(Self {
            input,
            offset: 0,
            nodes: 0,
            ids: BTreeSet::new(),
            defaults: BTreeMap::new(),
        })
    }

    fn peek(&self) -> Option<char> {
        self.input[self.offset..].chars().next()
    }

    fn take(&mut self) -> Option<char> {
        let next = self.peek()?;
        self.offset += next.len_utf8();
        Some(next)
    }

    fn push(&mut self, nodes: &mut Vec<Node>, node: Node) -> Result<()> {
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            return Err(limit());
        }
        nodes.push(node);
        Ok(())
    }

    fn sequence(&mut self, nested: bool, depth: usize) -> Result<Vec<Node>> {
        if depth > MAX_DEPTH {
            return Err(limit());
        }
        let mut nodes = Vec::new();
        let mut text = String::new();
        loop {
            match self.take() {
                Some('}') if nested => break,
                None if nested => return Err(malformed("unterminated snippet placeholder")),
                None => break,
                Some('\\') => match self.take() {
                    Some(ch @ ('$' | '}' | '\\')) => text.push(ch),
                    _ => return Err(malformed("invalid snippet text escape")),
                },
                Some('$') => {
                    if !text.is_empty() {
                        self.push(&mut nodes, Node::Text(std::mem::take(&mut text)))?;
                    }
                    let id = self.field(depth)?;
                    self.push(&mut nodes, Node::Field(id))?;
                }
                Some(ch) => text.push(ch),
            }
        }
        if !text.is_empty() {
            self.push(&mut nodes, Node::Text(text))?;
        }
        Ok(nodes)
    }

    fn field(&mut self, depth: usize) -> Result<u32> {
        let braced = self.peek() == Some('{');
        if braced {
            let _ = self.take();
        }
        if !self.peek().is_some_and(|ch| ch.is_ascii_digit()) {
            return Err(tool_err(
                "LSP_COMPLETION_UNSUPPORTED",
                "snippet variables and transformations are not supported; no environment or clipboard is read",
            ));
        }
        let start = self.offset;
        while self.peek().is_some_and(|ch| ch.is_ascii_digit()) {
            let _ = self.take();
        }
        let id = self.input[start..self.offset]
            .parse::<u32>()
            .ok()
            .filter(|index| *index <= 65535)
            .ok_or_else(limit)?;
        self.ids.insert(id);
        if self.ids.len() > MAX_FIELDS {
            return Err(limit());
        }
        if !braced {
            return Ok(id);
        }
        let default = match self.take() {
            Some('}') => return Ok(id),
            Some(':') => DefaultValue::Nodes(self.sequence(true, depth + 1)?),
            Some('|') => DefaultValue::Choices(self.choices()?),
            Some('/') => {
                return Err(tool_err(
                    "LSP_COMPLETION_UNSUPPORTED",
                    "snippet transformations are not supported",
                ));
            }
            _ => return Err(malformed("invalid snippet placeholder suffix")),
        };
        if self
            .defaults
            .get(&id)
            .is_some_and(|prior| prior != &default)
        {
            return Err(malformed(
                "repeated snippet placeholder has conflicting defaults",
            ));
        }
        self.defaults.insert(id, default);
        Ok(id)
    }

    fn choices(&mut self) -> Result<Vec<String>> {
        let mut choices = Vec::new();
        let mut text = String::new();
        loop {
            match self.take() {
                Some('\\') => match self.take() {
                    Some(ch @ (',' | '|' | '\\')) => text.push(ch),
                    _ => return Err(malformed("invalid snippet choice escape")),
                },
                Some(',') => {
                    choices.push(std::mem::take(&mut text));
                    if choices.len() >= 32 {
                        return Err(limit());
                    }
                }
                Some('|') => {
                    if self.take() != Some('}') {
                        return Err(malformed("unterminated snippet choice"));
                    }
                    choices.push(text);
                    return Ok(choices);
                }
                None => return Err(malformed("unterminated snippet choice")),
                Some(ch) => text.push(ch),
            }
        }
    }
}

struct Renderer<'a> {
    defaults: &'a BTreeMap<u32, DefaultValue>,
    values: BTreeMap<u32, &'a str>,
    memo: BTreeMap<u32, String>,
    active: BTreeSet<u32>,
    used: BTreeSet<u32>,
    missing: BTreeSet<u32>,
    work: usize,
    memo_bytes: usize,
}

fn append(output: &mut String, text: &str) -> Result<()> {
    if output.len().saturating_add(text.len()) > MAX_TEXT {
        return Err(limit());
    }
    output.push_str(text);
    Ok(())
}

impl Renderer<'_> {
    fn sequence(&mut self, nodes: &[Node], depth: usize) -> Result<String> {
        if depth > MAX_DEPTH {
            return Err(limit());
        }
        let mut output = String::new();
        for node in nodes {
            self.work = self.work.checked_sub(1).ok_or_else(limit)?;
            match node {
                Node::Text(text) => append(&mut output, text)?,
                Node::Field(id) => append(&mut output, &self.field(*id, depth + 1)?)?,
            }
        }
        Ok(output)
    }

    fn field(&mut self, id: u32, depth: usize) -> Result<String> {
        if depth > MAX_DEPTH {
            return Err(limit());
        }
        self.used.insert(id);
        if let Some(text) = self.memo.get(&id) {
            return Ok(text.clone());
        }
        if !self.active.insert(id) {
            return Err(malformed("cyclic snippet placeholder defaults"));
        }
        let text = if let Some(value) = self.values.get(&id) {
            (*value).to_string()
        } else {
            let defaults = self.defaults;
            match defaults.get(&id) {
                Some(DefaultValue::Nodes(nodes)) => self.sequence(nodes, depth)?,
                Some(DefaultValue::Choices(choices)) => {
                    choices.first().cloned().unwrap_or_default()
                }
                None => {
                    // $0 is a final cursor marker, not a required argument.
                    if id != 0 {
                        self.missing.insert(id);
                    }
                    String::new()
                }
            }
        };
        self.active.remove(&id);
        self.memo_bytes = self.memo_bytes.saturating_add(text.len());
        if self.memo_bytes > MAX_MEMO {
            return Err(limit());
        }
        self.memo.insert(id, text.clone());
        Ok(text)
    }
}

pub(super) struct Expanded {
    pub(super) item: Value,
    pub(super) fields: Value,
    pub(super) missing: Vec<u32>,
}

impl Expanded {
    pub(super) fn require_values(&self) -> Result<()> {
        if !self.missing.is_empty() {
            return Err(tool_err(
                "LSP_COMPLETION_VALUES_REQUIRED",
                format!(
                    "provide snippetValues for placeholders {:?}; unresolved arguments were not inserted",
                    self.missing
                ),
            ));
        }
        Ok(())
    }
}

/// Convert only the primary snippet insertion. Auto-import TextEdits are plain
/// text and must not be interpreted as snippets. Cache and resolve keep the
/// original server representation; preview choices never alter a later apply.
pub(super) fn prepare(item: &Value, values: Option<&Values>) -> Result<Expanded> {
    if let Some(values) = values {
        validate_values(values)?;
    }
    let format = item
        .get("insertTextFormat")
        .filter(|value| !value.is_null());
    if format.is_none() || format.is_some_and(|value| value.as_u64() == Some(1)) {
        if values.is_some() {
            return Err(tool_err(
                "LSP_USAGE",
                "snippetValues require a snippet completion",
            ));
        }
        return Ok(Expanded {
            item: item.clone(),
            fields: Value::Null,
            missing: Vec::new(),
        });
    }
    if !format.is_some_and(|value| value.as_u64() == Some(2)) {
        return Err(malformed("invalid completion insertTextFormat"));
    }
    bounded_size(item, MAX_ITEM_BYTES)?;
    let explicit_edit = item.get("textEdit").filter(|value| !value.is_null());
    let text = if let Some(edit) = explicit_edit {
        edit.get("newText")
    } else {
        item.get("insertText")
            .filter(|value| !value.is_null())
            .or_else(|| item.get("label"))
    }
    .and_then(Value::as_str)
    .ok_or_else(|| malformed("snippet insertion text must be a string"))?;
    let mut parser = Parser::new(text)?;
    let nodes = parser.sequence(false, 0)?;
    let values = values
        .into_iter()
        .flat_map(|values| values.iter())
        .map(|(key, value)| {
            (
                key.parse::<u32>().expect("validated snippet key"),
                value.as_str(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    if values.keys().any(|key| !parser.ids.contains(key)) {
        return Err(tool_err(
            "LSP_USAGE",
            "snippetValues contains an unknown placeholder index",
        ));
    }
    let mut renderer = Renderer {
        defaults: &parser.defaults,
        values,
        memo: BTreeMap::new(),
        active: BTreeSet::new(),
        used: BTreeSet::new(),
        missing: BTreeSet::new(),
        work: MAX_WORK,
        memo_bytes: 0,
    };
    let text = renderer.sequence(&nodes, 0)?;
    if renderer
        .values
        .keys()
        .any(|key| !renderer.used.contains(key))
    {
        return Err(tool_err(
            "LSP_USAGE",
            "snippetValues contains a placeholder suppressed by an outer replacement",
        ));
    }
    let fields = renderer
        .used
        .iter()
        .map(|id| {
            let choices = match parser.defaults.get(id) {
                Some(DefaultValue::Choices(choices)) => Some(choices),
                _ => None,
            };
            json!({"index":id,"missing":renderer.missing.contains(id),"choices":choices})
        })
        .collect::<Vec<_>>();
    let mut prepared = item.clone();
    if explicit_edit.is_some() {
        prepared["textEdit"]["newText"] = json!(text);
    } else {
        prepared["insertText"] = json!(text);
    }
    prepared["insertTextFormat"] = json!(1);
    Ok(Expanded {
        item: prepared,
        fields: json!(fields),
        missing: renderer.missing.into_iter().collect(),
    })
}

#[cfg(test)]
mod tests;
