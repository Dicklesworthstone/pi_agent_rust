//! Agent-to-editor filesystem requests. Published writes cannot be recalled:
//! abandoning their waiter fences that session until the editor settles them.

use super::*;
use crate::tools::{TextFileClient, ToolRegistry};
use async_trait::async_trait;
use std::path::Path;

const MAX_TEXT_BYTES: usize = 10 * 1024 * 1024;
const MAX_PENDING: usize = 128;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, Default)]
struct Capabilities {
    read: bool,
    write: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Read,
    Write,
}

struct Pending {
    session: String,
    id: Value,
    kind: Kind,
    sender: Option<oneshot::Sender<Value>>,
    abandoned: bool,
}

#[derive(Default)]
struct State {
    capabilities: Capabilities,
    pending: HashMap<String, Pending>,
}

#[derive(Default)]
pub(super) struct Requests {
    state: StdMutex<State>,
    counter: AtomicU64,
}

impl Requests {
    pub(super) fn negotiate(&self, params: &Value) {
        let fs = &params["clientCapabilities"]["fs"];
        if let Ok(mut state) = self.state.lock() {
            state.capabilities = Capabilities {
                read: fs["readTextFile"].as_bool() == Some(true),
                write: fs["writeTextFile"].as_bool() == Some(true),
            };
        }
    }

    pub(super) fn close(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending
            .clear();
    }

    pub(super) fn has_write(&self, session: &str) -> bool {
        self.state.lock().map_or(true, |state| {
            state
                .pending
                .values()
                .any(|pending| pending.session == session && pending.kind == Kind::Write)
        })
    }

    pub(super) fn cancel_reads(&self, session: &str) -> Vec<Value> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending
            .extract_if(|_, pending| pending.session == session && pending.kind == Kind::Read)
            .map(|(_, pending)| pending.id)
            .collect()
    }

    pub(super) fn route_response(&self, message: &Value, cx: &AgentCx) -> bool {
        if message["jsonrpc"].as_str() != Some("2.0") {
            return false;
        }
        let Some(id) = message.get("id") else {
            return false;
        };
        let key = json_rpc_id_key(id);
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        let Some(pending) = state.pending.get(&key) else {
            return false;
        };
        let result = message.get("result");
        let error = message.get("error");
        let valid_error =
            error.is_some_and(|error| error["code"].is_i64() && error["message"].is_string());
        let valid_result = result.is_some_and(|result| match pending.kind {
            Kind::Read => result["content"]
                .as_str()
                .is_some_and(|content| content.len() <= MAX_TEXT_BYTES),
            Kind::Write => result.is_object(),
        });
        let settled = (result.is_none() && valid_error) || (error.is_none() && valid_result);
        let response = if settled && valid_error {
            json!({"error":true})
        } else if settled && pending.kind == Kind::Read {
            json!({"content":result.and_then(|result| result.get("content"))})
        } else if settled {
            // Only acknowledgment matters. Never copy arbitrary client metadata.
            json!({})
        } else {
            json!({"invalidResponse":true})
        };
        let sender = if settled || pending.kind == Kind::Read {
            state
                .pending
                .remove(&key)
                .and_then(|pending| pending.sender)
        } else {
            // A malformed write response cannot prove that the mutation did
            // or did not happen. Keep its identity for a later valid response.
            state
                .pending
                .get_mut(&key)
                .and_then(|pending| pending.sender.take())
        };
        drop(state);
        if let Some(sender) = sender {
            let _ = sender.send(cx.cx(), response);
        }
        true
    }
}

struct Client {
    output: AcpOutput,
    session: String,
    cwd: PathBuf,
    internal_roots: Vec<PathBuf>,
    capabilities: Capabilities,
    timeout: Duration,
}

pub(super) fn bind(
    tools: &ToolRegistry,
    permissions: Option<&AcpPermissionClient>,
    session: &str,
    cwd: &Path,
    session_dir: Option<&PathBuf>,
) -> Result<()> {
    let Some(permissions) = permissions else {
        return Ok(());
    };
    let capabilities = permissions
        .out_tx
        .shared
        .files
        .state
        .lock()
        .map_err(|_| Error::session("ACP filesystem capabilities unavailable"))?
        .capabilities;
    if !capabilities.read && !capabilities.write {
        return Ok(());
    }
    let mut internal_roots = vec![crate::extensions::safe_canonicalize(&Config::global_dir())];
    if let Some(path) = session_dir {
        internal_roots.push(crate::extensions::safe_canonicalize(path));
    }
    tools.set_text_file_client(Arc::new(Client {
        output: permissions.out_tx.clone(),
        session: session.to_string(),
        cwd: crate::extensions::safe_canonicalize(cwd),
        internal_roots,
        capabilities,
        timeout: REQUEST_TIMEOUT,
    }))
}

struct RequestGuard {
    output: AcpOutput,
    key: String,
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        let action = {
            let mut state = self
                .output
                .shared
                .files
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match state.pending.get_mut(&self.key) {
                Some(pending) if pending.kind == Kind::Write && !pending.abandoned => {
                    pending.abandoned = true;
                    Some((Kind::Write, pending.id.clone(), pending.session.clone()))
                }
                Some(pending) if pending.kind == Kind::Read => state
                    .pending
                    .remove(&self.key)
                    .map(|pending| (Kind::Read, pending.id, pending.session)),
                _ => None,
            }
        };
        if let Some((kind, id, session)) = action {
            if kind == Kind::Read {
                let _ = self.output.send(json_rpc_notification(
                    "$/cancel_request",
                    json!({"requestId":id}),
                ));
            } else {
                // Stop every later tool in this prompt. Keep the request in the
                // broker: a late acknowledgment settles only this exact write.
                self.output.abort_prompt(&session);
                let _ = self.output.send(json_rpc_notification("session/update", json!({
                    "sessionId":session,
                    "update":{"sessionUpdate":"agent_message_chunk", "content":{"type":"text",
                        "text":"Editor file write outcome is not confirmed. It may still complete; it was not rolled back. Further prompts for this session wait for the editor's acknowledgment."}}
                })));
            }
        }
    }
}

impl Client {
    async fn request(&self, path: &Path, content: Option<&str>) -> Result<Value> {
        let owner = AgentCx::for_current_or_request();
        owner.checkpoint().map_err(|_| Error::Aborted)?;
        let path = crate::extensions::safe_canonicalize(path);
        if !path.is_absolute() || !self.handles_path(&path) {
            return Err(Error::tool(
                "editor_fs",
                "Path is outside the editor workspace",
            ));
        }
        let path = path
            .to_str()
            .filter(|path| path.len() <= 16 * 1024 && !path.contains('\0'))
            .ok_or_else(|| Error::tool("editor_fs", "Invalid editor filesystem path"))?;
        let kind = if content.is_some() {
            Kind::Write
        } else {
            Kind::Read
        };
        if (kind == Kind::Write && !self.capabilities.write)
            || (kind == Kind::Read && !self.capabilities.read)
        {
            return Err(Error::tool(
                "editor_fs",
                "Editor filesystem capability is unavailable",
            ));
        }
        if content.is_some_and(|content| content.len() > MAX_TEXT_BYTES) {
            return Err(Error::tool(
                "editor_fs",
                "Editor text exceeds the 10 MiB file limit",
            ));
        }
        let id = json!(format!(
            "pi-fs-{}",
            self.output
                .shared
                .files
                .counter
                .fetch_add(1, Ordering::SeqCst)
        ));
        let key = json_rpc_id_key(&id);
        let mut params = json!({"sessionId":self.session, "path":path});
        if let Some(content) = content {
            params["content"] = json!(content);
        }
        let frame = json!({"jsonrpc":"2.0", "id":id,
            "method":if kind == Kind::Read { "fs/read_text_file" } else { "fs/write_text_file" },
            "params":params})
        .to_string();
        let (sender, mut receiver) = oneshot::channel();
        {
            // Same order as permission publication and transport failure.
            let mut transport = self
                .output
                .shared
                .state
                .lock()
                .map_err(|_| Error::session("ACP output state unavailable"))?;
            if transport.stopping
                || transport.failure.is_some()
                || !transport
                    .prompts
                    .get(&self.session)
                    .is_some_and(|prompt| prompt.input.permits_tool())
            {
                return Err(Error::tool(
                    "editor_fs",
                    "Editor request cancelled before publication",
                ));
            }
            let mut state = self
                .output
                .shared
                .files
                .state
                .lock()
                .map_err(|_| Error::session("ACP filesystem request registry unavailable"))?;
            if state.pending.len() >= MAX_PENDING
                || state
                    .pending
                    .values()
                    .any(|pending| pending.session == self.session && pending.kind == Kind::Write)
            {
                return Err(Error::tool(
                    "editor_fs",
                    "Editor filesystem requests are still pending",
                ));
            }
            state.pending.insert(
                key.clone(),
                Pending {
                    session: self.session.clone(),
                    id,
                    kind,
                    sender: Some(sender),
                    abandoned: false,
                },
            );
            drop(state);
            if self.output.sender.try_send(frame).is_err() {
                self.output.shared.record_failure(
                    &mut transport,
                    "ACP filesystem request could not be delivered",
                );
                return Err(Error::tool(
                    "editor_fs",
                    "Editor filesystem request could not be delivered",
                ));
            }
        }
        let _guard = RequestGuard {
            output: self.output.clone(),
            key,
        };
        let response = timeout(
            wall_now(),
            self.timeout,
            Box::pin(receiver.recv(owner.cx())),
        )
        .await
        .map_err(|_| Error::tool("editor_fs", "Editor filesystem request timed out"))?
        .map_err(|_| Error::tool("editor_fs", "Editor filesystem response channel closed"))?;
        if response.get("error").is_some() || response.get("invalidResponse").is_some() {
            return Err(Error::tool(
                "editor_fs",
                "Editor filesystem request failed or returned an invalid response",
            ));
        }
        Ok(response)
    }
}

#[async_trait]
impl TextFileClient for Client {
    fn can_read(&self) -> bool {
        self.capabilities.read
    }
    fn can_write(&self) -> bool {
        self.capabilities.write
    }
    fn handles_path(&self, path: &Path) -> bool {
        path.starts_with(&self.cwd)
            && !self
                .internal_roots
                .iter()
                .any(|root| path.starts_with(root))
    }
    async fn read_text_file(&self, path: &Path) -> Result<String> {
        let response = self.request(path, None).await?;
        let content = response["content"]
            .as_str()
            .filter(|content| content.len() <= MAX_TEXT_BYTES)
            .ok_or_else(|| Error::tool("editor_fs", "Editor returned invalid or oversized text"))?;
        Ok(content.to_string())
    }
    async fn write_text_file(&self, path: &Path, content: &str) -> Result<()> {
        self.request(path, Some(content)).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests;
