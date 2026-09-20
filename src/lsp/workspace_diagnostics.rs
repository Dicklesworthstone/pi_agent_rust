//! Active, bounded diagnostics for workspace-relative globs.
//!
//! This explicit scan supplements the server-free cached diagnostics view.
//! Discover matching regular files, synchronize each with its own server, and
//! retain explicit failures instead of translating missing reports to clean.
//! This is a sequence of document reports, not an atomic project snapshot.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{LspInput, LspTool, MAX_PAYLOAD_BYTES, Result, ToolOutput, text_output, tool_err};
use crate::agent_cx::AgentCx;

const MAX_FILES: usize = 256;
const MAX_WALK_ENTRIES: usize = 50_000;
const MAX_DEPTH: usize = 64;
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_SOURCE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_PATH_BYTES: usize = 4096;
// Includes two escaped, bounded cursor paths, the glob and fixed metadata.
const RESULT_RESERVE: usize = 64 * 1024;

fn checkpoint(owner: &AgentCx) -> Result<()> {
    owner
        .checkpoint()
        .map_err(|_| tool_err("LSP_CANCELLED", "workspace diagnostics cancelled"))?;
    if !owner.capabilities().io {
        return Err(tool_err(
            "LSP_IO_PERMISSION",
            "workspace diagnostics requires filesystem I/O",
        ));
    }
    Ok(())
}

fn matcher(pattern: &str) -> Result<globset::GlobMatcher> {
    // Forward slashes give the same glob semantics on Unix and Windows.
    // Reject traversal, negation and native drive prefixes instead of treating
    // them as an empty workspace scan. Exact absolute file requests stay valid.
    if pattern.is_empty()
        || pattern.len() > MAX_PATH_BYTES
        || pattern.starts_with('/')
        || pattern.starts_with('!')
        || pattern.contains(['\\', ':'])
        || pattern.chars().any(char::is_control)
        || pattern.split('/').any(|part| part == "..")
    {
        return Err(tool_err(
            "LSP_USAGE",
            "diagnostic globs must be positive workspace-relative paths without traversal",
        ));
    }
    globset::GlobBuilder::new(pattern.strip_prefix("./").unwrap_or(pattern))
        .literal_separator(true)
        .backslash_escape(false)
        .build()
        .map(|glob| glob.compile_matcher())
        .map_err(|error| tool_err("LSP_USAGE", format!("invalid diagnostic glob: {error}")))
}

#[derive(Default)]
struct Discovery {
    files: BTreeSet<String>,
    matched: usize,
    eligible: usize,
    visited: usize,
    errors: usize,
    stop: Option<&'static str>,
}

fn discover(
    root: &Path,
    glob: &globset::GlobMatcher,
    limit: usize,
    after: Option<&str>,
    owner: &AgentCx,
    started: Instant,
    timeout: Duration,
) -> Result<Discovery> {
    checkpoint(owner)?;
    let mut found = Discovery::default();
    let mut walker = ignore::WalkBuilder::new(root)
        .follow_links(false)
        .parents(false)
        .git_global(false)
        .require_git(false)
        .max_depth(Some(MAX_DEPTH))
        .build();
    // Do not sort each directory: a single enormous directory would otherwise
    // have to be collected before any visit/time/cancellation limit is checked.
    loop {
        checkpoint(owner)?;
        if started.elapsed() >= timeout {
            found.stop = Some("timeout");
            break;
        }
        if found.visited >= MAX_WALK_ENTRIES {
            found.stop = Some("walk_limit");
            break;
        }
        let Some(entry) = walker.next() else {
            break;
        };
        checkpoint(owner)?;
        if started.elapsed() >= timeout {
            found.stop = Some("timeout");
            break;
        }
        found.visited += 1;
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                found.errors += 1;
                continue;
            }
        };
        if entry.error().is_some() {
            found.errors += 1;
        }
        if entry.file_type().is_some_and(|kind| kind.is_dir()) && entry.depth() >= MAX_DEPTH {
            found.stop.get_or_insert("depth_limit");
        }
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(root) else {
            found.errors += 1;
            continue;
        };
        if !glob.is_match(relative) {
            continue;
        }
        let Some(relative) = relative.to_str() else {
            found.errors += 1;
            continue;
        };
        if relative.len() > MAX_PATH_BYTES {
            found.errors += 1;
            continue;
        }
        let relative = relative.replace(std::path::MAIN_SEPARATOR, "/");
        found.matched += 1;
        if after.is_some_and(|after| relative.as_str() <= after) {
            continue;
        }
        found.eligible += 1;
        found.files.insert(relative);
        // Keep a bounded, deterministic first page even with unordered walks.
        if found.files.len() > limit {
            found.files.pop_last();
        }
    }
    if found.eligible > limit {
        found.stop.get_or_insert("file_limit");
    }
    Ok(found)
}

fn scoped_file(root: &Path, relative: &str) -> Result<(PathBuf, u64)> {
    let path = root.join(relative);
    if Path::new(relative)
        .components()
        .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(tool_err("LSP_FILE_SCOPE", "invalid discovered path"));
    }
    // Recheck the route immediately before reading. A directory or file that
    // became a symlink since discovery must not redirect this scan elsewhere.
    let canonical = path.canonicalize()?;
    let metadata = std::fs::symlink_metadata(&path)?;
    if canonical != path || !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(tool_err(
            "LSP_FILE_SCOPE",
            "diagnostic source is no longer a scoped regular file",
        ));
    }
    if metadata.len() > MAX_FILE_BYTES {
        return Err(tool_err(
            "LSP_FILE_LIMIT",
            "diagnostic source exceeds 8 MiB",
        ));
    }
    Ok((canonical, metadata.len()))
}

fn read_source(root: &Path, relative: &str) -> Result<String> {
    let (path, _) = scoped_file(root, relative)?;
    let mut text = String::new();
    std::fs::File::open(path)?
        .take(MAX_FILE_BYTES + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > MAX_FILE_BYTES {
        return Err(tool_err(
            "LSP_FILE_LIMIT",
            "diagnostic source grew beyond 8 MiB",
        ));
    }
    Ok(text)
}

fn disk_hash(root: &Path, relative: &str) -> Result<u64> {
    read_source(root, relative).map(|text| super::text::content_hash_for_drift(&text))
}

fn short_error(error: &crate::error::Error) -> String {
    error.to_string().chars().take(512).collect()
}

struct ByteBudget(usize);
impl Write for ByteBudget {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_sub(bytes.len())
            .ok_or_else(|| std::io::Error::other("diagnostic output limit"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn size_within(value: &Value, limit: usize) -> Option<usize> {
    let mut budget = ByteBudget(limit);
    serde_json::to_writer(&mut budget, value).ok()?;
    Some(limit - budget.0)
}

impl LspTool {
    async fn check_workspace_document(
        &self,
        root: &Path,
        relative: &str,
        before: u64,
        wait: Duration,
    ) -> Result<(Value, u64)> {
        let (path, _) = scoped_file(root, relative)?;
        let (uri, entry) = self.synced(&path).await?;
        let source = entry.client.synchronized_text(&uri).ok_or_else(|| {
            tool_err(
                "LSP_DIAGNOSTIC_STALE",
                "diagnostic source was not synchronized",
            )
        })?;
        if super::text::content_hash_for_drift(&source) != before {
            return Err(tool_err(
                "LSP_DIAGNOSTIC_STALE",
                "source changed during synchronization",
            ));
        }
        let diagnostics = entry.client.document_diagnostics(&uri, wait).await?;
        if disk_hash(root, relative)? != before {
            return Err(tool_err(
                "LSP_DIAGNOSTIC_STALE",
                "source changed while obtaining diagnostics",
            ));
        }
        Ok((
            json!({"file":relative,"server":entry.spec_name,"status":"checked",
            "count":diagnostics.len(),"diagnostics":diagnostics}),
            before,
        ))
    }

    pub(super) async fn run_workspace_diagnostics(
        &self,
        input: &LspInput,
        pattern: &str,
    ) -> Result<ToolOutput> {
        let glob = matcher(pattern)?;
        if let Some(after) = input.after.as_deref() {
            if after.is_empty()
                || after.len() > MAX_PATH_BYTES
                || after.contains('\0')
                || Path::new(after).is_absolute()
                || Path::new(after)
                    .components()
                    .any(|part| !matches!(part, Component::Normal(_)))
                || after.split('/').any(|part| matches!(part, "" | "." | ".."))
            {
                return Err(tool_err(
                    "LSP_USAGE",
                    "after must be a bounded workspace-relative cursor path",
                ));
            }
        }
        if input.limit == Some(0) {
            return Err(tool_err(
                "LSP_USAGE",
                "diagnostic file limit must be positive",
            ));
        }
        let limit = input.limit.unwrap_or(100).min(MAX_FILES);
        let timeout = Duration::from_secs(
            input
                .timeout
                .filter(|seconds| *seconds > 0)
                .unwrap_or(30)
                .min(120),
        );
        let started = Instant::now();
        let owner = AgentCx::for_current_or_request();
        checkpoint(&owner)?;
        let root = self.cwd.canonicalize()?;
        let found = discover(
            &root,
            &glob,
            limit,
            input.after.as_deref(),
            &owner,
            started,
            timeout,
        )?;
        let mut stop = found.stop;
        let mut entries = Vec::new();
        let mut guards = Vec::new();
        let mut remaining_bytes = MAX_PAYLOAD_BYTES - RESULT_RESERVE;
        let mut source_bytes = 0_u64;
        let mut output_truncated = false;
        for relative in &found.files {
            checkpoint(&owner)?;
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                stop = Some("timeout");
                break;
            }
            let mut guard = None;
            let result = if self.registry.spec_for_file(&root.join(relative)).is_none() {
                Err(tool_err(
                    "LSP_NO_SERVER",
                    "no configured language server for this file",
                ))
            } else {
                match read_source(&root, relative) {
                    Ok(source) => {
                        let bytes = source.len() as u64;
                        if source_bytes.saturating_add(bytes) > MAX_SOURCE_BYTES {
                            stop = Some("source_limit");
                            break;
                        }
                        source_bytes += bytes;
                        let hash = super::text::content_hash_for_drift(&source);
                        drop(source);
                        let now = owner
                            .cx()
                            .timer_driver()
                            .map_or_else(asupersync::time::wall_now, |timer| timer.now());
                        match asupersync::time::timeout(
                            now,
                            remaining,
                            self.check_workspace_document(
                                &root,
                                relative,
                                hash,
                                remaining.min(Duration::from_secs(5)),
                            ),
                        )
                        .await
                        {
                            Ok(result) => result,
                            Err(_) => Err(tool_err(
                                "LSP_TIMEOUT",
                                "workspace diagnostic budget expired",
                            )),
                        }
                    }
                    Err(error) => Err(error),
                }
            };
            checkpoint(&owner)?;
            let mut row = match result {
                Ok((row, hash)) => {
                    guard = Some(hash);
                    row
                }
                Err(error) => json!({"file":relative,"status":"error","error":short_error(&error)}),
            };
            if size_within(&row, remaining_bytes).is_none() {
                output_truncated = true;
                guard = None;
                row = json!({"file":relative,"status":"output_limit","count":row.get("count"),
                    "error":"Report omitted to preserve the structured output limit; request this file individually."});
            }
            let Some(bytes) = size_within(&row, remaining_bytes) else {
                stop = Some("output_limit");
                break;
            };
            remaining_bytes = remaining_bytes.saturating_sub(bytes + 1);
            if let Some(hash) = guard {
                guards.push((entries.len(), relative, hash));
            }
            entries.push(row);
        }
        if started.elapsed() >= timeout {
            stop = Some("timeout");
        }
        // Another file's analysis can change an earlier source. Do not label
        // those earlier diagnostics current merely because their call succeeded.
        for (index, relative, expected) in guards {
            checkpoint(&owner)?;
            if !disk_hash(&root, relative).is_ok_and(|actual| actual == expected) {
                entries[index] = json!({"file":relative,"status":"error",
                    "error":"LSP_DIAGNOSTIC_STALE"});
            }
        }
        let checked = entries
            .iter()
            .filter(|row| row["status"] == "checked")
            .count();
        let errors = entries
            .iter()
            .filter(|row| row["status"] == "error")
            .count();
        // A cursor is sound only after complete discovery: otherwise unvisited
        // paths may sort before the last emitted name and be skipped forever.
        let discovery_complete =
            found.errors == 0 && matches!(found.stop, None | Some("file_limit"));
        let has_more = discovery_complete.then_some(found.eligible > entries.len());
        let next_after = if has_more == Some(true) {
            entries.last().and_then(|entry| entry["file"].as_str())
        } else {
            None
        };
        let page_complete = discovery_complete
            && !output_truncated
            && matches!(stop, None | Some("file_limit"))
            && entries.len() == found.files.len()
            && checked == entries.len();
        let complete = input.after.is_none() && page_complete && has_more == Some(false);
        let payload = json!({"action":"workspace_diagnostics","glob":pattern,"cachedOnly":false,
            "complete":complete,"stopReason":stop,"outputTruncated":output_truncated,
            "after":input.after,"nextAfter":next_after,"hasMore":has_more,
            "pageComplete":page_complete,"discoveryComplete":discovery_complete,
            "files":entries.len(),"checkedFiles":checked,"failedFiles":errors,
            "matchedFiles":found.matched,"visitedEntries":found.visited,"discoveryErrors":found.errors,
            "remainingMatchedFiles":found.eligible,
            "entries":entries,"fileLimit":limit,
            "note":"Document reports for nonignored regular files under cwd, not an atomic project snapshot. Continuation pages never claim whole-workspace completeness; files added before a cursor need a fresh scan. Unversioned server pushes cannot prove server-side freshness. Missing or failed reports are not clean files."});
        // A final size guard includes metadata and replacements, not only rows.
        if size_within(&payload, MAX_PAYLOAD_BYTES).is_none() {
            return Err(tool_err(
                "LSP_DIAGNOSTIC_LIMIT",
                "workspace diagnostic output exceeds its byte limit",
            ));
        }
        let mut output = text_output(payload.to_string(), payload);
        output.is_error = errors > 0 || found.errors > 0;
        Ok(output)
    }
}

#[cfg(test)]
mod tests;
