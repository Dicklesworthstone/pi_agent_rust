//! Lossless conflict inspection and optimistic, staged resolution (bd-cv653.6.3).
//!
//! Git's unmerged index identifies files; exact byte spans identify regions.
//! Numbering is per snapshot. Tokens bind a choice to its complete file image,
//! path and region, not just to a mutable ordinal. Publication reuses the native
//! workspace-edit transaction, without staging Git's index or changing HEAD.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use super::{ConflictRegion, ResolvedDoc, URL_ROUTER_SCHEMA};
use crate::error::{Error, Result};
use crate::lsp::edits::{WorkspaceEditPlan, apply_workspace_edit};
use crate::lsp::text::{Range, TextEdit, content_hash_for_drift, offset_to_position};

mod io;
mod parser;
mod selection;

pub use selection::prepare_conflict_write;
pub use selection::{ConflictChoice, ConflictResolution, apply_conflict_choices};

const MAX_FILES: usize = 256;
const MAX_REGIONS: usize = 1024;
const MAX_TOTAL_BYTES: usize = 16 * 1024 * 1024;

fn fail(code: &str, message: &str) -> Error {
    Error::tool("conflict", format!("PI_CONFLICT_{code}: {message}"))
}

struct FileImage {
    relative: String,
    text: String,
    hash: [u8; 32],
}

struct Located {
    file: usize,
    region: parser::Region,
    token: String,
}

pub(crate) struct AppliedConflicts {
    pub regions: Vec<ConflictRegion>,
    pub changed_paths: Vec<PathBuf>,
}

struct Snapshot {
    root: PathBuf,
    files: Vec<FileImage>,
    regions: Vec<Located>,
    budget: io::Budget,
}

fn paths(bytes: &[u8]) -> Result<BTreeSet<String>> {
    if !bytes.is_empty() && !bytes.ends_with(b"\0") {
        return Err(fail("GIT", "unterminated unmerged-index record"));
    }
    let mut paths = BTreeSet::new();
    for record in bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let tab = record
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(|| fail("GIT", "invalid unmerged-index record"))?;
        let header = std::str::from_utf8(&record[..tab])
            .map_err(|_| fail("GIT", "invalid unmerged-index header"))?;
        let fields: Vec<_> = header.split(' ').collect();
        if fields.len() != 3 || !matches!(fields[2], "1" | "2" | "3") {
            return Err(fail("GIT", "invalid unmerged-index stage"));
        }
        let name = std::str::from_utf8(&record[tab + 1..]).map_err(|_| {
            fail(
                "ENCODING",
                "unmerged path is not UTF-8; no paths were guessed or skipped",
            )
        })?;
        if name.is_empty() {
            return Err(fail("PATH", "empty unmerged path"));
        }
        paths.insert(name.to_owned());
        if paths.len() > MAX_FILES {
            return Err(fail("LIMIT", "more than 256 unmerged files"));
        }
    }
    Ok(paths)
}

fn token(root: &Path, image: &FileImage, region: &parser::Region) -> String {
    let mut hash = Sha256::new();
    hash.update(b"pi.conflict.snapshot.v1\0");
    hash.update(root.as_os_str().as_encoded_bytes());
    hash.update(b"\0");
    hash.update(image.relative.as_bytes());
    hash.update(b"\0");
    hash.update(region.span.start.to_string().as_bytes());
    hash.update(b":");
    hash.update(region.span.end.to_string().as_bytes());
    hash.update(b"\0");
    hash.update(image.hash);
    crate::package_manager::hex_encode(&hash.finalize())
}

impl Snapshot {
    fn read(cwd: &Path) -> Result<Self> {
        let budget = io::Budget::new()?;
        let root = budget.git(cwd, &["rev-parse", "--show-toplevel"])?;
        let root = root.strip_suffix(b"\n").unwrap_or(&root);
        #[cfg(unix)]
        let root = {
            use std::os::unix::ffi::OsStrExt as _;
            PathBuf::from(std::ffi::OsStr::from_bytes(root))
        };
        #[cfg(not(unix))]
        let root = PathBuf::from(
            std::str::from_utf8(root)
                .map_err(|_| fail("ENCODING", "worktree root is not UTF-8"))?,
        );
        let root = std::fs::canonicalize(root)?;
        let listing = budget.git(&root, &["ls-files", "--unmerged", "-z", "--"])?;
        let mut snapshot = Self {
            root,
            files: Vec::new(),
            regions: Vec::new(),
            budget,
        };
        let mut bytes = 0_usize;
        for relative in paths(&listing)? {
            snapshot.budget.check()?;
            let text = io::read(&snapshot.root, Path::new(&relative))?;
            bytes = bytes
                .checked_add(text.len())
                .filter(|bytes| *bytes <= MAX_TOTAL_BYTES)
                .ok_or_else(|| fail("LIMIT", "conflict source images exceed 16 MiB"))?;
            let regions = parser::parse(&text, MAX_REGIONS.saturating_sub(snapshot.regions.len()))?;
            let file = snapshot.files.len();
            let hash: [u8; 32] = Sha256::digest(text.as_bytes()).into();
            let image = FileImage {
                relative,
                text,
                hash,
            };
            for region in regions {
                snapshot.regions.push(Located {
                    file,
                    token: token(&snapshot.root, &image, &region),
                    region,
                });
            }
            snapshot.files.push(image);
        }
        snapshot.budget.check()?;
        Ok(snapshot)
    }

    fn location(&self, index: usize) -> Result<&Located> {
        self.regions.get(index).ok_or_else(|| {
            fail(
                "NOT_FOUND",
                "no such conflict region in the current worktree; read conflict://* again",
            )
        })
    }

    fn region(&self, index: usize) -> Result<ConflictRegion> {
        let located = self.location(index)?;
        let image = &self.files[located.file];
        let region = &located.region;
        Ok(ConflictRegion {
            index,
            file: image.relative.clone(),
            ours_label: region.ours_label.clone(),
            theirs_label: region.theirs_label.clone(),
            ours: image.text[region.ours.clone()].to_owned(),
            theirs: image.text[region.theirs.clone()].to_owned(),
            base: region
                .base
                .as_ref()
                .map_or_else(String::new, |span| image.text[span.clone()].to_owned()),
        })
    }

    fn metadata(&self, index: usize) -> Result<Value> {
        let located = self.location(index)?;
        let mut metadata = serde_json::to_value(self.region(index)?)?;
        metadata["token"] = json!(located.token);
        metadata["hasBase"] = json!(located.region.base.is_some());
        Ok(metadata)
    }

    fn render(&self, index: usize) -> Result<String> {
        let region = self.region(index)?;
        let located = self.location(index)?;
        Ok(format!(
            "### conflict {} in {}\nsnapshot: {}:{}\n--- ours ({}) ---\n{}--- base ---\n{}--- theirs ({}) ---\n{}",
            index,
            json!(region.file),
            index,
            located.token,
            json!(region.ours_label),
            region.ours,
            if located.region.base.is_some() {
                region.base
            } else {
                "[no base section]\n".to_owned()
            },
            json!(region.theirs_label),
            region.theirs,
        ))
    }

    fn apply(&self, choices: &[(usize, &str)]) -> Result<Vec<ConflictRegion>> {
        let replacements = choices
            .iter()
            .map(|&(index, side)| {
                let located = self.location(index)?;
                let text = located
                    .region
                    .chosen(&self.files[located.file].text, side)?;
                Ok((index, text))
            })
            .collect::<Result<Vec<_>>>()?;
        self.apply_replacements(&replacements)
            .map(|applied| applied.regions)
    }

    fn apply_replacements(&self, choices: &[(usize, &str)]) -> Result<AppliedConflicts> {
        self.budget.check()?;
        let mut seen = BTreeSet::new();
        let mut plan = WorkspaceEditPlan::default();
        let mut expected = HashMap::new();
        let mut selected = Vec::with_capacity(choices.len());
        for &(index, replacement) in choices {
            if !seen.insert(index) {
                return Err(fail("DUPLICATE", "a region was selected more than once"));
            }
            let located = self.location(index)?;
            let image = &self.files[located.file];
            let region = &located.region;
            let path = self.root.join(&image.relative);
            let start = offset_to_position(&image.text, region.span.start)
                .ok_or_else(|| fail("RANGE", "conflict start is not a text boundary"))?;
            let end = offset_to_position(&image.text, region.span.end)
                .ok_or_else(|| fail("RANGE", "conflict end is not a text boundary"))?;
            plan.text_edits
                .entry(path.clone())
                .or_default()
                .push(TextEdit {
                    range: Range { start, end },
                    new_text: replacement.to_owned(),
                });
            expected.insert(path, content_hash_for_drift(&image.text));
            selected.push(self.region(index)?);
        }
        // Reject path replacement and byte drift before entering the existing
        // staged transaction. That transaction rechecks preimages at commit.
        for &index in &seen {
            self.budget.check()?;
            let located = self.location(index)?;
            let image = &self.files[located.file];
            if io::read(&self.root, Path::new(&image.relative))? != image.text {
                return Err(fail(
                    "STALE",
                    "conflict file changed after inspection; re-read it",
                ));
            }
        }
        self.budget.check()?;
        let applied = apply_workspace_edit(&plan, Some(&expected))?;
        Ok(AppliedConflicts {
            regions: selected,
            changed_paths: applied.changed_paths,
        })
    }
}

/// Enumerate text regions in stable path/byte order, relative to the Git root.
///
/// # Errors
/// Fail explicitly on malformed, binary, oversized or unsafe source paths.
pub fn conflict_regions(cwd: &Path) -> Result<Vec<ConflictRegion>> {
    let snapshot = Snapshot::read(cwd)?;
    (0..snapshot.regions.len())
        .map(|index| snapshot.region(index))
        .collect()
}

/// Resolve one current region while preserving all other bytes and permissions.
///
/// This legacy API takes a fresh snapshot; use token-checked choices to guard
/// an earlier read. No Git index entry is staged and no commit is created.
///
/// # Errors
/// Refuse unknown regions, absent bases, malformed markers and changed preimages.
pub fn write_conflict_resolution(cwd: &Path, index: usize, side: &str) -> Result<ConflictRegion> {
    let snapshot = Snapshot::read(cwd)?;
    snapshot
        .apply(&[(index, side)])?
        .into_iter()
        .next()
        .ok_or_else(|| fail("INTERNAL", "resolution produced no selected region"))
}

pub(super) fn resolve(reference: &str, cwd: &Path) -> Result<ResolvedDoc> {
    let selection = selection::parse_read(reference)?;
    let selector = selection.side;
    let snapshot = Snapshot::read(cwd)?;
    let (content, metadata) = if selection.index.is_none() {
        if selector != "full" {
            return Err(fail(
                "REFERENCE",
                "bulk conflict reads cannot select a side",
            ));
        }
        let mut text = format!(
            "{} text conflict region(s) in {} unmerged file(s).\n",
            snapshot.regions.len(),
            snapshot.files.len()
        );
        for index in 0..snapshot.regions.len() {
            text.push_str(&snapshot.render(index)?);
        }
        (
            text,
            json!({"count":snapshot.regions.len(),"unmergedFiles":snapshot.files.len(),"complete":true}),
        )
    } else {
        let index = selection
            .index
            .ok_or_else(|| fail("REFERENCE", "conflict index missing"))?;
        let located = snapshot.location(index)?;
        if selection.token.is_some_and(|token| token != located.token) {
            return Err(fail(
                "STALE",
                "conflict read token no longer matches; inspect the current inventory",
            ));
        }
        let content = if selector == "full" {
            snapshot.render(index)?
        } else {
            located
                .region
                .chosen(&snapshot.files[located.file].text, selector)?
                .to_owned()
        };
        let mut metadata = snapshot.metadata(index)?;
        metadata["selector"] = json!(selector);
        (content, metadata)
    };
    Ok(ResolvedDoc {
        schema: URL_ROUTER_SCHEMA.to_owned(),
        scheme: "conflict".to_owned(),
        reference: reference.to_owned(),
        content,
        content_type: "text/plain".to_owned(),
        metadata,
        line_addressable: true,
    })
}

#[cfg(test)]
mod tests;
