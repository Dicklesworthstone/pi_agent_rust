//! Editor-owned foreground processes. Allocation is not cancellable after
//! publication: retain its request identity, clean up late replies, and fence
//! only the owning session until terminal release is acknowledged.

use super::*;
use crate::model::TextContent;
use crate::text_completion::{RequestStop, with_timeout, with_timeout_and_abort};
use crate::tools::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TerminalClient, TerminalCommand, ToolOutput, ToolUpdate,
    truncate_tail,
};
use async_trait::async_trait;
use std::future::Future;
use std::path::Path;

const MAX_RUNS: usize = 32;
const MAX_PENDING: usize = MAX_RUNS * 4;
const MAX_COMMAND_BYTES: usize = 256 * 1024;
const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const OUTPUT_TIMEOUT: Duration = Duration::from_secs(5);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(3);
const POLL_INTERVAL: Duration = Duration::from_millis(250);

fn failure(message: &'static str) -> Error {
    Error::tool("bash", message)
}

fn valid_identifier(value: &Value) -> bool {
    value.as_str().is_some_and(|id| {
        !id.is_empty() && id.len() <= 512 && !id.chars().any(char::is_control)
    })
}

fn valid_exit(value: &Value) -> bool {
    value.is_object()
        && value.get("exitCode").is_none_or(|code| {
            code.is_null() || code.as_i64().is_some_and(|code| i32::try_from(code).is_ok())
        })
        && value.get("signal").is_none_or(|signal| {
            signal.is_null()
                || signal.as_str().is_some_and(|signal| {
                    !signal.is_empty()
                        && signal.len() <= 128
                        && !signal.chars().any(char::is_control)
                })
        })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Create,
    Output,
    Wait,
    Kill,
    Release,
}

impl Kind {
    fn method(self) -> &'static str {
        match self {
            Self::Create => "terminal/create",
            Self::Output => "terminal/output",
            Self::Wait => "terminal/wait_for_exit",
            Self::Kill => "terminal/kill",
            Self::Release => "terminal/release",
        }
    }

    fn cleanup(self) -> bool {
        matches!(self, Self::Kill | Self::Release)
    }

    fn cancellable(self) -> bool {
        matches!(self, Self::Output | Self::Wait)
    }

    fn valid_result(self, result: &Value) -> bool {
        match self {
            Self::Create => valid_identifier(&result["terminalId"]),
            Self::Output => {
                result["output"]
                    .as_str()
                    .is_some_and(|text| text.len() <= DEFAULT_MAX_BYTES)
                    && result["truncated"].is_boolean()
                    && result
                        .get("exitStatus")
                        .is_none_or(|status| status.is_null() || valid_exit(status))
            }
            Self::Wait => valid_exit(result),
            Self::Kill | Self::Release => result.is_object(),
        }
    }

    fn response(self, result: &Value) -> Value {
        match self {
            Self::Create => json!({"terminalId": result["terminalId"]}),
            Self::Output => json!({
                "output": result["output"], "truncated": result["truncated"],
                "exitStatus": result.get("exitStatus"),
            }),
            Self::Wait => json!({"exitCode": result.get("exitCode"), "signal": result.get("signal")}),
            Self::Kill | Self::Release => json!({}),
        }
    }
}

struct Pending {
    run: u64,
    id: Value,
    kind: Kind,
    sender: Option<oneshot::Sender<Value>>,
}

struct Run {
    session: String,
    terminal: Option<String>,
    creating: bool,
    abandoned: bool,
    exited: bool,
    cleanup_started: bool,
}

#[derive(Default)]
struct State {
    enabled: bool,
    runs: HashMap<u64, Run>,
    pending: HashMap<String, Pending>,
}

#[derive(Default)]
pub(super) struct Requests {
    state: StdMutex<State>,
    counter: AtomicU64,
}

impl Requests {
    pub(super) fn negotiate(&self, params: &Value) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .enabled = params["clientCapabilities"]["terminal"].as_bool() == Some(true);
    }

    pub(super) fn close(&self) {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pending.clear();
        state.runs.clear();
    }

    pub(super) fn has_unsettled(&self, session: &str) -> bool {
        self.state.lock().map_or(true, |state| {
            state.runs.values().any(|run| run.session == session && run.abandoned)
        })
    }

    pub(super) fn has_pending(&self) -> bool {
        self.state.lock().map_or(true, |state| !state.runs.is_empty())
    }

    fn contains(&self, run: u64) -> bool {
        self.state.lock().map_or(true, |state| state.runs.contains_key(&run))
    }

    fn start(&self, output: &AcpOutput, session: &str) -> Result<Lease> {
        let transport = output.shared.state.lock().map_err(|_| failure("ACP output state unavailable"))?;
        if transport.stopping || transport.failure.is_some() {
            return Err(failure("Editor terminal connection is stopping"));
        }
        let prompt = transport.prompts.get(session)
            .filter(|prompt| prompt.input.permits_tool())
            .ok_or_else(|| failure("Editor terminal cancelled before allocation"))?;
        let abort = prompt.abort.signal();
        if abort.is_aborted() {
            return Err(Error::Aborted);
        }
        let mut state = self.state.lock().map_err(|_| failure("Editor terminal registry unavailable"))?;
        if state.runs.len() >= MAX_RUNS
            || state.runs.values().any(|run| run.session == session && run.abandoned)
        {
            return Err(failure("Editor terminal cleanup is still pending"));
        }
        let run = self.counter.fetch_add(1, Ordering::SeqCst);
        state.runs.insert(run, Run {
            session: session.to_string(), terminal: None, creating: false,
            abandoned: false, exited: false, cleanup_started: false,
        });
        Ok(Lease { output: output.clone(), run, abort })
    }

    fn publish(
        &self,
        output: &AcpOutput,
        run: u64,
        kind: Kind,
        mut params: Value,
        sender: Option<oneshot::Sender<Value>>,
    ) -> Result<String> {
        let id = json!(format!("pi-terminal-{}", self.counter.fetch_add(1, Ordering::SeqCst)));
        let key = json_rpc_id_key(&id);
        // Publication and cancellation use the same admission lock. A terminal
        // cleanup request cannot overtake its creation request on the wire.
        let mut transport = output.shared.state.lock().map_err(|_| failure("ACP output state unavailable"))?;
        if transport.failure.is_some() || (transport.stopping && !kind.cleanup()) {
            return Err(failure("Editor terminal connection is unavailable"));
        }
        let mut state = self.state.lock().map_err(|_| failure("Editor terminal registry unavailable"))?;
        if state.pending.len() >= MAX_PENDING {
            return Err(failure("Editor terminal request limit reached"));
        }
        let entry = state.runs.get_mut(&run).ok_or_else(|| failure("Editor terminal owner has retired"))?;
        if entry.abandoned && !kind.cleanup() {
            return Err(failure("Editor terminal was cancelled"));
        }
        if kind == Kind::Create {
            if !transport.prompts.get(&entry.session).is_some_and(|prompt| {
                prompt.input.permits_tool() && !prompt.abort.signal().is_aborted()
            }) {
                return Err(failure("Editor terminal cancelled before publication"));
            }
            entry.creating = true;
        }
        params["sessionId"] = json!(entry.session);
        let frame = json!({"jsonrpc":"2.0", "id":id, "method":kind.method(), "params":params}).to_string();
        state.pending.insert(key.clone(), Pending { run, id, kind, sender });
        drop(state);
        if output.sender.try_send(frame).is_err() {
            output.shared.record_failure(&mut transport, "ACP terminal request could not be delivered");
            return Err(failure("Editor terminal request could not be delivered"));
        }
        Ok(key)
    }

    fn abandon(&self, output: &AcpOutput, run_id: u64) {
        let (cancelled, cleanup) = {
            let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(run) = state.runs.get_mut(&run_id) else { return };
            run.abandoned = true;
            let cleanup = if let Some(terminal) = run.terminal.as_ref() {
                if run.cleanup_started {
                    None
                } else {
                    run.cleanup_started = true;
                    Some((terminal.clone(), run.exited))
                }
            } else if !run.creating {
                state.runs.remove(&run_id);
                None
            } else {
                None
            };
            let cancelled = state.pending.extract_if(|_, pending| {
                pending.run == run_id && pending.kind.cancellable()
            }).map(|(_, pending)| pending).collect::<Vec<_>>();
            (cancelled, cleanup)
        };
        for pending in cancelled {
            let _ = output.send(json_rpc_notification("$/cancel_request", json!({"requestId":pending.id})));
        }
        if let Some((terminal, exited)) = cleanup {
            if !exited {
                let _ = self.publish(output, run_id, Kind::Kill, json!({"terminalId":terminal}), None);
            }
            // Release also terminates a running terminal under the ACP contract.
            // Keep ownership until its valid acknowledgment, never just enqueue.
            let _ = self.publish(output, run_id, Kind::Release, json!({"terminalId":terminal}), None);
        }
    }

    pub(super) fn cancel_session(&self, output: &AcpOutput, session: &str) {
        let runs = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
            .runs.iter().filter(|(_, run)| run.session == session).map(|(id, _)| *id).collect::<Vec<_>>();
        for run in runs {
            self.abandon(output, run);
        }
    }

    pub(super) fn shutdown(&self, output: &AcpOutput) {
        let runs = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
            .runs.keys().copied().collect::<Vec<_>>();
        for run in runs {
            self.abandon(output, run);
        }
    }

    pub(super) fn route_response(&self, output: &AcpOutput, message: &Value, cx: &AgentCx) -> bool {
        if message["jsonrpc"].as_str() != Some("2.0") {
            return false;
        }
        let Some(id) = message.get("id") else { return false };
        let key = json_rpc_id_key(id);
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(pending) = state.pending.get(&key) else { return false };
        let kind = pending.kind;
        let run_id = pending.run;
        let result = message.get("result");
        let error = message.get("error");
        let valid_error = error.is_some_and(|error| error["code"].is_i64() && error["message"].is_string());
        let mut valid_result = result.is_some_and(|result| kind.valid_result(result));
        if kind == Kind::Create && valid_result {
            let session = state.runs.get(&run_id).map(|run| run.session.as_str());
            let terminal = result.and_then(|result| result["terminalId"].as_str());
            valid_result = !state.runs.iter().any(|(id, run)| {
                *id != run_id && Some(run.session.as_str()) == session && run.terminal.as_deref() == terminal
            });
        }
        let settled = (result.is_none() && valid_error) || (error.is_none() && valid_result);
        let response = if settled && valid_error {
            // Never copy client diagnostics that may contain commands or secrets.
            json!({"error":true})
        } else if settled {
            kind.response(result.unwrap_or(&Value::Null))
        } else {
            json!({"invalidResponse":true})
        };
        let sender = if settled || !matches!(kind, Kind::Create | Kind::Release) {
            state.pending.remove(&key).and_then(|pending| pending.sender)
        } else {
            // An invalid allocation/release response cannot prove resource
            // retirement. A later valid response to this exact ID still settles it.
            state.pending.get_mut(&key).and_then(|pending| pending.sender.take())
        };
        let mut discarded = Vec::new();
        if settled {
            if kind == Kind::Create && valid_error {
                state.runs.remove(&run_id);
            } else if kind == Kind::Release && !valid_error {
                state.runs.remove(&run_id);
                discarded = state.pending.extract_if(|_, pending| pending.run == run_id)
                    .map(|(_, pending)| pending).collect();
            } else if let Some(run) = state.runs.get_mut(&run_id) {
                if kind == Kind::Create && !valid_error {
                    run.terminal = response["terminalId"].as_str().map(str::to_string);
                    run.creating = false;
                }
                if !valid_error && (kind == Kind::Wait || (kind == Kind::Output && response["exitStatus"].is_object())) {
                    run.exited = true;
                }
                if kind == Kind::Release && valid_error {
                    // A new explicit session/cancel can retry failed cleanup.
                    run.cleanup_started = false;
                }
            }
        }
        let abandoned = state.runs.get(&run_id).is_some_and(|run| run.abandoned);
        drop(state);
        drop(discarded);
        let undelivered = sender.is_some_and(|sender| sender.send(cx.cx(), response).is_err());
        if kind == Kind::Create && settled && !valid_error && (abandoned || undelivered) {
            self.abandon(output, run_id);
        }
        true
    }
}

struct Lease {
    output: AcpOutput,
    run: u64,
    abort: AbortSignal,
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.output.shared.terminals.abandon(&self.output, self.run);
    }
}

struct RequestGuard {
    output: AcpOutput,
    key: String,
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        let cancelled = {
            let mut state = self.output.shared.terminals.state.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match state.pending.get_mut(&self.key) {
                Some(pending) if pending.kind == Kind::Create => {
                    pending.sender.take();
                    None
                }
                Some(pending) if pending.kind.cancellable() => state.pending.remove(&self.key),
                _ => None,
            }
        };
        if let Some(pending) = cancelled {
            let _ = self.output.send(json_rpc_notification("$/cancel_request", json!({"requestId":pending.id})));
        }
    }
}

struct Client {
    output: AcpOutput,
    session: String,
    cwd: PathBuf,
    rpc_timeout: Duration,
    output_timeout: Duration,
    cleanup_timeout: Duration,
    poll_interval: Duration,
}

pub(super) fn bind(
    tools: &ToolRegistry,
    permissions: Option<&AcpPermissionClient>,
    session: &str,
    cwd: &Path,
) -> Result<()> {
    let Some(permissions) = permissions else { return Ok(()) };
    let enabled = permissions.out_tx.shared.terminals.state.lock()
        .map_err(|_| failure("ACP terminal capabilities unavailable"))?.enabled;
    if !enabled {
        return Ok(());
    }
    tools.set_terminal_client(Arc::new(Client {
        output: permissions.out_tx.clone(), session: session.to_string(),
        cwd: crate::extensions::safe_canonicalize(cwd), rpc_timeout: RPC_TIMEOUT,
        output_timeout: OUTPUT_TIMEOUT, cleanup_timeout: CLEANUP_TIMEOUT, poll_interval: POLL_INTERVAL,
    }))
}

#[derive(Default)]
struct Observed {
    terminal: Option<String>,
    output: String,
    truncated: bool,
    exit_code: Option<i32>,
    signal: Option<String>,
}

impl Observed {
    fn output(&mut self, value: &Value) -> Result<()> {
        let text = value["output"].as_str().ok_or_else(|| failure("Invalid terminal output"))?;
        let tail = truncate_tail(text, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
        self.output = tail.content;
        self.truncated |= tail.truncated || value["truncated"].as_bool() == Some(true);
        Ok(())
    }

    fn exit(&mut self, value: &Value) {
        self.exit_code = value["exitCode"].as_i64().and_then(|code| i32::try_from(code).ok());
        self.signal = value["signal"].as_str().map(str::to_string);
    }

    fn progress(&self, on_update: Option<&(dyn Fn(ToolUpdate) + Send + Sync)>) {
        if let Some(on_update) = on_update {
            on_update(ToolUpdate {
                content: if self.output.is_empty() { Vec::new() } else {
                    vec![ContentBlock::Text(TextContent::new(self.output.clone()))]
                },
                details: Some(json!({"terminalId":self.terminal, "execution":"editor_terminal", "truncated":self.truncated})),
            });
        }
    }
}

impl Client {
    fn validate(&self, command: &TerminalCommand) -> Result<PathBuf> {
        let cwd = crate::extensions::safe_canonicalize(&command.cwd);
        if !cwd.is_absolute() || !cwd.starts_with(&self.cwd) {
            return Err(failure("Terminal working directory is outside the editor workspace"));
        }
        if cwd.to_str().is_none_or(|cwd| cwd.len() > 16 * 1024 || cwd.contains('\0')) {
            return Err(failure("Invalid editor terminal working directory"));
        }
        let argument_bytes = command.args.iter().try_fold(0usize, |bytes, arg| {
            bytes.checked_add(arg.len())
        });
        if command.command.is_empty() || command.command.contains('\0')
            || command.command.len() > 16 * 1024 || command.args.len() > 64
            || command.args.iter().any(|arg| arg.contains('\0'))
            || argument_bytes.is_none_or(|bytes| bytes > MAX_COMMAND_BYTES)
        {
            return Err(failure("Editor terminal command exceeds its argument bounds"));
        }
        let environment_bytes = command.env.iter().try_fold(0usize, |bytes, (name, value)| {
            bytes.checked_add(name.len())?.checked_add(value.len())
        });
        if command.env.len() > 128 || command.env.iter().any(|(name, value)| {
            name.is_empty() || name.len() > 256 || name.contains(['\0', '='])
                || value.len() > 16 * 1024 || value.contains('\0')
        }) || environment_bytes.is_none_or(|bytes| bytes > 128 * 1024) {
            return Err(failure("Editor terminal environment exceeds its bounds"));
        }
        Ok(cwd)
    }

    async fn request(&self, lease: &Lease, owner: &AgentCx, kind: Kind, params: Value) -> Result<Value> {
        owner.checkpoint().map_err(|_| Error::Aborted)?;
        let (sender, mut receiver) = oneshot::channel();
        let key = self.output.shared.terminals.publish(&self.output, lease.run, kind, params, Some(sender))?;
        let _guard = RequestGuard { output: self.output.clone(), key };
        let receive = receiver.recv(owner.cx());
        let response = if kind == Kind::Wait {
            receive.await
        } else {
            owner.with_current(with_timeout(
                if kind == Kind::Output { self.output_timeout } else { self.rpc_timeout },
                receive,
            )).await.map_err(|stop| match stop {
                RequestStop::Cancelled => Error::Aborted,
                RequestStop::TimedOut => failure("Editor terminal request timed out"),
                RequestStop::TimeUnavailable => failure("Editor terminals require timer capabilities"),
            })?
        }.map_err(|_| failure("Editor terminal response channel closed"))?;
        if response.get("error").is_some() || response.get("invalidResponse").is_some() {
            return Err(failure("Editor terminal request failed or returned an invalid response"));
        }
        Ok(response)
    }

    async fn run(
        &self,
        lease: &Lease,
        owner: &AgentCx,
        command: &TerminalCommand,
        observed: &mut Observed,
        on_update: Option<&(dyn Fn(ToolUpdate) + Send + Sync)>,
    ) -> Result<()> {
        let created = self.request(lease, owner, Kind::Create, json!({
            "command":command.command, "args":command.args,
            "cwd":command.cwd,
            "env":command.env.iter().map(|(name,value)| json!({"name":name,"value":value})).collect::<Vec<_>>(),
            "outputByteLimit":DEFAULT_MAX_BYTES,
        })).await?;
        let terminal = created["terminalId"].as_str().ok_or_else(|| failure("Invalid terminal allocation"))?;
        observed.terminal = Some(terminal.to_string());
        observed.progress(on_update);
        let mut wait = Box::pin(self.request(lease, owner, Kind::Wait, json!({"terminalId":terminal})));
        loop {
            let read = self.request(lease, owner, Kind::Output, json!({"terminalId":terminal}));
            match futures::future::select(wait.as_mut(), Box::pin(read)).await {
                futures::future::Either::Left((status, pending_read)) => {
                    drop(pending_read);
                    observed.exit(&status?);
                    break;
                }
                futures::future::Either::Right((snapshot, _)) => {
                    let snapshot = snapshot?;
                    observed.output(&snapshot)?;
                    observed.progress(on_update);
                    if snapshot["exitStatus"].is_object() {
                        observed.exit(&snapshot["exitStatus"]);
                        break;
                    }
                }
            }
            let clock = owner.time();
            match futures::future::select(wait.as_mut(), Box::pin(clock.sleep(self.poll_interval))).await {
                futures::future::Either::Left((status, _)) => {
                    observed.exit(&status?);
                    break;
                }
                futures::future::Either::Right(_) => {}
            }
        }
        drop(wait);
        let final_output = self.request(lease, owner, Kind::Output, json!({"terminalId":terminal})).await?;
        observed.output(&final_output)?;
        Ok(())
    }

    async fn cleanup(&self, lease: &Lease) -> bool {
        self.output.shared.terminals.abandon(&self.output, lease.run);
        let cleanup = AgentCx::for_request();
        let settled = cleanup.with_current(with_timeout(self.cleanup_timeout, async {
            while self.output.shared.terminals.contains(lease.run) {
                cleanup.time().sleep(Duration::from_millis(10)).await;
            }
        })).await.is_ok();
        settled && self.output.failure_reason().is_none()
    }
}

async fn run_with_deadline<F: Future>(
    owner: &AgentCx,
    abort: &AbortSignal,
    duration: Option<Duration>,
    work: F,
) -> std::result::Result<F::Output, RequestStop> {
    if let Some(duration) = duration {
        return owner.with_current(with_timeout_and_abort(duration, Some(abort), work)).await;
    }
    // timeout:0 disables the command deadline, not cancellation or bounded
    // allocation/output/cleanup exchanges. There is no detached waiter.
    if !owner.capabilities().time {
        return Err(RequestStop::TimeUnavailable);
    }
    let cancelled = async {
        loop {
            if abort.is_aborted() || owner.checkpoint().is_err() {
                return;
            }
            owner.time().sleep(Duration::from_millis(25)).await;
        }
    };
    match futures::future::select(Box::pin(cancelled), Box::pin(owner.with_current(work))).await {
        futures::future::Either::Left(_) => Err(RequestStop::Cancelled),
        futures::future::Either::Right((result, _)) => {
            if abort.is_aborted() || owner.checkpoint().is_err() {
                Err(RequestStop::Cancelled)
            } else {
                Ok(result)
            }
        }
    }
}

#[async_trait]
impl TerminalClient for Client {
    async fn execute(
        &self,
        command: TerminalCommand,
        on_update: Option<&(dyn Fn(ToolUpdate) + Send + Sync)>,
    ) -> Result<ToolOutput> {
        let owner = AgentCx::for_current_or_request();
        owner.checkpoint().map_err(|_| Error::Aborted)?;
        if !owner.capabilities().io || !owner.capabilities().spawn {
            return Err(failure("Editor terminals require I/O and spawn capabilities"));
        }
        if !owner.capabilities().time {
            return Err(failure("Editor terminals require timer capabilities"));
        }
        let command = TerminalCommand { cwd: self.validate(&command)?, ..command };
        let lease = self.output.shared.terminals.start(&self.output, &self.session)?;
        let mut observed = Observed::default();
        let outcome = run_with_deadline(&owner, &lease.abort, command.timeout,
            self.run(&lease, &owner, &command, &mut observed, on_update)).await;
        let cleaned = self.cleanup(&lease).await;
        let (status, reason) = match outcome {
            Ok(Ok(())) if observed.exit_code == Some(0) && observed.signal.is_none() => ("completed", None),
            Ok(Ok(())) => ("failed", Some("Editor command ended without a successful exit status".to_string())),
            Ok(Err(error)) => ("failed", Some(error.to_string())),
            Err(RequestStop::Cancelled) => ("cancelled", Some("Editor command cancelled".to_string())),
            Err(RequestStop::TimedOut) => ("timedOut", Some("Editor command timed out".to_string())),
            Err(RequestStop::TimeUnavailable) => ("failed", Some("Editor terminals require timer capabilities".to_string())),
        };
        let mut text = observed.output;
        if let Some(reason) = reason {
            if !text.is_empty() { text.push_str("\n\n"); }
            text.push_str(&reason);
        }
        if observed.truncated {
            text.push_str("\n\n[Editor terminal output truncated]");
        }
        text.push_str("\n\n[Editor terminal: shell state is isolated per call.]");
        if !cleaned {
            let note = "Editor terminal cleanup is unconfirmed. The command may still be running. Further prompts for this session wait for release acknowledgment; cancel the session to retry cleanup or reconnect the editor.";
            text.push_str("\n\n");
            text.push_str(note);
            self.output.abort_prompt(&self.session);
            let _ = self.output.send(json_rpc_notification("session/update", json!({
                "sessionId":self.session,
                "update":{"sessionUpdate":"agent_message_chunk", "content":{"type":"text","text":note}},
            })));
        }
        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(text))],
            details: Some(json!({
                "execution":"editor_terminal", "terminalId":observed.terminal,
                "status":status, "exitCode":observed.exit_code, "signal":observed.signal,
                "truncated":observed.truncated, "cleanup":if cleaned {"released"} else {"unconfirmed"},
                "shellState":{"mode":"isolated"},
                "timeoutMs":command.timeout.map(|value| u64::try_from(value.as_millis()).unwrap_or(u64::MAX)),
            })),
            is_error: status != "completed" || !cleaned,
        })
    }
}

pub(super) fn tool_content(blocks: &[ContentBlock], details: Option<&Value>) -> Vec<Value> {
    let mut content = super::content::tool_result_content(blocks);
    if let Some(terminal) = details.and_then(|details| details.get("terminalId"))
        && valid_identifier(terminal)
    {
        content.push(json!({"type":"terminal", "terminalId":terminal}));
    }
    content
}

#[cfg(test)]
mod tests;
