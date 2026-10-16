//! Explicit, snapshot-bound choices. No path or command comes from a choice.

use std::path::Path;
use serde::{Deserialize, Serialize};
use super::{ConflictRegion, Result, Snapshot, fail};

const MAX_CHOICES: usize = 64;
const MAX_REPLACEMENT_BYTES: usize = 64 * 1024;
const MAX_REPLACEMENTS_BYTES: usize = 1024 * 1024;

/// One side or an exact authored replacement. Empty text is a deliberate deletion.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "camelCase", deny_unknown_fields)]
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
    token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn validate(choices: &[ConflictChoice]) -> Result<()> {
    if choices.is_empty() || choices.len() > MAX_CHOICES {
        return Err(fail("CHOICES", "select 1-64 conflict regions"));
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut total = 0_usize;
    for choice in choices {
        if !valid_token(&choice.token) {
            return Err(fail("TOKEN", "each choice needs its lowercase SHA-256 token from read"));
        }
        if !seen.insert(choice.index) {
            return Err(fail("DUPLICATE", "a region was selected more than once"));
        }
        if let ConflictResolution::Text(text) = &choice.resolution {
            total = total.checked_add(text.len()).filter(|total| *total <= MAX_REPLACEMENTS_BYTES)
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
pub fn apply_conflict_choices(cwd: &Path, choices: &[ConflictChoice]) -> Result<Vec<ConflictRegion>> {
    validate(choices)?;
    let snapshot = Snapshot::read(cwd)?;
    let mut replacements = Vec::with_capacity(choices.len());
    for choice in choices {
        snapshot.budget.check()?;
        let located = snapshot.location(choice.index)?;
        if located.token != choice.token {
            return Err(fail("STALE", "a conflict choice no longer matches its read snapshot; re-read all choices"));
        }
        let source = &snapshot.files[located.file].text;
        let text = match &choice.resolution {
            ConflictResolution::Ours => located.region.chosen(source, "ours")?,
            ConflictResolution::Theirs => located.region.chosen(source, "theirs")?,
            ConflictResolution::Base => located.region.chosen(source, "base")?,
            ConflictResolution::Text(text) => text.as_str(),
        };
        replacements.push((choice.index, text));
    }
    snapshot.apply_replacements(&replacements)
}

pub(super) struct ReadReference<'a> {
    pub(super) index: Option<usize>,
    pub(super) side: &'a str,
    pub(super) token: Option<&'a str>,
}

/// Preserve the older space/@ spelling and add OMP slash-side views. An
/// optional token gives SDK consumers a read of exactly the inspected image.
pub(super) fn parse_read(reference: &str) -> Result<ReadReference<'_>> {
    if reference.len() > 160 { return Err(fail("REFERENCE", "conflict reference is too long")); }
    let (path, legacy_side) = reference.split_once(' ').map_or((reference, None), |(path, side)| {
        (path, Some(side.trim().strip_prefix('@').unwrap_or(side.trim())))
    });
    let parts: Vec<_> = path.split('/').collect();
    let (index, token, side) = match parts.as_slice() {
        [index] => (*index, None, legacy_side.unwrap_or("full")),
        [index, side] if matches!(*side, "ours" | "theirs" | "base") && legacy_side.is_none() => {
            (*index, None, *side)
        }
        [index, token] if valid_token(token) => (*index, Some(*token), legacy_side.unwrap_or("full")),
        [index, token, side] if valid_token(token) && legacy_side.is_none() => (*index, Some(*token), *side),
        _ => return Err(fail("REFERENCE", "use conflict://N, N/side or N/token[/side]")),
    };
    if !matches!(side, "full" | "ours" | "theirs" | "base") {
        return Err(fail("SIDE", "choose ours, theirs or base"));
    }
    let index = if matches!(index, "" | "*") {
        if token.is_some() || side != "full" {
            return Err(fail("REFERENCE", "bulk reads cannot select a token or side"));
        }
        None
    } else {
        if !index.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(fail("REFERENCE", "conflict index must be a nonnegative integer"));
        }
        Some(index.parse().map_err(|_| fail("REFERENCE", "conflict index is out of range"))?)
    };
    Ok(ReadReference { index, side, token })
}

#[cfg(test)]
mod tests;
