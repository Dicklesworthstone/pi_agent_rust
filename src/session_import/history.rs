//! Select one foreign conversation before converting or reconciling tools.
//!
//! Claude's parentUuid, not append order, defines the active transcript. Keep
//! excluded records in the import's audit attachments; never blend alternate
//! answers or subagent tool results into the selected conversation. Compact
//! boundaries follow parentUuid only, never logicalParentUuid.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use super::ImportSource;
use crate::error::{Error, Result};

const MAX_RECORDS: usize = 250_000;
const MAX_ID_BYTES: usize = 1024;

pub(super) struct SourceLine<'a> {
    /// One-based original line, including intervening blanks.
    pub number: usize,
    pub bytes: &'a [u8],
}

pub(super) struct History<'a> {
    pub lines: Vec<SourceLine<'a>>,
    pub order: Vec<usize>,
    pub leaf: Option<String>,
    pub branches: usize,
    excluded: HashSet<usize>,
}

struct Node {
    line: usize,
    id: String,
    parent: Option<String>,
    parent_declared: bool,
    message: bool,
    sidechain: bool,
    meta: bool,
    team: bool,
}

fn invalid(message: &str) -> Error {
    Error::tool("import", format!("[PI_IMPORT_HISTORY] {message}"))
}

fn identifier(value: &Value) -> Result<String> {
    value
        .as_str()
        .filter(|id| {
            !id.is_empty() && id.len() <= MAX_ID_BYTES && !id.chars().any(char::is_control)
        })
        .map(str::to_string)
        .ok_or_else(|| invalid("history identifiers must be nonempty bounded strings"))
}

fn flag(entry: &Value, name: &str) -> Result<bool> {
    match entry.get(name) {
        None => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(invalid("history flags must be booleans")),
    }
}

impl<'a> History<'a> {
    pub(super) fn prepare(
        source: ImportSource,
        raw: &'a [u8],
        selected: Option<&str>,
    ) -> Result<Self> {
        let mut lines = Vec::new();
        for (offset, bytes) in raw.split(|byte| *byte == b'\n').enumerate() {
            if bytes.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            if lines.len() == MAX_RECORDS {
                return Err(invalid("source exceeds the 250000 nonblank record limit"));
            }
            lines.push(SourceLine { number: offset + 1, bytes });
        }
        let mut history = Self {
            order: (0..lines.len()).collect(),
            lines,
            leaf: None,
            branches: 0,
            excluded: HashSet::new(),
        };
        if source == ImportSource::Claude {
            history.select_claude(selected)?;
        } else if selected.is_some() {
            return Err(invalid("explicit branch selection requires a Claude transcript"));
        }
        Ok(history)
    }

    pub(super) fn excluded(&self, line: usize) -> bool {
        self.excluded.contains(&line)
    }

    #[allow(clippy::too_many_lines)]
    fn select_claude(&mut self, selected: Option<&str>) -> Result<()> {
        let linked = self.lines.iter().any(|line| {
            serde_json::from_slice::<Value>(line.bytes).is_ok_and(|entry| {
                matches!(entry.get("type").and_then(Value::as_str), Some("user" | "assistant"))
                    && entry.get("parentUuid").is_some()
            })
        });
        if !linked {
            if selected.is_some() {
                return Err(invalid("explicit selection requires parent-linked messages"));
            }
            // Old/simple exports without parent links retain sequential
            // import semantics, including existing corruption tolerance.
            return Ok(());
        }
        let mut nodes = Vec::new();
        let mut by_id: HashMap<String, usize> = HashMap::new();
        let mut duplicate_of = HashMap::new();
        let mut messages = Vec::new();
        for (line, source) in self.lines.iter().enumerate() {
            // The ordinary importer retains malformed bytes. A missing link
            // to such a row is nevertheless an error, not a shorter success.
            let Ok(entry) = serde_json::from_slice::<Value>(source.bytes) else {
                continue;
            };
            let kind = entry.get("type").and_then(Value::as_str);
            let message = matches!(kind, Some("user" | "assistant"));
            if message {
                messages.push(line);
            }
            if !matches!(kind, Some("user" | "assistant" | "progress" | "system" | "attachment")) {
                continue;
            }
            // Do not choose an ancestry from a last-key-wins interpretation
            // of conflicting identity, role or message fields.
            serde_json::from_slice::<UniqueRoot>(source.bytes)
                .map_err(|_| invalid("history records must have unique top-level fields"))?;
            let Some(id) = entry.get("uuid") else {
                continue;
            };
            let id = identifier(id)?;
            if let Some(&previous) = by_id.get(&id) {
                let previous: &Node = &nodes[previous];
                if self.lines[previous.line].bytes.trim_ascii() != source.bytes.trim_ascii() {
                    return Err(invalid("conflicting records share a history UUID"));
                }
                // Repeated identical envelopes are audit records, never a
                // second execution or a second conversational message.
                duplicate_of.insert(line, previous.line);
                continue;
            }
            let parent = match entry.get("parentUuid") {
                None | Some(Value::Null) => None,
                Some(value) => Some(identifier(value)?),
            };
            let team = match entry.get("teamName") {
                None | Some(Value::Null) => false,
                Some(Value::String(name)) => !name.is_empty(),
                Some(_) => return Err(invalid("teamName must be a string")),
            };
            let node = Node {
                line,
                id: id.clone(),
                parent,
                parent_declared: entry.get("parentUuid").is_some(),
                message,
                sidechain: flag(&entry, "isSidechain")?,
                meta: flag(&entry, "isMeta")?,
                team,
            };
            by_id.insert(id, nodes.len());
            nodes.push(node);
        }
        let known_message_lines: HashSet<_> = nodes.iter()
            .filter(|node| node.message && node.parent_declared)
            .map(|node| node.line)
            .collect();
        for &line in &messages {
            if !known_message_lines.contains(&line) {
                // Only byte-identical duplicates may omit a separate node.
                let duplicate = duplicate_of.get(&line)
                    .is_some_and(|original| known_message_lines.contains(original));
                if !duplicate {
                    return Err(invalid("linked and unlinked message records cannot be combined"));
                }
            }
        }
        let parents: Vec<_> = nodes.iter().map(|node| {
            node.parent.as_ref().map(|id| by_id.get(id).copied()
                .ok_or_else(|| invalid("history references a missing parent UUID")))
                .transpose()
        }).collect::<Result<_>>()?;
        validate_graph(&parents)?;

        // A message is a leaf when no eligible main-chain message descends
        // from it. Progress/meta tails and sidechain forks cannot select an
        // obsolete ancestor or displace the main conversation. Each ancestry
        // edge is visited at most once across all candidate walks.
        let mut ancestors = vec![false; nodes.len()];
        for (index, node) in nodes.iter().enumerate() {
            if !node.message || node.sidechain || node.meta || node.team {
                continue;
            }
            let mut cursor = parents[index];
            while let Some(parent) = cursor {
                if ancestors[parent] { break; }
                ancestors[parent] = true;
                cursor = parents[parent];
            }
        }
        let candidates: Vec<_> = nodes.iter().enumerate()
            .filter(|(index, node)| node.message && !node.sidechain && !node.meta && !node.team && !ancestors[*index])
            .map(|(index, _)| index)
            .collect();
        self.branches = candidates.len();
        let leaf = if let Some(id) = selected {
            if id.is_empty() || id.len() > MAX_ID_BYTES || id.chars().any(char::is_control) {
                return Err(invalid("selected UUID must be a nonempty bounded string"));
            }
            by_id.get(id).copied()
                .filter(|&index| nodes[index].message && !nodes[index].meta)
                .ok_or_else(|| invalid("selected UUID is not an importable message"))?
        } else {
            candidates.into_iter().max_by_key(|&index| nodes[index].line)
                .ok_or_else(|| invalid("no main conversation leaf; select an explicit message UUID"))?
        };
        let mut active = Vec::new();
        let mut cursor = Some(leaf);
        while let Some(index) = cursor {
            let node = &nodes[index];
            if selected.is_none() && (node.sidechain || node.team) {
                return Err(invalid("main conversation ancestry crosses a subagent or team chain"));
            }
            if node.message && !node.meta {
                active.push(node.line);
            }
            cursor = parents[index];
        }
        active.reverse();
        let included: HashSet<_> = active.iter().copied().collect();
        self.excluded = messages.into_iter().filter(|line| !included.contains(line)).collect();
        // Metadata and excluded source bytes are archived first, followed by
        // active messages in ancestry order (parents may be later in the file).
        self.order = (0..self.lines.len()).filter(|line| !included.contains(line)).collect();
        self.order.extend(active);
        self.leaf = Some(nodes[leaf].id.clone());
        Ok(())
    }
}

struct UniqueRoot;

impl<'de> serde::Deserialize<'de> for UniqueRoot {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = UniqueRoot;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an object with unique fields")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> std::result::Result<UniqueRoot, A::Error> {
                let mut keys = HashSet::new();
                while let Some(key) = map.next_key::<String>()? {
                    if !keys.insert(key) {
                        return Err(serde::de::Error::custom("duplicate history field"));
                    }
                    map.next_value::<serde::de::IgnoredAny>()?;
                }
                Ok(UniqueRoot)
            }
        }
        deserializer.deserialize_map(Visitor)
    }
}

fn validate_graph(parents: &[Option<usize>]) -> Result<()> {
    let mut colors = vec![0_u8; parents.len()];
    for start in 0..parents.len() {
        let mut path = Vec::new();
        let mut cursor = Some(start);
        while let Some(index) = cursor {
            match colors[index] {
                2 => break,
                1 => return Err(invalid("history contains a parent cycle")),
                _ => {}
            }
            colors[index] = 1;
            path.push(index);
            cursor = parents[index];
        }
        for index in path { colors[index] = 2; }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
