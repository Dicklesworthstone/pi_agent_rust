//! Typed, bounded agent Markdown definitions. This is the documented agent
//! frontmatter subset, not an arbitrary YAML loader: no tags, anchors, merges,
//! interpolation, includes or executable constructors are evaluated.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use super::{AgentDefinition, AgentSource, Error, Result, Value, compile_output_schema};

pub(super) const MAX_DEFINITIONS: usize = 256;
const MAX_FILE_BYTES: usize = 1024 * 1024;
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_LIST_ITEMS: usize = 128;

fn invalid(path: &Path, reason: &str) -> Error {
    Error::tool(
        "subagent",
        format!("PI_AGENT_DEFINITION_INVALID: {path:?}: {reason}"),
    )
}

pub(super) fn read(path: &Path, source: AgentSource) -> Result<AgentDefinition> {
    // NONBLOCK makes a FIFO with an .md name fail the regular-file check
    // instead of suspending agent discovery. Authored symlinks remain allowed.
    #[cfg(unix)]
    let file = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    )
    .map(File::from)
    .map_err(std::io::Error::from);
    #[cfg(not(unix))]
    let file = File::open(path);
    let file = file.map_err(|_| invalid(path, "cannot open agent definition"))?;
    let metadata = file
        .metadata()
        .map_err(|_| invalid(path, "cannot inspect agent definition"))?;
    if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES as u64 {
        return Err(invalid(
            path,
            "definition must be a regular file of at most 1 MiB",
        ));
    }
    let mut bytes = Vec::new();
    file.take((MAX_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| invalid(path, "cannot read agent definition"))?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(invalid(path, "definition exceeds 1 MiB"));
    }
    let text =
        std::str::from_utf8(&bytes).map_err(|_| invalid(path, "definition must be UTF-8"))?;
    parse(text, path, source)
}

struct Field<'a> {
    inline: &'a str,
    continuation: &'a [&'a str],
}

fn split_header<'a>(raw: &'a str, path: &Path) -> Result<(&'a str, &'a str)> {
    if raw.len() > MAX_FILE_BYTES || raw.contains('\0') {
        return Err(invalid(
            path,
            "definition exceeds 1 MiB or contains a NUL byte",
        ));
    }
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let mut lines = raw.split_inclusive('\n');
    let first = lines
        .next()
        .ok_or_else(|| invalid(path, "missing frontmatter"))?;
    if first.trim() != "---" {
        return Err(invalid(path, "definition requires --- frontmatter"));
    }
    let start = first.len();
    let mut offset = start;
    for line in lines {
        if offset - start > MAX_HEADER_BYTES {
            return Err(invalid(path, "frontmatter exceeds 64 KiB"));
        }
        if line.trim_end_matches(['\r', '\n']) == "---" {
            return Ok((&raw[start..offset], &raw[offset + line.len()..]));
        }
        offset += line.len();
    }
    Err(invalid(path, "unclosed frontmatter"))
}

fn fields<'a>(lines: &'a [&'a str], path: &Path) -> Result<BTreeMap<&'a str, Field<'a>>> {
    let mut fields = BTreeMap::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        index += 1;
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        if line.starts_with([' ', '\t']) {
            return Err(invalid(path, "unexpected frontmatter indentation"));
        }
        let (key, inline) = line
            .split_once(':')
            .ok_or_else(|| invalid(path, "expected a frontmatter field"))?;
        let key = key.trim();
        if !matches!(
            key,
            "name"
                | "description"
                | "model"
                | "reasoning"
                | "thinking"
                | "tools"
                | "skills"
                | "output_schema"
                | "output"
                | "structured"
        ) {
            return Err(invalid(path, "unsupported frontmatter field"));
        }
        let start = index;
        while index < lines.len()
            && (lines[index].trim().is_empty()
                || lines[index].starts_with([' ', '\t'])
                || lines[index].starts_with('#'))
        {
            index += 1;
        }
        if fields
            .insert(
                key,
                Field {
                    inline: commentless(inline).trim(),
                    continuation: &lines[start..index],
                },
            )
            .is_some()
        {
            return Err(invalid(path, "duplicate frontmatter field"));
        }
    }
    for (first, second) in [("thinking", "reasoning"), ("output", "output_schema")] {
        if fields.contains_key(first) && fields.contains_key(second) {
            return Err(invalid(path, "conflicting frontmatter aliases"));
        }
    }
    Ok(fields)
}

/// YAML comments start outside quotes, after whitespace (or at the start).
/// Apostrophes in ordinary prose are data, not the start of a quoted scalar.
fn commentless(value: &str) -> &str {
    let mut quote = None;
    let mut escaped = false;
    let mut previous = None;
    let mut characters = value.char_indices().peekable();
    while let Some((index, character)) = characters.next() {
        if escaped {
            escaped = false;
        } else if quote == Some('"') && character == '\\' {
            escaped = true;
        } else if quote == Some('\'')
            && character == '\''
            && characters.peek().is_some_and(|(_, next)| *next == '\'')
        {
            let _ = characters.next();
        } else if quote == Some(character) {
            quote = None;
        } else if quote.is_none() {
            if character == '#' && previous.is_none_or(char::is_whitespace) {
                return &value[..index];
            }
            if matches!(character, '\'' | '"')
                && previous.is_none_or(|ch: char| ch.is_whitespace() || "[, {:".contains(ch))
            {
                quote = Some(character);
            }
        }
        previous = Some(character);
    }
    value
}

fn scalar(value: &str, path: &Path) -> Result<String> {
    let value = commentless(value).trim();
    if value.starts_with('"') {
        return serde_json::from_str::<String>(value)
            .map_err(|_| invalid(path, "invalid double-quoted scalar; use JSON escapes"));
    }
    if let Some(value) = value.strip_prefix('\'') {
        let value = value
            .strip_suffix('\'')
            .ok_or_else(|| invalid(path, "unclosed single-quoted scalar"))?;
        let mut chars = value.chars();
        let mut text = String::new();
        while let Some(character) = chars.next() {
            if character == '\'' && chars.next() != Some('\'') {
                return Err(invalid(
                    path,
                    "single quotes inside a scalar must be doubled",
                ));
            }
            text.push(character);
        }
        return Ok(text);
    }
    if value.starts_with(['[', '{', '&', '*', '!', '|', '>']) {
        return Err(invalid(
            path,
            "expected a literal scalar, not a collection, tag or anchor",
        ));
    }
    Ok(value.to_owned())
}

fn continuation_lines<'a>(field: &Field<'a>) -> impl Iterator<Item = &'a str> {
    field
        .continuation
        .iter()
        .copied()
        .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
}

fn field_text(field: &Field<'_>, path: &Path) -> Result<String> {
    if matches!(field.inline, "|" | "|-" | "|+" | ">" | ">-" | ">+") {
        return block_text(field, path);
    }
    if continuation_lines(field).next().is_some() {
        return Err(invalid(path, "multiline scalar requires | or >"));
    }
    scalar(field.inline, path)
}

fn block_text(field: &Field<'_>, path: &Path) -> Result<String> {
    let content: Vec<_> = field
        .continuation
        .iter()
        .copied()
        .filter(|line| !line.starts_with('#'))
        .collect();
    let indentation = content
        .iter()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start_matches(' ').len());
    let Some(indentation) = indentation else {
        return Ok(String::new());
    };
    if indentation == 0 {
        return Err(invalid(path, "block scalar must be indented with spaces"));
    }
    let prefix = " ".repeat(indentation);
    let lines: Vec<_> = content
        .iter()
        .map(|line| {
            if line.trim().is_empty() {
                Ok("")
            } else {
                line.strip_prefix(prefix.as_str())
                    .ok_or_else(|| invalid(path, "inconsistent block scalar indentation"))
            }
        })
        .collect::<Result<_>>()?;
    let mut text = if field.inline.starts_with('>') {
        let end = lines
            .iter()
            .rposition(|line| !line.is_empty())
            .map_or(0, |i| i + 1);
        // Keep the accepted subset exact, rather than inventing YAML's more
        // complex paragraph/more-indented folding rules. Literal blocks handle
        // those values without changing their bytes.
        if lines[..end]
            .iter()
            .any(|line| line.is_empty() || line.starts_with([' ', '\t']))
        {
            return Err(invalid(
                path,
                "folded scalars require plain consecutive lines; use | for paragraphs or indentation",
            ));
        }
        let mut text = lines[..end].join(" ");
        for _ in end..lines.len() {
            text.push('\n');
        }
        text
    } else {
        lines.join("\n")
    };
    text.push('\n');
    if field.inline.ends_with('-') {
        text.truncate(text.trim_end_matches('\n').len());
    } else if !field.inline.ends_with('+') {
        text.truncate(text.trim_end_matches('\n').len());
        text.push('\n');
    }
    Ok(text)
}

fn list_item(value: &str, path: &Path) -> Result<String> {
    let raw = commentless(value).trim();
    if raw.is_empty()
        || matches!(
            raw,
            "null" | "Null" | "NULL" | "~" | "true" | "false" | "True" | "False" | "TRUE" | "FALSE"
        )
        || raw.parse::<f64>().is_ok()
    {
        return Err(invalid(
            path,
            "list entries must be strings; quote numeric or boolean names",
        ));
    }
    if !raw.starts_with(['\'', '"']) && (raw.contains(": ") || raw.contains(":\t")) {
        return Err(invalid(path, "list entries must be strings, not mappings"));
    }
    let value = scalar(raw, path)?;
    if value.trim().is_empty() || value.contains('\0') {
        return Err(invalid(
            path,
            "list entries must be nonempty strings without NUL bytes",
        ));
    }
    Ok(value)
}

fn list(field: &Field<'_>, path: &Path) -> Result<Vec<String>> {
    let continuation: Vec<_> = continuation_lines(field).collect();
    let mut values = Vec::new();
    if !continuation.is_empty() {
        if !field.inline.is_empty() {
            return Err(invalid(path, "list cannot mix inline and block entries"));
        }
        let indentation = continuation[0].len() - continuation[0].trim_start_matches(' ').len();
        for line in continuation {
            if indentation == 0 || line.len() - line.trim_start_matches(' ').len() != indentation {
                return Err(invalid(
                    path,
                    "list entries require consistent space indentation",
                ));
            }
            let item = line
                .trim_start_matches(' ')
                .strip_prefix("- ")
                .ok_or_else(|| invalid(path, "expected a - string list entry"))?;
            values.push(list_item(item, path)?);
        }
    } else if field.inline.starts_with('[') {
        let interior = field
            .inline
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .ok_or_else(|| invalid(path, "unclosed inline list"))?;
        let mut start = 0;
        let mut quote = None;
        let mut escaped = false;
        let mut characters = interior.char_indices().peekable();
        while let Some((index, character)) = characters.next() {
            if escaped {
                escaped = false;
                continue;
            }
            if quote == Some('"') && character == '\\' {
                escaped = true;
                continue;
            }
            if quote == Some('\'')
                && character == '\''
                && characters.peek().is_some_and(|(_, next)| *next == '\'')
            {
                let _ = characters.next();
                continue;
            }
            if quote == Some(character) {
                quote = None;
            } else if quote.is_none()
                && matches!(character, '\'' | '"')
                && interior[start..index].trim().is_empty()
            {
                quote = Some(character);
            } else if quote.is_none() && character == ',' {
                values.push(list_item(&interior[start..index], path)?);
                start = index + 1;
            }
        }
        if quote.is_some() {
            return Err(invalid(path, "unclosed quoted list entry"));
        }
        if !interior[start..].trim().is_empty() {
            values.push(list_item(&interior[start..], path)?);
        }
    } else {
        let text = field_text(field, path)?;
        // A scalar retains native CSV semantics. Use a list to preserve a
        // comma inside one skill path. Explicit blank means zero items.
        for value in text
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            if field.inline.starts_with(['\'', '"']) {
                if value.contains('\0') {
                    return Err(invalid(path, "list entries cannot contain NUL bytes"));
                }
                values.push(value.to_owned());
            } else {
                values.push(list_item(value, path)?);
            }
        }
    }
    if values.len() > MAX_LIST_ITEMS {
        return Err(invalid(path, "tools or skills exceed 128 items"));
    }
    Ok(values)
}

fn parse(raw: &str, path: &Path, source: AgentSource) -> Result<AgentDefinition> {
    let (header, body) = split_header(raw, path)?;
    let lines: Vec<_> = header.lines().collect();
    let fields = fields(&lines, path)?;
    let required = |key: &str| -> Result<String> {
        let field = fields
            .get(key)
            .ok_or_else(|| invalid(path, &format!("requires frontmatter field {key}")))?;
        let value = field_text(field, path)?;
        if value.trim().is_empty() || value.contains('\0') {
            return Err(invalid(
                path,
                "required field must be a nonempty string without NUL bytes",
            ));
        }
        Ok(value.trim().to_owned())
    };
    let optional = |key: &str| -> Result<Option<String>> {
        fields.get(key).map(|_| required(key)).transpose()
    };
    let tools = fields
        .get("tools")
        .map(|field| list(field, path))
        .transpose()?;
    if tools.as_ref().is_some_and(|tools| {
        tools.iter().any(|tool| {
            tool.starts_with('-')
                || tool.contains(',')
                || tool
                    .chars()
                    .any(|character| character.is_whitespace() || character.is_control())
        })
    }) {
        return Err(invalid(
            path,
            "each tool must be one literal name without whitespace or commas",
        ));
    }
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let skills = fields
        .get("skills")
        .map(|field| list(field, path))
        .transpose()?
        .unwrap_or_default()
        .into_iter()
        .map(PathBuf::from)
        .map(|skill| {
            if skill.is_absolute() {
                skill
            } else {
                directory.join(skill)
            }
        })
        .collect();
    let structured = fields
        .get("structured")
        .map(|field| {
            if continuation_lines(field).next().is_some() {
                return Err(invalid(path, "structured must be a boolean"));
            }
            match field.inline {
                "true" => Ok(true),
                "false" => Ok(false),
                _ => Err(invalid(path, "structured must be true or false")),
            }
        })
        .transpose()?;
    let mut output_schema = fields
        .get("output_schema")
        .or_else(|| fields.get("output"))
        .map(|field| {
            let text = if field.inline.starts_with(['{', '['])
                && continuation_lines(field).next().is_none()
            {
                field.inline.to_owned()
            } else {
                field_text(field, path)?
            };
            let schema: Value = serde_json::from_str(&text)
                .map_err(|_| invalid(path, "invalid output_schema JSON"))?;
            if !schema.is_object() && !schema.is_boolean() {
                return Err(invalid(
                    path,
                    "output_schema must be a JSON Schema object or boolean",
                ));
            }
            compile_output_schema(&schema)
                .map_err(|_| invalid(path, "invalid output_schema JSON Schema"))?;
            Ok(schema)
        })
        .transpose()?;
    if structured == Some(false) && output_schema.is_some() {
        return Err(invalid(
            path,
            "structured: false conflicts with an explicit output_schema",
        ));
    }
    if structured == Some(true) && output_schema.is_none() {
        // Pi always validates a declared schema, regardless of the optional
        // structured-result text footer. An unconstrained schema still requires JSON.
        output_schema = Some(serde_json::json!({}));
    }
    Ok(AgentDefinition {
        name: required("name")?,
        description: required("description")?,
        model: optional("model")?,
        reasoning: optional("reasoning")?.or(optional("thinking")?),
        tools,
        skills,
        system_prompt: body.to_owned(),
        output_schema,
        source,
        file_path: path.to_path_buf(),
    })
}

#[cfg(test)]
mod tests;
