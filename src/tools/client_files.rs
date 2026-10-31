//! Host-owned text buffers for native file tools. The transport stays outside
//! tools; local confinement, transformations and output bounds stay here.

use super::*;
use std::sync::RwLock;

pub(crate) const MAX_CLIENT_TEXT_BYTES: usize = 10 * 1024 * 1024;

/// A session's editor filesystem. An error never authorizes a disk fallback.
#[async_trait]
pub trait TextFileClient: Send + Sync {
    fn can_read(&self) -> bool;
    fn can_write(&self) -> bool;
    /// Internal artifacts and paths outside the client's workspace stay local.
    fn handles_path(&self, path: &Path) -> bool;
    async fn read_text_file(&self, path: &Path) -> Result<String>;
    async fn write_text_file(&self, path: &Path, content: &str) -> Result<()>;
}

/// Shared only by one owning registry and its published snapshots.
#[derive(Clone, Default)]
pub struct TextFileClientScope(Arc<RwLock<Option<Arc<dyn TextFileClient>>>>);

impl TextFileClientScope {
    pub(crate) fn set(&self, client: Arc<dyn TextFileClient>) -> Result<()> {
        *self
            .0
            .write()
            .map_err(|_| Error::session("Text file client scope unavailable"))? = Some(client);
        Ok(())
    }

    fn for_path(&self, path: &Path) -> Result<Option<Arc<dyn TextFileClient>>> {
        let client = self
            .0
            .read()
            .map_err(|_| Error::session("Text file client scope unavailable"))?;
        Ok(client
            .as_ref()
            .filter(|client| client.handles_path(path))
            .cloned())
    }
}

fn bounded_text(content: String, tool: &'static str) -> Result<String> {
    if content.len() > MAX_CLIENT_TEXT_BYTES {
        return Err(Error::tool(
            tool,
            "Editor text exceeds the 10 MiB file limit",
        ));
    }
    if content.contains('\0') {
        return Err(Error::tool(
            tool,
            "Editor returned binary content for a text file",
        ));
    }
    Ok(content)
}

fn text_read_candidate(input: &ReadInput, path: &Path) -> bool {
    if !input.raw && crate::document_read::recognize(&path.to_string_lossy(), "").is_some() {
        return false;
    }
    !matches!(
        path.extension()
            .and_then(OsStr::to_str)
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some(
            "png"
                | "jpg"
                | "jpeg"
                | "gif"
                | "webp"
                | "bmp"
                | "ico"
                | "tif"
                | "tiff"
                | "pdf"
                | "zip"
                | "gz"
                | "xz"
                | "7z"
                | "docx"
                | "pptx"
                | "xlsx"
                | "epub"
                | "sqlite"
                | "sqlite3"
                | "db"
                | "mp4"
                | "mov"
                | "mkv"
                | "avi"
                | "webm"
                | "mp3"
                | "wav"
                | "ogg"
                | "flac"
                | "woff"
                | "woff2"
                | "ttf"
        )
    )
}

impl ReadTool {
    pub(super) async fn execute_client_read(
        &self,
        input: &ReadInput,
        path: &Path,
    ) -> Result<Option<ToolOutput>> {
        let Some(client) = self.text_file_client.for_path(path)? else {
            return Ok(None);
        };
        if !client.can_read() || !text_read_candidate(input, path) {
            return Ok(None);
        }
        let source = bounded_text(client.read_text_file(path).await?, "read")?;
        if !input.raw && crate::notebook::is_notebook_path(path) {
            let display_path = path.to_string_lossy().into_owned();
            let request = input.clone();
            return asupersync::runtime::spawn_blocking(move || {
                notebook_source_read_output(
                    &request,
                    source.as_bytes(),
                    serde_json::json!({"source": "editor", "path": display_path}),
                )
            })
            .await
            .map(Some);
        }
        let content = normalize_to_lf(&source);
        // Ask for the complete buffer and apply the native window exactly once.
        // Never key an editor buffer's cache entry on stale disk metadata.
        let total = content.lines().count();
        let start = usize::try_from(input.offset.unwrap_or(1))
            .unwrap_or(1)
            .max(1);
        if (total == 0 && input.offset.unwrap_or(0) > 0) || start > total.max(1) {
            return Err(Error::tool(
                "read",
                format!("Offset {start} is beyond end of file ({total} lines total)"),
            ));
        }
        let limit = input
            .limit
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(DEFAULT_MAX_LINES);
        let width = start
            .saturating_add(limit)
            .saturating_sub(1)
            .min(total)
            .to_string()
            .len()
            .max(5);
        let mut text = String::new();
        let mut shown = 0;
        for (index, line) in content.lines().enumerate().skip(start - 1).take(limit) {
            let prefix = if input.hashline {
                format!("{}:", format_hashline_tag(index, line))
            } else {
                format!("{:>width$}→", index + 1)
            };
            let size = line
                .len()
                .saturating_add(prefix.len())
                .saturating_add(usize::from(shown > 0));
            if size > DEFAULT_MAX_BYTES.saturating_sub(text.len()) {
                if shown == 0 {
                    return Err(Error::tool(
                        "read",
                        "Editor buffer line exceeds the file tool output limit",
                    ));
                }
                break;
            }
            if shown > 0 {
                text.push('\n');
            }
            text.push_str(&prefix);
            text.push_str(line);
            shown += 1;
        }
        let end = start - 1 + shown;
        if end < total {
            let _ = write!(
                text,
                "\n\n[Showing lines {start}-{end} of {total}. Use offset={} to continue.]",
                end + 1
            );
        }
        Ok(Some(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(text))],
            details: Some(
                serde_json::json!({"source":"editor", "totalLines":total, "offset":start,
                "outputLines":shown, "truncated":end < total}),
            ),
            is_error: false,
        }))
    }
}

async fn read_source(
    client: &dyn TextFileClient,
    path: &Path,
    roots: &[PathBuf],
    tool: &'static str,
) -> Result<String> {
    if client.can_read() {
        return bounded_text(client.read_text_file(path).await?, tool);
    }
    let path = path.to_path_buf();
    let roots = roots.to_vec();
    let bytes = asupersync::runtime::spawn_blocking_io(move || {
        read_file_capped_within_roots_sync(&path, &roots, MAX_CLIENT_TEXT_BYTES as u64)
    })
    .await
    .map_err(|error| Error::tool(tool, format!("Cannot read source file: {error}")))?;
    let content =
        String::from_utf8(bytes).map_err(|_| Error::tool(tool, "File is not valid UTF-8"))?;
    bounded_text(content, tool)
}

struct ClientWrite {
    verified: Option<String>,
    editor: bool,
}

async fn write_client(
    client: &dyn TextFileClient,
    path: &Path,
    content: &str,
    tool: &'static str,
) -> Result<ClientWrite> {
    if content.len() > MAX_CLIENT_TEXT_BYTES {
        return Err(Error::tool(
            tool,
            "Editor text exceeds the 10 MiB file limit",
        ));
    }
    client.write_text_file(path, content).await?;
    // Acknowledgment means the editor accepted the mutation. A failed
    // verification must not imply it was rolled back or invite an unsafe retry.
    let verified = if client.can_read() {
        match client.read_text_file(path).await {
            Ok(text) => bounded_text(text, tool).ok(),
            Err(_) => None,
        }
    } else {
        None
    };
    Ok(ClientWrite {
        verified,
        editor: true,
    })
}

fn mutation_output(
    tool: &'static str,
    display_path: &str,
    before: Option<&str>,
    requested: &str,
    written: ClientWrite,
    notebook_edit: bool,
) -> ToolOutput {
    let mut details = serde_json::json!({"destination":if written.editor {"editor"} else {"disk"},
        "verified":written.verified.is_some(),
        "changed":before.zip(written.verified.as_deref()).map(|(before, after)| before != after)});
    let mut text = if written.editor {
        format!("Editor acknowledged {tool} for {display_path}.")
    } else {
        format!("Successfully applied {tool} to {display_path}; disk matched the editor preimage.")
    };
    if let Some(actual) = written.verified.as_deref() {
        if actual != requested {
            text.push_str(" The editor transformed the content; re-read it before the next edit.");
            details["editorTransformed"] = serde_json::json!(true);
        }
        if let Some(before) = before {
            // A notebook diff uses the same cell-source view as read. Compare
            // raw bytes above for editor transformations and keep persistence
            // tied to the complete JSON, including hidden outputs/metadata.
            let views = if notebook_edit {
                crate::notebook::Notebook::parse(before, display_path)
                    .and_then(|notebook| notebook.editable_text())
                    .and_then(|before| {
                        crate::notebook::Notebook::parse(actual, display_path)
                            .and_then(|notebook| notebook.editable_text())
                            .map(|after| (before, after))
                    })
            } else {
                Ok((
                    normalize_to_lf(strip_bom(before).0),
                    normalize_to_lf(strip_bom(actual).0),
                ))
            };
            match views {
                Ok((before, after)) => {
                    let (diff, first) = generate_diff_string(&before, &after);
                    details["diff"] = serde_json::json!(diff);
                    if let Some(first) = first {
                        details["firstChangedLine"] = serde_json::json!(first);
                    }
                }
                Err(_) => {
                    // The editor already acknowledged this write. An invalid
                    // read-back cannot turn that into a safe-to-retry failure.
                    text.push_str(" The editor returned invalid notebook JSON; inspect it with raw=true before further edits.");
                    details["notebookProjectionUnavailable"] = serde_json::json!(true);
                }
            }
        }
    } else {
        text.push_str(" Read-back verification was unavailable; re-read the file before further edits. The acknowledged write was not rolled back.");
    }
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(text))],
        details: Some(details),
        is_error: false,
    }
}

impl WriteTool {
    pub(super) async fn execute_client_write(
        &self,
        input: &WriteInput,
        path: &Path,
    ) -> Result<Option<ToolOutput>> {
        let Some(client) = self.text_file_client.for_path(path)? else {
            return Ok(None);
        };
        if !client.can_write() {
            return Ok(None);
        }
        let before = if ends_with_read_truncation_notice(&input.content) {
            let existing = read_source(
                client.as_ref(),
                path,
                &self.workspace.snapshot_or(&self.cwd).all(),
                "write",
            )
            .await?;
            if existing.len() > input.content.len() {
                return Err(Error::tool(
                    "write",
                    "Refusing to overwrite the editor buffer with an incomplete read; read the remaining lines or use edit",
                ));
            }
            Some(existing)
        } else if client.can_read() {
            // This observation is optional: creating a new editor file or
            // writing without read permission must keep working. Unknown
            // preimages cannot authorize a prewalk handoff.
            read_source(
                client.as_ref(),
                path,
                &self.workspace.snapshot_or(&self.cwd).all(),
                "write",
            )
            .await
            .ok()
        } else {
            None
        };
        let path = enforce_cwd_scope(path, &self.cwd, "write", &self.workspace)?;
        let written = write_client(client.as_ref(), &path, &input.content, "write").await?;
        Ok(Some(mutation_output(
            "write",
            &input.path,
            before.as_deref().filter(|_| client.can_read()),
            &input.content,
            written,
            false,
        )))
    }
}

/// Read/modify/write uses the same source twice to detect changes while
/// preparing the edit. ACP has no versioned CAS: the final client write is
/// editor-owned, not an atomic local rename. Do not claim otherwise.
async fn persist_edit(
    client: &dyn TextFileClient,
    path: &Path,
    cwd: &Path,
    workspace: &WorkspaceHandle,
    before: &str,
    after: &str,
    tool: &'static str,
) -> Result<ClientWrite> {
    if after.len() > MAX_CLIENT_TEXT_BYTES {
        return Err(Error::tool(
            tool,
            "Edited content exceeds the 10 MiB editor file limit",
        ));
    }
    let path = enforce_cwd_scope(path, cwd, tool, workspace)?;
    let roots = workspace.snapshot_or(cwd).all();
    let current = read_source(client, &path, &roots, tool).await?;
    if current != before {
        return Err(Error::tool(
            tool,
            "File changed while preparing the edit; re-read it before retrying",
        ));
    }
    if client.can_write() {
        return write_client(client, &path, after, tool).await;
    }
    // A read-only bridge may expose unsaved bytes. The local CAS refuses to
    // overwrite disk unless it still matches that exact editor preimage.
    let expected = AtomicContentExpectation::from_bytes(before.as_bytes());
    let content = after.as_bytes().to_vec();
    asupersync::runtime::spawn_blocking_io(move || {
        atomic_replace_file_if_unchanged(&path, &roots, &content, expected)
    })
    .await
    .map_err(|error| Error::tool(tool, format!("Cannot persist editor-derived edit: {error}")))?;
    Ok(ClientWrite {
        verified: Some(after.to_string()),
        editor: false,
    })
}

impl EditTool {
    pub(super) async fn execute_client_edit(
        &self,
        input: &EditInput,
        path: &Path,
    ) -> Result<Option<ToolOutput>> {
        let Some(client) = self.text_file_client.for_path(path)? else {
            return Ok(None);
        };
        if !client.can_read() && !client.can_write() {
            return Ok(None);
        }
        let before = read_source(
            client.as_ref(),
            path,
            &self.workspace.snapshot_or(&self.cwd).all(),
            "edit",
        )
        .await?;
        let outcome = apply_file_text_replacement(before.as_bytes(), input, path)?;
        let written = persist_edit(
            client.as_ref(),
            path,
            &self.cwd,
            &self.workspace,
            &before,
            &outcome.final_content,
            "edit",
        )
        .await?;
        Ok(Some(mutation_output(
            "edit",
            &input.path,
            Some(&before),
            &outcome.final_content,
            written,
            !input.raw && crate::notebook::is_notebook_path(path),
        )))
    }
}

impl HashlineEditTool {
    pub(super) async fn execute_client_hashline_edit(
        &self,
        input: &HashlineEditInput,
        path: &Path,
    ) -> Result<Option<ToolOutput>> {
        let Some(client) = self.text_file_client.for_path(path)? else {
            return Ok(None);
        };
        if !client.can_read() && !client.can_write() {
            return Ok(None);
        }
        let before = read_source(
            client.as_ref(),
            path,
            &self.workspace.snapshot_or(&self.cwd).all(),
            "hashline_edit",
        )
        .await?;
        let outcome = apply_file_hashline_edits(&before, input, path)?;
        let written = persist_edit(
            client.as_ref(),
            path,
            &self.cwd,
            &self.workspace,
            &before,
            &outcome.final_content,
            "hashline_edit",
        )
        .await?;
        Ok(Some(mutation_output(
            "hashline_edit",
            &input.path,
            Some(&before),
            &outcome.final_content,
            written,
            !input.raw && crate::notebook::is_notebook_path(path),
        )))
    }
}
