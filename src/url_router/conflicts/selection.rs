//! Explicit, snapshot-bound choices. No path or command comes from a choice.

use super::{ConflictRegion, Result, Snapshot, fail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

const MAX_CHOICES: usize = 64;
const MAX_REPLACEMENT_BYTES: usize = 64 * 1024;
const MAX_REPLACEMENTS_BYTES: usize = 1024 * 1024;

/// One side or an exact authored replacement. Empty text is a deliberate deletion.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "camelCase",
    deny_unknown_fields
)]
pub enum ConflictResolution {
    Ours,
    Theirs,
    Base,
    Text(String),
}

/// A choice addresses an ordinal only under its read-time SHA-256 snapshot token.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConflictChoice {
    pub index: usize,
    pub token: String,
    pub resolution: ConflictResolution,
}

fn valid_token(token: &str) -> bool {
    token.len() == 64
        && token
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn validate(choices: &[ConflictChoice]) -> Result<()> {
    if choices.is_empty() || choices.len() > MAX_CHOICES {
        return Err(fail("CHOICES", "select 1-64 conflict regions"));
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut total = 0_usize;
    for choice in choices {
        if !valid_token(&choice.token) {
            return Err(fail(
                "TOKEN",
                "each choice needs its lowercase SHA-256 token from read",
            ));
        }
        if !seen.insert(choice.index) {
            return Err(fail("DUPLICATE", "a region was selected more than once"));
        }
        if let ConflictResolution::Text(text) = &choice.resolution {
            total = total
                .checked_add(text.len())
                .filter(|total| *total <= MAX_REPLACEMENTS_BYTES)
                .ok_or_else(|| fail("LIMIT", "authored replacements exceed 1 MiB"))?;
            if text.len() > MAX_REPLACEMENT_BYTES {
                return Err(fail("LIMIT", "an authored replacement exceeds 64 KiB"));
            }
        }
    }
    Ok(())
}

/// Validate every selection against one fresh inventory, then stage all selected
/// replacements together. The existing transaction owns rollback and recovery.
/// No Git index update, implicit commit, whole-file replacement or replay occurs.
///
/// # Errors
/// Invalid/duplicate/stale choices and absent base sections fail before mutation.
/// Native transaction errors retain their original rollback/recovery taxonomy.
pub fn apply_conflict_choices(
    cwd: &Path,
    choices: &[ConflictChoice],
) -> Result<Vec<ConflictRegion>> {
    validate(choices)?;
    PreparedConflicts::new(cwd, choices.to_vec())?.apply()
}

/// One validated inventory shared by scope checks, undo capture and staging.
/// Preparing never mutates a file or the Git index.
pub(crate) struct PreparedConflicts {
    snapshot: Snapshot,
    choices: Vec<ConflictChoice>,
}

impl PreparedConflicts {
    fn new(cwd: &Path, choices: Vec<ConflictChoice>) -> Result<Self> {
        validate(&choices)?;
        let snapshot = Snapshot::read(cwd)?;
        for choice in &choices {
            snapshot.budget.check()?;
            let located = snapshot.location(choice.index)?;
            if located.token != choice.token {
                return Err(fail(
                    "STALE",
                    "a conflict choice no longer matches its read snapshot; re-read all choices",
                ));
            }
            let source = &snapshot.files[located.file].text;
            // Reject every unavailable side before recording any undo images.
            // An absent base is not an empty replacement.
            match &choice.resolution {
                ConflictResolution::Ours => {
                    located.region.chosen(source, "ours")?;
                }
                ConflictResolution::Theirs => {
                    located.region.chosen(source, "theirs")?;
                }
                ConflictResolution::Base => {
                    located.region.chosen(source, "base")?;
                }
                ConflictResolution::Text(_) => {}
            }
        }
        Ok(Self { snapshot, choices })
    }

    /// Exact selected paths, not the enclosing Git root. Opening a session in
    /// a subdirectory must not grant writes to the rest of its repository.
    pub(crate) fn paths(&self) -> Vec<PathBuf> {
        self.choices
            .iter()
            .map(|choice| {
                let located = &self.snapshot.regions[choice.index];
                self.snapshot
                    .root
                    .join(&self.snapshot.files[located.file].relative)
            })
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    pub(crate) fn apply(self) -> Result<Vec<ConflictRegion>> {
        let replacements = self
            .choices
            .iter()
            .map(|choice| {
                let located = self.snapshot.location(choice.index)?;
                let source = &self.snapshot.files[located.file].text;
                let text = match &choice.resolution {
                    ConflictResolution::Ours => located.region.chosen(source, "ours")?,
                    ConflictResolution::Theirs => located.region.chosen(source, "theirs")?,
                    ConflictResolution::Base => located.region.chosen(source, "base")?,
                    ConflictResolution::Text(text) => text.as_str(),
                };
                Ok((choice.index, text))
            })
            .collect::<Result<Vec<_>>>()?;
        self.snapshot.apply_replacements(&replacements)
    }
}

/// Write accepts exact authored text at N/token, or a closed typed batch at *.
/// Content is always literal, including "@theirs". Invalid shapes and payload
/// limits are checked before invoking Git.
pub(crate) fn prepare_conflict_write(cwd: &Path, value: &Value) -> Result<PreparedConflicts> {
    let input = value
        .as_object()
        .ok_or_else(|| fail("INPUT", "write input must be an object"))?;
    let reference = input
        .get("path")
        .and_then(Value::as_str)
        .and_then(|path| path.strip_prefix("conflict://"))
        .ok_or_else(|| fail("REFERENCE", "a conflict write needs a conflict:// path"))?;
    let choices = if reference == "*" {
        if input.len() != 2 || !input.contains_key("conflicts") {
            return Err(fail(
                "INPUT",
                "conflict://* requires only path and conflicts; omit content",
            ));
        }
        let raw = input["conflicts"]
            .as_array()
            .ok_or_else(|| fail("CHOICES", "conflicts must be an array of typed choices"))?;
        if raw.is_empty() || raw.len() > MAX_CHOICES {
            return Err(fail("CHOICES", "select 1-64 conflict regions"));
        }
        let mut total = 0_usize;
        for choice in raw {
            if let Some(text) = choice.pointer("/resolution/value").and_then(Value::as_str) {
                total = total
                    .checked_add(text.len())
                    .filter(|total| *total <= MAX_REPLACEMENTS_BYTES)
                    .ok_or_else(|| fail("LIMIT", "authored replacements exceed 1 MiB"))?;
                if text.len() > MAX_REPLACEMENT_BYTES {
                    return Err(fail("LIMIT", "an authored replacement exceeds 64 KiB"));
                }
            }
        }
        Vec::<ConflictChoice>::deserialize(&input["conflicts"])
            .map_err(|error| fail("CHOICES", &format!("invalid conflict choices: {error}")))?
    } else {
        if input.len() != 2 || !input.contains_key("content") {
            return Err(fail(
                "INPUT",
                "a single conflict write requires only path and content",
            ));
        }
        if reference.split('/').count() != 2 || reference.chars().any(char::is_whitespace) {
            return Err(fail(
                "REFERENCE",
                "write conflict://N/token using the token returned by read; side views are read-only",
            ));
        }
        let selection = parse_read(reference)?;
        let index = selection
            .index
            .ok_or_else(|| fail("REFERENCE", "select a conflict region"))?;
        let token = selection
            .token
            .ok_or_else(|| fail("TOKEN", "a conflict write needs its token from read"))?;
        if selection.side != "full" {
            return Err(fail("REFERENCE", "conflict side views are read-only"));
        }
        let content = input["content"]
            .as_str()
            .ok_or_else(|| fail("INPUT", "content must be a string"))?;
        if content.len() > MAX_REPLACEMENT_BYTES {
            return Err(fail("LIMIT", "an authored replacement exceeds 64 KiB"));
        }
        vec![ConflictChoice {
            index,
            token: token.to_owned(),
            resolution: ConflictResolution::Text(content.to_owned()),
        }]
    };
    PreparedConflicts::new(cwd, choices)
}

pub(super) struct ReadReference<'a> {
    pub(super) index: Option<usize>,
    pub(super) side: &'a str,
    pub(super) token: Option<&'a str>,
}

/// Preserve the older space/@ spelling and add OMP slash-side views. An
/// optional token gives SDK consumers a read of exactly the inspected image.
pub(super) fn parse_read(reference: &str) -> Result<ReadReference<'_>> {
    if reference.len() > 160 {
        return Err(fail("REFERENCE", "conflict reference is too long"));
    }
    let (path, legacy_side) =
        reference
            .split_once(' ')
            .map_or((reference, None), |(path, side)| {
                (
                    path,
                    Some(side.trim().strip_prefix('@').unwrap_or(side.trim())),
                )
            });
    let parts: Vec<_> = path.split('/').collect();
    let (index, token, side) = match parts.as_slice() {
        [index] => (*index, None, legacy_side.unwrap_or("full")),
        [index, side] if matches!(*side, "ours" | "theirs" | "base") && legacy_side.is_none() => {
            (*index, None, *side)
        }
        [index, token] if valid_token(token) => {
            (*index, Some(*token), legacy_side.unwrap_or("full"))
        }
        [index, token, side] if valid_token(token) && legacy_side.is_none() => {
            (*index, Some(*token), *side)
        }
        _ => {
            return Err(fail(
                "REFERENCE",
                "use conflict://N, N/side or N/token[/side]",
            ));
        }
    };
    if !matches!(side, "full" | "ours" | "theirs" | "base") {
        return Err(fail("SIDE", "choose ours, theirs or base"));
    }
    let index = if matches!(index, "" | "*") {
        if token.is_some() || side != "full" {
            return Err(fail(
                "REFERENCE",
                "bulk reads cannot select a token or side",
            ));
        }
        None
    } else {
        if !index.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(fail(
                "REFERENCE",
                "conflict index must be a nonnegative integer",
            ));
        }
        Some(
            index
                .parse()
                .map_err(|_| fail("REFERENCE", "conflict index is out of range"))?,
        )
    };
    Ok(ReadReference { index, side, token })
}

#[cfg(test)]
mod tests;
