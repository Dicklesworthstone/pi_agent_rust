//! Independent peers exercise consent, model dispatch, and cancellation over
//! every native MCP wire. The invocation host is local; no live model is used.

use super::*;
use crate::ask::{AskAnswer, AskRequest, AskResponse, AskUiRequest};
use crate::mcp::sampling::SamplingContext;
use crate::mcp::{ConfiguredServer, McpDiscovery, Provenance, TrustStore};
use crate::model::{StopReason, ToolCall};
use crate::text_completion::{AuxiliaryPrivacy, AuxiliaryPrivacySnapshot};
use crate::tools::{
    SharedToolRegistry, ToolInvocationContext, ToolInvocationHost, ToolOutput, ToolRegistry,
    ToolSamplingModel, ToolSamplingRequest, ToolSamplingResult, ToolSamplingRole,
};
use asupersync::{Budget, Cx};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const PEER: &str = r"
import json, pathlib, queue, sys, threading, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

root = pathlib.Path(sys.argv[1])
mode, scenario = sys.argv[2:4]
log_lock = threading.Lock()
write_lock = threading.Lock()
events = queue.Queue(maxsize=16)
replied = threading.Event()
stopped = threading.Event()
sample_reply = None

def record(frame):
    with log_lock:
        with (root / 'calls.jsonl').open('a') as log:
            log.write(json.dumps(frame) + '\n')

def prompt(ident='sample-request'):
    params = {'messages':[
        {'role':'user','content':[
            {'type':'text','text':'First block scoped-secret-123456'},
            {'type':'text','text':'Second block'}]},
        {'role':'assistant','content':{'type':'text','text':'Previous draft'}},
        {'role':'user','content':{'type':'text','text':'Please refine the draft'}}],
        'systemPrompt':'Review\x1b[2J\n\u202e https://example.com',
        'maxTokens':99999, 'temperature':0.4,
        'modelPreferences':{'hints':[{'name':'unselected-server-model'}],
                            'intelligencePriority':0.8},
        'metadata':{'apiKey':'must-not-be-provider-options'}}
    if scenario == 'task':
        params['_meta'] = {'io.modelcontextprotocol/related-task':{'taskId':'owned-sample'}}
    if scenario == 'tools':
        params['tools'] = [{'name':'read','inputSchema':{'type':'object'}}]
    if scenario == 'context':
        params['includeContext'] = 'allServers'
    if scenario == 'image':
        params['messages'][0]['content'] = {'type':'image','mimeType':'image/png','data':'aW1hZ2U='}
    return {'jsonrpc':'2.0','id':ident,'method':'sampling/createMessage','params':params}

def exchange(frame, emit):
    global sample_reply
    record(frame)
    method, ident = frame.get('method'), frame.get('id')
    if method is None:
        if ident == 'cancelled-sample':
            assert frame['error']['code'] == -1
            return
        assert ident == 'sample-request'
        assert not replied.is_set(), 'duplicate sampling response'
        sample_reply = frame
        replied.set()
        return
    if ident is None:
        return
    if method == 'initialize':
        assert frame['params']['capabilities']['sampling'] == {}
        caps = {'tools':{}}
        if scenario == 'task':
            caps['tasks'] = {'requests':{'tools':{'call':{}}},'cancel':{}}
        value = {'protocolVersion':'2025-11-25','capabilities':caps,
                 'serverInfo':{'name':'sampling-peer','version':'1'}}
    elif method == 'tools/list':
        tool = {'name':'work','inputSchema':{'type':'object'},'description':'Server-requested generation'}
        if scenario == 'task':
            tool['execution'] = {'taskSupport':'required'}
        value = {'tools':[tool]}
    elif method == 'tools/call' and scenario == 'task':
        assert 'task' in frame['params']
        value = {'task':{'taskId':'owned-sample','status':'input_required',
                        'createdAt':'2025-11-25T10:00:00Z',
                        'lastUpdatedAt':'2025-11-25T10:00:00Z','ttl':60000}}
        emit({'jsonrpc':'2.0','id':ident,'result':value})
        # Deliberately race the caller's task-receipt binding on push transports.
        if mode != 'http':
            emit(prompt())
        return
    elif method in ('tools/call','tasks/result','resources/read','prompts/get'):
        if scenario == 'cancel-generation':
            emit(prompt('cancelled-sample'))
            deadline = time.monotonic() + 5
            while not (root / 'sample-started').exists():
                assert time.monotonic() < deadline, 'generation never started'
                time.sleep(0.005)
            emit({'jsonrpc':'2.0','method':'notifications/cancelled',
                  'params':{'requestId':'cancelled-sample'}})
        if scenario == 'context-overlap' and method == 'tools/call':
            (root / 'tool-dispatched').write_text('ready')
        elif scenario == 'context-overlap':
            deadline = time.monotonic() + 5
            while not (root / 'tool-dispatched').exists():
                assert time.monotonic() < deadline, 'tool never dispatched'
                time.sleep(0.005)
            emit(prompt())
        elif scenario != 'task' or mode == 'http':
            emit(prompt())
        assert replied.wait(8), 'client never answered sampling'
        if method == 'resources/read':
            value = {'contents':[{'uri':frame['params']['uri'],'text':'resource ready'}]}
        elif method == 'prompts/get':
            value = {'messages':[{'role':'user','content':{'type':'text','text':'prompt ready'}}]}
        else:
            value = {'content':[{'type':'text','text':'finished'}],
                     'structuredContent':{'sampling':sample_reply}}
        if scenario == 'task':
            value['_meta'] = {'io.modelcontextprotocol/related-task':{'taskId':'owned-sample'}}
    elif method == 'ping':
        value = {}
    elif method == 'tasks/cancel':
        return
    else:
        raise RuntimeError('unexpected method: ' + str(method))
    emit({'jsonrpc':'2.0','id':ident,'result':value})

def safe_exchange(frame, emit):
    try:
        exchange(frame, emit)
    except Exception as error:
        (root / 'peer-error').write_text(repr(error))
        raise

if mode == 'stdio':
    def emit(frame):
        with write_lock:
            print(json.dumps(frame), flush=True)
    for line in sys.stdin:
        frame = json.loads(line)
        if frame.get('method') in ('tools/call','tasks/result','resources/read','prompts/get'):
            threading.Thread(target=safe_exchange,args=(frame,emit),daemon=True).start()
        else:
            safe_exchange(frame,emit)
else:
    class Handler(BaseHTTPRequestHandler):
        protocol_version = 'HTTP/1.1'
        def log_message(self, *args):
            pass
        def empty(self, status):
            self.send_response(status)
            self.send_header('Content-Length','0')
            self.end_headers()
        def do_GET(self):
            if mode == 'http':
                self.empty(405)
                return
            self.send_response(200)
            self.send_header('Content-Type','text/event-stream')
            self.send_header('Connection','close')
            self.end_headers()
            if mode == 'sse':
                self.wfile.write(b'event: endpoint\ndata: /messages\n\n')
            self.wfile.flush()
            deadline = time.monotonic() + 15
            while not stopped.is_set() and time.monotonic() < deadline:
                try:
                    value = events.get(timeout=0.1)
                except queue.Empty:
                    continue
                self.wfile.write(('data: '+json.dumps(value)+'\n\n').encode())
                self.wfile.flush()
            self.close_connection = True
        def do_DELETE(self):
            stopped.set()
            self.empty(200)
        def do_POST(self):
            count = int(self.headers.get('Content-Length','0'))
            assert 0 < count <= 65536
            frame = json.loads(self.rfile.read(count))
            if 'method' not in frame or 'id' not in frame:
                safe_exchange(frame, lambda value: events.put(value,timeout=1))
                self.empty(202)
                return
            if mode == 'sse':
                self.empty(202)
                threading.Thread(target=safe_exchange,args=(frame,lambda value: events.put(value,timeout=1)),daemon=True).start()
                return
            if mode == 'http' and frame['method'] in ('tools/call','tasks/result','resources/read','prompts/get'):
                self.send_response(200)
                self.send_header('Content-Type','text/event-stream')
                self.send_header('Connection','close')
                self.end_headers()
                def emit(value):
                    self.wfile.write(('data: '+json.dumps(value)+'\n\n').encode())
                    self.wfile.flush()
                safe_exchange(frame,emit)
                self.close_connection = True
                return
            replies = []
            def emit(value):
                if 'method' in value:
                    events.put(value,timeout=1)
                else:
                    replies.append(value)
            safe_exchange(frame,emit)
            payload = json.dumps(replies[0]).encode()
            self.send_response(200)
            self.send_header('Content-Type','application/json')
            self.send_header('Content-Length',str(len(payload)))
            if frame['method'] == 'initialize':
                self.send_header('Mcp-Session-Id','sampling-session')
            self.end_headers()
            self.wfile.write(payload)
    server = ThreadingHTTPServer(('127.0.0.1',0),Handler)
    (root / 'port.tmp').write_text(str(server.server_port))
    (root / 'port.tmp').replace(root / 'port')
    server.serve_forever()
";

struct Peer(Child);

impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn fixture(root: &Path, mode: &str, scenario: &str) -> (Arc<McpManager>, Option<Peer>) {
    let script = root.join("sampling_peer.py");
    std::fs::write(&script, PEER).unwrap();
    let args = vec![
        "-u".to_string(),
        script.display().to_string(),
        root.display().to_string(),
        mode.to_string(),
        scenario.to_string(),
    ];
    let mut peer = None;
    let url = if mode == "stdio" {
        None
    } else {
        peer = Some(Peer(
            Command::new("python3")
                .args(&args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        ));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !root.join("port").exists() {
            assert!(Instant::now() < deadline, "sampling peer did not bind");
            assert!(peer.as_mut().unwrap().0.try_wait().unwrap().is_none());
            std::thread::sleep(Duration::from_millis(5));
        }
        Some(format!(
            "http://127.0.0.1:{}/mcp",
            std::fs::read_to_string(root.join("port")).unwrap().trim()
        ))
    };
    let manager = Arc::new(McpManager::new(
        root,
        root,
        McpDiscovery {
            servers: vec![ConfiguredServer {
                name: "sampling-peer".to_string(),
                command: (mode == "stdio").then(|| "python3".to_string()),
                args: if mode == "stdio" { args } else { Vec::new() },
                env: Vec::new(),
                url,
                headers: Vec::new(),
                oauth: None,
                transport_hint: Some(if mode == "http-get" { "http" } else { mode }.to_string()),
                enabled: true,
                cwd: None,
                timeout_ms: Some(5000),
                provenance: Provenance::ProjectPi,
                source_file: root.join("mcp.json"),
            }],
            warnings: Vec::new(),
        },
    ));
    let entry = manager.entry("sampling-peer").unwrap();
    let fingerprint = manager.trust_fingerprint_for(&entry);
    let mut trust = TrustStore::load(&manager.inner.trust_path).unwrap();
    if let Some(identity) = entry.config.execution_identity(root).unwrap() {
        trust
            .acknowledge_execution("sampling-peer", &fingerprint, "operator", identity)
            .unwrap();
    } else {
        trust
            .acknowledge("sampling-peer", &fingerprint, "operator")
            .unwrap();
    }
    (manager, peer)
}

fn frames(root: &Path) -> Vec<Value> {
    std::fs::read_to_string(root.join("calls.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap()
}

fn privacy() -> AuxiliaryPrivacySnapshot {
    AuxiliaryPrivacySnapshot::from_privacy(AuxiliaryPrivacy::from_settings(Some(
        &crate::secrets::SecretsSettings {
            mode: Some("obfuscate".to_string()),
            extra_patterns: Some(vec!["scoped-secret-[0-9]+".to_string()]),
        },
    )))
}

fn privacy_projection(parts: &[&str]) -> Result<Vec<String>> {
    privacy().project_inputs(parts)
}

struct Host {
    requests: Mutex<Vec<ToolSamplingRequest>>,
    model_changed: AtomicBool,
    live_session_changed: AtomicBool,
    stall_first: Option<PathBuf>,
    first_dropped: AtomicBool,
    stop_reason: StopReason,
}

impl Host {
    fn new() -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            model_changed: AtomicBool::new(false),
            live_session_changed: AtomicBool::new(false),
            stall_first: None,
            first_dropped: AtomicBool::new(false),
            stop_reason: StopReason::Stop,
        }
    }
}

struct SampleLifetime<'a>(&'a AtomicBool);

impl Drop for SampleLifetime<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl ToolInvocationHost for Host {
    async fn call_tool(&self, _call: ToolCall, _expected_session: &str) -> Result<ToolOutput> {
        panic!("basic sampling must not acquire executable tools")
    }

    fn sampling_model(&self) -> Result<ToolSamplingModel> {
        Ok(ToolSamplingModel {
            provider: "fixture-provider".to_string(),
            model: if self.model_changed.load(Ordering::SeqCst) {
                "replacement-model"
            } else {
                "fixture-model"
            }
            .to_string(),
            session_id: "owning-agent-session".to_string(),
            max_tokens: Some(128),
            temperature: Some(0.25),
        })
    }

    async fn check_sampling_session(&self, expected_session: &str) -> Result<()> {
        assert_eq!(expected_session, "owning-agent-session");
        if self.live_session_changed.load(Ordering::SeqCst) {
            Err(Error::tool("mcp", "fixture live session changed"))
        } else {
            Ok(())
        }
    }

    async fn sample(
        &self,
        request: ToolSamplingRequest,
        expected_session: &str,
    ) -> Result<ToolSamplingResult> {
        assert!(Cx::current().is_some(), "provider dispatch must keep its owner");
        assert_eq!(expected_session, "owning-agent-session");
        let first = {
            let mut requests = McpManager::lock(&self.requests);
            requests.push(request);
            requests.len() == 1
        };
        if first && let Some(marker) = &self.stall_first {
            let _lifetime = SampleLifetime(&self.first_dropped);
            std::fs::write(marker, "provider started").unwrap();
            futures::future::pending::<()>().await;
        }
        Ok(ToolSamplingResult {
            text: "Generated safely. scoped-secret-654321".to_string(),
            stop_reason: self.stop_reason,
        })
    }
}

fn answer(request: &AskRequest, label: &str) -> AskResponse {
    AskResponse {
        answers: vec![AskAnswer {
            question_id: request.questions[0].id.clone().unwrap(),
            selected: vec![label.to_string()],
            other: None,
        }],
        dismissed: false,
    }
}

async fn sample(manager: &McpManager, host: Option<&Host>) -> Result<Value> {
    let sampling = host
        .map(|host| SamplingContext::capture(host, privacy()))
        .transpose()?;
    manager
        .call_tool_with_sampling("sampling-peer", "work", json!({}), None, sampling)
        .await
}

async fn mount(manager: &Arc<McpManager>, registry: &SharedToolRegistry) {
    manager.refresh_tools("sampling-peer").await.unwrap();
    crate::mcp::reconcile_tools(manager, registry);
    assert!(registry.snapshot().get("ask").is_none());
}

fn assert_single_reply(root: &Path) -> Value {
    let requests = frames(root);
    assert_eq!(
        requests.iter().filter(|frame| frame["method"] == "tools/call").count(),
        1,
        "originating tool must not be replayed"
    );
    let replies: Vec<_> = requests
        .iter()
        .filter(|frame| frame["id"] == "sample-request")
        .collect();
    assert_eq!(replies.len(), 1, "sampling response must be delivered exactly once");
    assert!(!root.join("peer-error").exists());
    replies[0].clone()
}

#[test]
fn sampling_uses_two_approvals_and_the_owning_model_on_all_native_transports() {
    for mode in ["stdio", "http", "http-get", "sse"] {
        for scenario in ["accept", "task"] {
            let temp = tempfile::tempdir().unwrap();
            let (manager, _peer) = fixture(temp.path(), mode, scenario);
            let registry = SharedToolRegistry::new(ToolRegistry::without_builtins(None));
            let approvals = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&approvals);
            registry.snapshot().host_ask_tool().set_handler(Arc::new(move |request| {
                assert!(Cx::current().is_some(), "approval must keep its tool owner");
                crate::ask::validate_request(request.clone()).unwrap();
                let question = &request.questions[0];
                assert!(question.question.contains("sampling-peer"));
                assert!(!question.question.contains('\u{1b}'));
                assert!(!question.question.contains('\u{202e}'));
                assert!(!question.question.contains("https://"));
                assert!(!question.question.contains("scoped-secret-"));
                let stage = observed.fetch_add(1, Ordering::SeqCst);
                let response = answer(&request, if stage == 0 {
                    assert_eq!(question.id.as_deref(), Some("mcp_sample_prompt"));
                    "Generate this reply"
                } else {
                    assert_eq!(stage, 1);
                    assert_eq!(question.id.as_deref(), Some("mcp_sample_review"));
                    "Send reply to server"
                });
                Box::pin(async move { Ok(response) })
            }));
            let mut host = Host::new();
            if scenario == "task" {
                host.stop_reason = StopReason::Length;
            }
            runtime().block_on(async {
                mount(&manager, &registry).await;
                let snapshot = registry.snapshot();
                let tool = snapshot.get("mcp__sampling-peer__work").unwrap();
                let output = tool.execute_with_invocation(
                    "owned-sampling-call",
                    json!({}),
                    None,
                    ToolInvocationContext {
                        protect: &privacy_projection,
                        host: Some(&host),
                        snapshot_privacy: &privacy,
                    },
                ).await.unwrap();
                assert!(!output.is_error);
                let _ = manager.shutdown_all().await;
            });
            let response = assert_single_reply(temp.path());
            assert!(response.get("error").is_none(), "{mode}/{scenario}: {response}");
            assert_eq!(response["result"]["role"], "assistant");
            assert_eq!(response["result"]["model"], "fixture-model");
            assert_eq!(response["result"]["content"]["type"], "text");
            assert_eq!(
                response["result"]["content"]["text"],
                "Generated safely. <pi-secret:redacted>"
            );
            assert_eq!(response["result"]["stopReason"], if scenario == "task" {
                "maxTokens"
            } else {
                "endTurn"
            });
            if scenario == "task" {
                assert_eq!(
                    response["result"]["_meta"]["io.modelcontextprotocol/related-task"]["taskId"],
                    "owned-sample"
                );
            }
            assert_eq!(approvals.load(Ordering::SeqCst), 2);
            let requests = McpManager::lock(&host.requests);
            assert_eq!(requests.len(), 1);
            let request = &requests[0];
            assert_eq!(request.max_tokens, 128);
            assert_eq!(request.temperature, Some(0.4));
            assert_eq!(request.messages.len(), 3);
            assert!(matches!(request.messages[0].role, ToolSamplingRole::User));
            assert!(matches!(request.messages[1].role, ToolSamplingRole::Assistant));
            assert_eq!(
                request.messages[0].text,
                "First block <pi-secret:redacted>\nSecond block"
            );
            assert_eq!(request.messages[2].text, "Please refine the draft");
        }
    }
}

#[test]
fn unavailable_or_denied_sampling_never_discloses_a_generated_reply() {
    for mode in ["stdio", "http", "sse"] {
        for scenario in ["headless", "no-host", "decline", "discard"] {
            let temp = tempfile::tempdir().unwrap();
            let (manager, _peer) = fixture(temp.path(), mode, scenario);
            let registry = SharedToolRegistry::new(ToolRegistry::without_builtins(None));
            let approvals = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&approvals);
            if scenario != "headless" {
                registry.snapshot().host_ask_tool().set_handler(Arc::new(move |request| {
                    observed.fetch_add(1, Ordering::SeqCst);
                    let review = request.questions[0].id.as_deref() == Some("mcp_sample_review");
                    let response = answer(&request, if review {
                        "Discard reply"
                    } else if scenario == "decline" {
                        "Decline request"
                    } else {
                        "Generate this reply"
                    });
                    Box::pin(async move { Ok(response) })
                }));
            }
            let host = Host::new();
            runtime().block_on(async {
                mount(&manager, &registry).await;
                sample(&manager, (scenario != "no-host").then_some(&host)).await.unwrap();
                let _ = manager.shutdown_all().await;
            });
            let response = assert_single_reply(temp.path());
            assert_eq!(response["error"]["code"], -1, "{mode}/{scenario}: {response}");
            assert!(response.get("result").is_none());
            assert!(!response.to_string().contains("Generated safely"));
            assert_eq!(McpManager::lock(&host.requests).len(), usize::from(scenario == "discard"));
            if matches!(scenario, "headless" | "no-host") {
                assert_eq!(approvals.load(Ordering::SeqCst), 0);
            }
        }
    }
}

#[test]
fn nonadvertised_sampling_features_are_rejected_before_ui_or_provider_dispatch() {
    for mode in ["stdio", "http", "sse"] {
        for scenario in ["tools", "context", "image"] {
            let temp = tempfile::tempdir().unwrap();
            let (manager, _peer) = fixture(temp.path(), mode, scenario);
            let registry = SharedToolRegistry::new(ToolRegistry::without_builtins(None));
            registry.snapshot().host_ask_tool().set_handler(Arc::new(|_| {
                panic!("unsupported request must not acquire a user approval")
            }));
            let host = Host::new();
            runtime().block_on(async {
                mount(&manager, &registry).await;
                sample(&manager, Some(&host)).await.unwrap();
                let _ = manager.shutdown_all().await;
            });
            let response = assert_single_reply(temp.path());
            assert_eq!(response["error"]["code"], -32602, "{mode}/{scenario}: {response}");
            assert!(McpManager::lock(&host.requests).is_empty());
        }
    }
}

#[test]
fn edits_are_reprojected_and_reviewed_before_generation_and_release() {
    for mode in ["stdio", "http"] {
        let temp = tempfile::tempdir().unwrap();
        let (manager, _peer) = fixture(temp.path(), mode, "accept");
        let registry = SharedToolRegistry::new(ToolRegistry::without_builtins(None));
        let approvals = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&approvals);
        registry.snapshot().host_ask_tool().set_handler(Arc::new(move |request| {
            let stage = observed.fetch_add(1, Ordering::SeqCst);
            let mut response = answer(&request, if stage < 2 {
                "Generate this reply"
            } else {
                "Send reply to server"
            });
            if stage == 0 {
                response.answers[0].selected.clear();
                response.answers[0].other = Some(json!({
                    "messages":[{"role":"user","content":{
                        "type":"text","text":"Edited scoped-secret-777777"
                    }}],
                    "maxTokens":31,
                    "temperature":0.8
                }).to_string());
            } else if stage == 1 {
                assert_eq!(request.questions[0].id.as_deref(), Some("mcp_sample_prompt"));
                assert!(request.questions[0].question.contains("Edited"));
                assert!(!request.questions[0].question.contains("scoped-secret-777777"));
            } else if stage == 2 {
                response.answers[0].selected.clear();
                response.answers[0].other = Some("Replacement scoped-secret-888888".to_string());
            } else {
                assert_eq!(stage, 3);
                assert_eq!(request.questions[0].id.as_deref(), Some("mcp_sample_review"));
                assert!(request.questions[0].question.contains("Replacement"));
                assert!(!request.questions[0].question.contains("scoped-secret-888888"));
            }
            Box::pin(async move { Ok(response) })
        }));
        let host = Host::new();
        runtime().block_on(async {
            mount(&manager, &registry).await;
            sample(&manager, Some(&host)).await.unwrap();
            let _ = manager.shutdown_all().await;
        });
        let response = assert_single_reply(temp.path());
        assert_eq!(
            response["result"]["content"]["text"],
            "Replacement <pi-secret:redacted>"
        );
        assert_eq!(approvals.load(Ordering::SeqCst), 4);
        let requests = McpManager::lock(&host.requests);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].messages.len(), 1);
        assert_eq!(requests[0].messages[0].text, "Edited <pi-secret:redacted>");
        assert_eq!(requests[0].max_tokens, 31);
        assert_eq!(requests[0].temperature, Some(0.8));
    }
}

#[test]
fn owner_cancellation_releases_the_exact_prompt_or_reply_review() {
    for mode in ["stdio", "http", "sse"] {
        for review in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let (manager, _peer) = fixture(temp.path(), mode, "accept");
            let registry = SharedToolRegistry::new(ToolRegistry::without_builtins(None));
            let picker = registry.snapshot().host_ask_tool();
            let (sender, mut receiver) = asupersync::channel::mpsc::channel::<AskUiRequest>(4);
            picker.install_channel_ui(sender);
            let host = Host::new();
            let runtime = runtime();
            let owner = AgentCx::from_cx(runtime.request_cx_with_budget(Budget::new()));
            runtime.block_on(owner.with_current(async {
                mount(&manager, &registry).await;
                let cancelled_id = Arc::new(Mutex::new(None));
                let cancel = async {
                    let mut request = asupersync::time::timeout(
                        asupersync::time::wall_now(),
                        Duration::from_secs(7),
                        receiver.recv(owner.cx()),
                    ).await.unwrap().unwrap();
                    if review {
                        assert!(picker.respond_ui(&request.id, answer(&request.request, "Generate this reply")));
                        request = asupersync::time::timeout(
                            asupersync::time::wall_now(),
                            Duration::from_secs(7),
                            receiver.recv(owner.cx()),
                        ).await.unwrap().unwrap();
                        assert_eq!(request.request.questions[0].id.as_deref(), Some("mcp_sample_review"));
                    }
                    *McpManager::lock(&cancelled_id) = Some(request.id.clone());
                    owner.cancel_with(asupersync::types::CancelKind::User, Some("cancel sampling owner"));
                };
                let (result, ()) = futures::join!(sample(&manager, Some(&host)), cancel);
                assert!(result.is_err());
                let id = McpManager::lock(&cancelled_id).clone().unwrap();
                assert!(!picker.channel_ui_request_is_pending(&id));
                assert!(!picker.respond_ui(&id, AskResponse { answers: Vec::new(), dismissed: true }));
                assert_eq!(picker.close_channel_ui(), 0);
            }));
            assert_eq!(McpManager::lock(&host.requests).len(), usize::from(review));
            let requests = frames(temp.path());
            assert_eq!(requests.iter().filter(|frame| frame["method"] == "tools/call").count(), 1);
            assert!(!requests.iter().any(|frame| {
                frame["id"] == "sample-request" && frame.get("result").is_some()
            }));
        }
    }
}

#[test]
fn server_cancellation_drops_pending_generation_and_can_request_a_replacement() {
    for mode in ["stdio", "http", "http-get", "sse"] {
        let temp = tempfile::tempdir().unwrap();
        let (manager, _peer) = fixture(temp.path(), mode, "cancel-generation");
        let registry = SharedToolRegistry::new(ToolRegistry::without_builtins(None));
        registry.snapshot().host_ask_tool().set_handler(Arc::new(|request| {
            let response = answer(&request, if request.questions[0].id.as_deref() == Some("mcp_sample_prompt") {
                "Generate this reply"
            } else {
                "Send reply to server"
            });
            Box::pin(async move { Ok(response) })
        }));
        let mut host = Host::new();
        host.stall_first = Some(temp.path().join("sample-started"));
        runtime().block_on(async {
            mount(&manager, &registry).await;
            sample(&manager, Some(&host)).await.unwrap();
            let _ = manager.shutdown_all().await;
        });
        assert!(host.first_dropped.load(Ordering::SeqCst));
        assert_eq!(McpManager::lock(&host.requests).len(), 2);
        let response = assert_single_reply(temp.path());
        assert_eq!(response["result"]["content"]["type"], "text");
        let requests = frames(temp.path());
        let cancelled: Vec<_> = requests.iter().filter(|frame| frame["id"] == "cancelled-sample").collect();
        assert_eq!(cancelled.len(), 1);
        assert_eq!(cancelled[0]["error"]["code"], -1);
        assert!(cancelled[0].get("result").is_none());
    }
}

#[test]
fn changing_the_model_or_live_session_invalidates_an_approved_prompt_or_reply() {
    for mode in ["stdio", "http", "sse"] {
        for (changed, review) in [
            ("model", false),
            ("model", true),
            ("live-session", false),
            ("live-session", true),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let (manager, _peer) = fixture(temp.path(), mode, "accept");
            let registry = SharedToolRegistry::new(ToolRegistry::without_builtins(None));
            let host = Arc::new(Host::new());
            let changed_host = Arc::clone(&host);
            registry.snapshot().host_ask_tool().set_handler(Arc::new(move |request| {
                let is_review = request.questions[0].id.as_deref() == Some("mcp_sample_review");
                if is_review == review {
                    if changed == "model" {
                        changed_host.model_changed.store(true, Ordering::SeqCst);
                    } else {
                        // The frozen model/options descriptor stays identical;
                        // only the separately checked live session changes.
                        changed_host.live_session_changed.store(true, Ordering::SeqCst);
                    }
                }
                let response = answer(&request, if is_review { "Send reply to server" } else { "Generate this reply" });
                Box::pin(async move { Ok(response) })
            }));
            runtime().block_on(async {
                mount(&manager, &registry).await;
                sample(&manager, Some(host.as_ref())).await.unwrap();
                let _ = manager.shutdown_all().await;
            });
            let response = assert_single_reply(temp.path());
            assert_eq!(response["error"]["code"], -1);
            assert!(response.get("result").is_none());
            assert_eq!(McpManager::lock(&host.requests).len(), usize::from(review));
        }
    }
}

#[test]
fn sampling_shares_form_request_ids_and_requires_an_unambiguous_live_caller() {
    let registry = Arc::new(crate::mcp::elicitation::ElicitationRegistry::default());
    let params = json!({"messages":[{"role":"user","content":{"type":"text","text":"hello"}}],"maxTokens":1});
    let denied = |id: &str, generation| {
        assert_eq!(registry.route_sampling(&json!(id), Some(&params), generation).unwrap()["error"]["code"], -1);
    };
    denied("unsolicited", 7);
    let dispatch = Arc::new(RequestDispatch::new());
    let first = registry.subscribe(Arc::clone(&dispatch), 7, false).unwrap();
    denied("queued", 7);
    assert!(dispatch.claim());
    denied("old-generation", 8);
    let other_dispatch = Arc::new(RequestDispatch::new());
    assert!(other_dispatch.claim());
    let other = registry.subscribe(other_dispatch, 7, false).unwrap();
    denied("ambiguous", 7);
    drop(other);
    let form = json!({"message":"choose","requestedSchema":{"type":"object"}});
    assert!(registry.route(&json!("shared-id"), Some(&form), 7).is_none());
    assert!(registry.route_sampling(&json!("shared-id"), Some(&params), 7).is_none());
    assert!(registry.route_sampling(&json!("shared-id"), None, 7).is_none());
    denied("overlapping", 7);
    registry.cancel(Some(&json!({"requestId":"shared-id"})), 7);
    assert!(registry.route_sampling(&json!("replacement"), Some(&params), 7).is_none());
    assert!(registry.route(&json!("replacement"), Some(&json!({"mode":"url"})), 7).is_none());
    drop(first);
    denied("closed", 7);
}

#[test]
fn context_requests_never_acquire_the_tools_sampling_authority() {
    for mode in ["stdio", "http", "http-get", "sse"] {
        for method in ["resources/read", "prompts/get"] {
            let temp = tempfile::tempdir().unwrap();
            let (manager, _peer) = fixture(temp.path(), mode, "accept");
            let registry = SharedToolRegistry::new(ToolRegistry::without_builtins(None));
            registry.snapshot().host_ask_tool().set_handler(Arc::new(|_| {
                panic!("context forms do not grant context requests model authority")
            }));
            runtime().block_on(async {
                mount(&manager, &registry).await;
                if method == "resources/read" {
                    manager.read_resource("sampling-peer", "opaque://resource").await.unwrap();
                } else {
                    manager.get_prompt("sampling-peer", "prompt", None).await.unwrap();
                }
                let _ = manager.shutdown_all().await;
            });
            let requests = frames(temp.path());
            let replies: Vec<_> = requests.iter().filter(|frame| frame["id"] == "sample-request").collect();
            assert_eq!(replies.len(), 1);
            assert_eq!(replies[0]["error"]["code"], -1);
            assert!(!temp.path().join("peer-error").exists());
        }
    }

    let temp = tempfile::tempdir().unwrap();
    let (manager, _peer) = fixture(temp.path(), "stdio", "context-overlap");
    let registry = SharedToolRegistry::new(ToolRegistry::without_builtins(None));
    registry.snapshot().host_ask_tool().set_handler(Arc::new(|_| {
        panic!("an uncorrelated context request cannot borrow the tool's sampling approvals")
    }));
    let host = Host::new();
    runtime().block_on(async {
        mount(&manager, &registry).await;
        let (tool, resource) = futures::join!(
            sample(&manager, Some(&host)),
            manager.read_resource("sampling-peer", "opaque://resource"),
        );
        tool.unwrap();
        resource.unwrap();
        let _ = manager.shutdown_all().await;
    });
    assert!(McpManager::lock(&host.requests).is_empty());
    assert_eq!(assert_single_reply(temp.path())["error"]["code"], -1);
}
