//! Independent server peers exercise mounted tools, owner-bound prompts, and
//! client responses on stdio, POST SSE, GET SSE, and legacy SSE transports.

use super::*;
use crate::ask::{AskAnswer, AskResponse, AskUiRequest};
use crate::mcp::{ConfiguredServer, McpDiscovery, Provenance, TrustStore};
use crate::tools::{SharedToolRegistry, ToolRegistry};
use asupersync::{Budget, Cx};
use serde_json::json;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

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
form_reply = None

def record(frame):
    with log_lock:
        with (root / 'calls.jsonl').open('a') as log:
            log.write(json.dumps(frame) + '\n')

def prompt(ident='form-request'):
    params = {'message':'Profile\x1b[2J\n\u202e https://example.com',
              'requestedSchema':{'type':'object', 'properties':{
                  'name':{'type':'string','title':'Name','pattern':'^[A-Za-z]+$'}},
                  'required':['name']}}
    if scenario == 'task':
        params['_meta'] = {'io.modelcontextprotocol/related-task':{'taskId':'owned-form'}}
    if scenario == 'url':
        params.update(mode='url', url='https://example.com', elicitationId='url-form')
    if scenario == 'bad-schema':
        params['requestedSchema']['properties']['name'] = {'type':'object'}
    return {'jsonrpc':'2.0','id':ident,'method':'elicitation/create','params':params}

def exchange(frame, emit):
    global form_reply
    record(frame)
    method, ident = frame.get('method'), frame.get('id')
    if method is None:
        if ident == 'cancelled-form':
            assert frame['result']['action'] == 'cancel'
            return
        assert ident == 'form-request'
        assert not replied.is_set(), 'duplicate elicitation response'
        form_reply = frame
        replied.set()
        return
    if ident is None:
        return
    if method == 'initialize':
        assert frame['params']['capabilities']['elicitation'] == {'form':{}}
        assert 'sampling' not in frame['params']['capabilities']
        caps = {'tools':{}}
        if scenario == 'task':
            caps['tasks'] = {'requests':{'tools':{'call':{}}},'cancel':{}}
        value = {'protocolVersion':'2025-11-25','capabilities':caps,
                 'serverInfo':{'name':'form-peer','version':'1'}}
    elif method == 'tools/list':
        tool = {'name':'work','inputSchema':{'type':'object'},'description':'Interactive work'}
        if scenario == 'task':
            tool['execution'] = {'taskSupport':'required'}
        value = {'tools':[tool]}
    elif method == 'tools/call' and scenario == 'task':
        assert 'task' in frame['params']
        value = {'task':{'taskId':'owned-form','status':'input_required',
                        'createdAt':'2025-11-25T10:00:00Z',
                        'lastUpdatedAt':'2025-11-25T10:00:00Z','ttl':60000}}
        emit({'jsonrpc':'2.0','id':ident,'result':value})
        # The reader may consume this before the calling task binds its receipt.
        if mode != 'http':
            emit(prompt())
        return
    elif method in ('tools/call','tasks/result'):
        if scenario == 'server-cancel':
            emit(prompt('cancelled-form'))
            deadline = time.monotonic() + 5
            while not (root / 'first-visible').exists():
                assert time.monotonic() < deadline, 'first form never became visible'
                time.sleep(0.005)
            emit({'jsonrpc':'2.0','method':'notifications/cancelled','params':{'requestId':'cancelled-form'}})
        if scenario != 'task' or mode == 'http':
            emit(prompt())
        assert replied.wait(8), 'client never answered elicitation'
        value = {'content':[{'type':'text','text':'finished'}],
                 'structuredContent':{'form':form_reply}}
        if scenario == 'task':
            value['_meta'] = {'io.modelcontextprotocol/related-task':{'taskId':'owned-form'}}
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
        if frame.get('method') in ('tools/call','tasks/result'):
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
            if mode == 'http' and frame['method'] in ('tools/call','tasks/result'):
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
                self.send_header('Mcp-Session-Id','form-session')
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
    let script = root.join("elicitation_peer.py");
    std::fs::write(&script, PEER).unwrap();
    let args = vec!["-u".to_string(), script.display().to_string(), root.display().to_string(), mode.to_string(), scenario.to_string()];
    let mut peer = None;
    let url = if mode == "stdio" { None } else {
        peer = Some(Peer(Command::new("python3").args(&args).stdin(Stdio::null()).stdout(Stdio::null()).spawn().unwrap()));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !root.join("port").exists() {
            assert!(Instant::now() < deadline, "peer did not bind");
            assert!(peer.as_mut().unwrap().0.try_wait().unwrap().is_none());
            std::thread::sleep(Duration::from_millis(5));
        }
        Some(format!("http://127.0.0.1:{}/mcp", std::fs::read_to_string(root.join("port")).unwrap().trim()))
    };
    let manager = Arc::new(McpManager::new(root, root, McpDiscovery {
        servers: vec![ConfiguredServer {
            name: "form-peer".to_string(),
            command: (mode == "stdio").then(|| "python3".to_string()),
            args: if mode == "stdio" { args } else { Vec::new() },
            env: Vec::new(), url, headers: Vec::new(), oauth: None,
            transport_hint: Some(if mode == "http-get" { "http" } else { mode }.to_string()),
            enabled: true, cwd: None, timeout_ms: Some(5000),
            provenance: Provenance::ProjectPi, source_file: root.join("mcp.json"),
        }], warnings: Vec::new(),
    }));
    let entry = manager.entry("form-peer").unwrap();
    let fingerprint = manager.trust_fingerprint_for(&entry);
    let mut trust = TrustStore::load(&manager.inner.trust_path).unwrap();
    if let Some(identity) = entry.config.execution_identity(root).unwrap() {
        trust.acknowledge_execution("form-peer", &fingerprint, "operator", identity).unwrap();
    } else {
        trust.acknowledge("form-peer", &fingerprint, "operator").unwrap();
    }
    (manager, peer)
}

fn frames(root: &Path) -> Vec<Value> {
    std::fs::read_to_string(root.join("calls.jsonl")).unwrap_or_default()
        .lines().map(|line| serde_json::from_str(line).unwrap()).collect()
}

fn runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap()
}

#[test]
fn mounted_forms_validate_allow_edits_and_reply_on_all_native_transports() {
    for mode in ["stdio", "http", "http-get", "sse"] {
        for scenario in ["accept", "task"] {
            let temp = tempfile::tempdir().unwrap();
            let (manager, _peer) = fixture(temp.path(), mode, scenario);
            let registry = SharedToolRegistry::new(ToolRegistry::without_builtins(None));
            let picker = registry.snapshot().host_ask_tool();
            let fields = Arc::new(AtomicUsize::new(0));
            let reviews = Arc::new(AtomicUsize::new(0));
            let field_count = Arc::clone(&fields);
            let review_count = Arc::clone(&reviews);
            picker.set_handler(Arc::new(move |request| {
                assert!(Cx::current().is_some(), "UI must run under the tool owner");
                crate::ask::validate_request(request.clone()).unwrap();
                let question = &request.questions[0];
                assert!(!question.question.contains('\u{1b}'));
                assert!(!question.question.contains('\u{202e}'));
                assert!(!question.question.contains("https://"));
                assert!(question.question.contains("form-peer"));
                let review = question.id.as_deref() == Some("mcp_review");
                let answer = if review {
                    let prior = review_count.fetch_add(1, Ordering::Relaxed);
                    AskAnswer { question_id: "mcp_review".to_string(), selected: vec![if prior == 0 { "Edit answers" } else { "Submit these values" }.to_string()], other: None }
                } else {
                    let prior = field_count.fetch_add(1, Ordering::Relaxed);
                    AskAnswer { question_id: "mcp_field".to_string(), selected: Vec::new(), other: Some(match prior { 0 => "invalid1", 1 => "correct", _ => "changed" }.to_string()) }
                };
                Box::pin(async move { Ok(AskResponse { answers: vec![answer], dismissed: false }) })
            }));
            runtime().block_on(async {
                manager.refresh_tools("form-peer").await.unwrap();
                crate::mcp::reconcile_tools(&manager, &registry);
                let snapshot = registry.snapshot();
                assert!(snapshot.get("ask").is_none());
                let tool = snapshot.get("mcp__form-peer__work").unwrap();
                let output = tool.execute("owned-call", json!({}), None).await.unwrap();
                assert!(!output.is_error);
                let _ = manager.shutdown_all().await;
            });
            let requests = frames(temp.path());
            assert_eq!(requests.iter().filter(|frame| frame["method"] == "tools/call").count(), 1);
            let replies: Vec<_> = requests.iter().filter(|frame| frame["id"] == "form-request").collect();
            assert_eq!(replies.len(), 1, "{mode}/{scenario}");
            assert_eq!(replies[0]["result"]["action"], "accept");
            assert_eq!(replies[0]["result"]["content"], json!({"name":"changed"}));
            if scenario == "task" {
                assert_eq!(replies[0]["result"]["_meta"]["io.modelcontextprotocol/related-task"]["taskId"], "owned-form");
            }
            assert_eq!(fields.load(Ordering::Relaxed), 3);
            assert_eq!(reviews.load(Ordering::Relaxed), 2);
            assert!(!temp.path().join("peer-error").exists());
        }
    }
}

#[test]
fn headless_declined_and_unsupported_requests_return_no_user_content() {
    for mode in ["stdio", "http", "sse"] {
        for scenario in ["headless", "decline", "url", "bad-schema"] {
            let temp = tempfile::tempdir().unwrap();
            let (manager, _peer) = fixture(temp.path(), mode, scenario);
            let registry = SharedToolRegistry::new(ToolRegistry::without_builtins(None));
            if scenario == "decline" {
                registry.snapshot().host_ask_tool().set_handler(Arc::new(|request| {
                    let id = request.questions[0].id.clone().unwrap();
                    Box::pin(async move { Ok(AskResponse { answers: vec![AskAnswer { question_id: id, selected: vec!["Decline request".to_string()], other: None }], dismissed: false }) })
                }));
            }
            runtime().block_on(async {
                manager.refresh_tools("form-peer").await.unwrap();
                crate::mcp::reconcile_tools(&manager, &registry);
                registry.snapshot().get("mcp__form-peer__work").unwrap().execute("call", json!({}), None).await.unwrap();
                let _ = manager.shutdown_all().await;
            });
            let requests = frames(temp.path());
            let response = requests.iter().find(|frame| frame["id"] == "form-request").unwrap();
            if matches!(scenario, "url" | "bad-schema") {
                assert_eq!(response["error"]["code"], -32602);
            } else {
                assert_eq!(response["result"]["action"], if scenario == "decline" { "decline" } else { "cancel" });
                assert!(response["result"].get("content").is_none());
            }
        }
    }
}

#[test]
fn cancelling_the_tool_owner_releases_the_exact_pending_host_picker() {
    for mode in ["stdio", "http", "sse"] {
        let temp = tempfile::tempdir().unwrap();
        let (manager, _peer) = fixture(temp.path(), mode, "accept");
        let registry = SharedToolRegistry::new(ToolRegistry::without_builtins(None));
        let picker = registry.snapshot().host_ask_tool();
        let (sender, mut receiver) = asupersync::channel::mpsc::channel::<AskUiRequest>(4);
        picker.install_channel_ui(sender);
        let runtime = runtime();
        let owner = AgentCx::from_cx(runtime.request_cx_with_budget(Budget::new()));
        runtime.block_on(owner.with_current(async {
            manager.refresh_tools("form-peer").await.unwrap();
            crate::mcp::reconcile_tools(&manager, &registry);
            let snapshot = registry.snapshot();
            let tool = snapshot.get("mcp__form-peer__work").unwrap();
            let cancelled_id = Arc::new(Mutex::new(None));
            let cancel = async {
                let request = receiver.recv(owner.cx()).await.unwrap();
                *McpManager::lock(&cancelled_id) = Some(request.id.clone());
                owner.cancel_with(asupersync::types::CancelKind::User, Some("cancel form owner"));
            };
            let (result, ()) = futures::join!(tool.execute("cancelled", json!({}), None), cancel);
            assert!(result.is_err());
            let id = McpManager::lock(&cancelled_id).clone().unwrap();
            assert!(!picker.channel_ui_request_is_pending(&id));
            assert!(!picker.respond_ui(&id, AskResponse { answers: Vec::new(), dismissed: true }));
            assert_eq!(picker.close_channel_ui(), 0);
        }));
        assert_eq!(frames(temp.path()).iter().filter(|frame| frame["method"] == "tools/call").count(), 1);
    }
}

#[test]
fn server_can_cancel_a_visible_form_and_continue_with_another_in_the_same_tool() {
    for mode in ["stdio", "http", "http-get", "sse"] {
        let temp = tempfile::tempdir().unwrap();
        let (manager, _peer) = fixture(temp.path(), mode, "server-cancel");
        let registry = SharedToolRegistry::new(ToolRegistry::without_builtins(None));
        let picker = registry.snapshot().host_ask_tool();
        let (sender, mut receiver) = asupersync::channel::mpsc::channel::<AskUiRequest>(4);
        picker.install_channel_ui(sender);
        runtime().block_on(async {
            manager.refresh_tools("form-peer").await.unwrap();
            crate::mcp::reconcile_tools(&manager, &registry);
            let snapshot = registry.snapshot();
            let tool = snapshot.get("mcp__form-peer__work").unwrap();
            let ui = async {
                let owner = AgentCx::for_current_or_request();
                let first = receiver.recv(owner.cx()).await.unwrap();
                assert!(picker.channel_ui_request_is_pending(&first.id));
                std::fs::write(temp.path().join("first-visible"), "visible").unwrap();
                let second = receiver.recv(owner.cx()).await.unwrap();
                assert!(!picker.channel_ui_request_is_pending(&first.id));
                assert!(picker.respond_ui(&second.id, AskResponse {
                    answers: vec![AskAnswer { question_id: "mcp_field".to_string(), selected: Vec::new(), other: Some("continued".to_string()) }], dismissed: false,
                }));
                let review = receiver.recv(owner.cx()).await.unwrap();
                assert!(picker.respond_ui(&review.id, AskResponse {
                    answers: vec![AskAnswer { question_id: "mcp_review".to_string(), selected: vec!["Submit these values".to_string()], other: None }], dismissed: false,
                }));
            };
            let (output, ()) = futures::join!(tool.execute("continued-call", json!({}), None), ui);
            assert!(!output.unwrap().is_error);
            assert_eq!(picker.close_channel_ui(), 0);
            let _ = manager.shutdown_all().await;
        });
        let requests = frames(temp.path());
        assert_eq!(requests.iter().filter(|frame| frame["method"] == "tools/call").count(), 1);
        let cancelled: Vec<_> = requests.iter().filter(|frame| frame["id"] == "cancelled-form").collect();
        assert_eq!(cancelled.len(), 1);
        let accepted: Vec<_> = requests.iter().filter(|frame| frame["id"] == "form-request").collect();
        assert_eq!(accepted.len(), 1);
        assert_eq!(accepted[0]["result"]["content"], json!({"name":"continued"}));
        assert!(!temp.path().join("peer-error").exists());
    }
}
