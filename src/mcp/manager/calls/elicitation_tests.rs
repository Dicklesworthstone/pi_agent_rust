//! Independent server peers exercise mounted tools and context, owner-bound prompts, and
//! client responses on stdio, POST SSE, GET SSE, and legacy SSE transports.

use super::*;
use crate::ask::{AskAnswer, AskResponse, AskUiRequest};
use crate::mcp::{ConfiguredServer, McpDiscovery, Provenance, TrustStore};
use crate::tools::Tool as _;
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
        assert frame['params']['capabilities']['sampling'] == {}
        caps = {'tools':{}}
        if scenario == 'task':
            caps['tasks'] = {'requests':{'tools':{'call':{}}},'cancel':{}}
        value = {'protocolVersion':'2025-11-25','capabilities':caps,
                 'serverInfo':{'name':'form-peer','version':'1'}}
    elif method == 'tools/list':
        if scenario == 'catalog-form' or (scenario == 'catalog-overlap' and (root / 'allow-catalog-form').exists()):
            if scenario == 'catalog-overlap':
                deadline = time.monotonic() + 5
                while not (root / 'tool-dispatched').exists():
                    assert time.monotonic() < deadline, 'tool never dispatched'
                    time.sleep(0.005)
            emit(prompt())
            assert replied.wait(8), 'client never answered catalog elicitation'
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
    elif method in ('tools/call','tasks/result','resources/read','prompts/get',
                    'resources/list','resources/templates/list','prompts/list','completion/complete'):
        if scenario == 'server-cancel':
            emit(prompt('cancelled-form'))
            deadline = time.monotonic() + 5
            while not (root / 'first-visible').exists():
                assert time.monotonic() < deadline, 'first form never became visible'
                time.sleep(0.005)
            emit({'jsonrpc':'2.0','method':'notifications/cancelled','params':{'requestId':'cancelled-form'}})
        if scenario in ('context-overlap','catalog-overlap') and method == 'tools/call':
            (root / 'tool-dispatched').write_text('ready')
        elif scenario == 'context-overlap':
            deadline = time.monotonic() + 5
            while not (root / 'tool-dispatched').exists():
                assert time.monotonic() < deadline, 'tool never dispatched'
                time.sleep(0.005)
            emit(prompt())
        elif scenario != 'task' or mode == 'http':
            emit(prompt())
        assert replied.wait(8), 'client never answered elicitation'
        if method == 'resources/read':
            value = {'contents':[{'uri':frame['params']['uri'],'text':'resource ready'}]}
        elif method == 'prompts/get':
            value = {'messages':[{'role':'user','content':{'type':'text','text':'prompt ready'}}]}
        elif method == 'resources/list':
            value = {'resources':[]}
        elif method == 'resources/templates/list':
            value = {'resourceTemplates':[]}
        elif method == 'prompts/list':
            value = {'prompts':[]}
        elif method == 'completion/complete':
            value = {'completion':{'values':[]}}
        else:
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
        if frame.get('method') in ('tools/list','tools/call','tasks/result','resources/read','prompts/get',
                                  'resources/list','resources/templates/list','prompts/list','completion/complete'):
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
                self.empty(404 if scenario == 'reply-session-expired' and 'method' not in frame else 202)
                return
            if scenario in ('context-session-expired','context-auth-required') and frame['method'] in ('resources/read','prompts/get'):
                record(frame)
                self.empty(404 if scenario == 'context-session-expired' else 401)
                return
            if mode == 'sse':
                self.empty(202)
                threading.Thread(target=safe_exchange,args=(frame,lambda value: events.put(value,timeout=1)),daemon=True).start()
                return
            if mode == 'http' and (frame['method'] in ('tools/call','tasks/result','resources/read','prompts/get',
                                                     'resources/list','resources/templates/list','prompts/list','completion/complete') or
                                   (frame['method'] == 'tools/list' and scenario in ('catalog-form','catalog-overlap'))):
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
            assert!(Instant::now() < deadline, "peer did not bind");
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
                name: "form-peer".to_string(),
                command: (mode == "stdio").then(|| "python3".to_string()),
                args: if mode == "stdio" { args } else { Vec::new() },
                env: Vec::new(),
                url,
                headers: Vec::new(),
                oauth: None,
                transport_hint: Some(if mode == "http-get" { "http" } else { mode }.to_string()),
                enabled: true,
                cwd: None,
                timeout_ms: Some(if scenario == "deadline" { 500 } else { 5000 }),
                provenance: Provenance::ProjectPi,
                source_file: root.join("mcp.json"),
            }],
            warnings: Vec::new(),
        },
    ));
    let entry = manager.entry("form-peer").unwrap();
    let fingerprint = manager.trust_fingerprint_for(&entry);
    let mut trust = TrustStore::load(&manager.inner.trust_path).unwrap();
    if let Some(identity) = entry.config.execution_identity(root).unwrap() {
        trust
            .acknowledge_execution("form-peer", &fingerprint, "operator", identity)
            .unwrap();
    } else {
        trust
            .acknowledge("form-peer", &fingerprint, "operator")
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
                    AskAnswer {
                        question_id: "mcp_review".to_string(),
                        selected: vec![
                            if prior == 0 {
                                "Edit answers"
                            } else {
                                "Submit these values"
                            }
                            .to_string(),
                        ],
                        other: None,
                    }
                } else {
                    let prior = field_count.fetch_add(1, Ordering::Relaxed);
                    AskAnswer {
                        question_id: "mcp_field".to_string(),
                        selected: Vec::new(),
                        other: Some(
                            match prior {
                                0 => "invalid1",
                                1 => "correct",
                                _ => "changed",
                            }
                            .to_string(),
                        ),
                    }
                };
                Box::pin(async move {
                    Ok(AskResponse {
                        answers: vec![answer],
                        dismissed: false,
                    })
                })
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
            assert_eq!(
                requests
                    .iter()
                    .filter(|frame| frame["method"] == "tools/call")
                    .count(),
                1
            );
            let replies: Vec<_> = requests
                .iter()
                .filter(|frame| frame["id"] == "form-request")
                .collect();
            assert_eq!(replies.len(), 1, "{mode}/{scenario}");
            assert_eq!(replies[0]["result"]["action"], "accept");
            assert_eq!(replies[0]["result"]["content"], json!({"name":"changed"}));
            if scenario == "task" {
                assert_eq!(
                    replies[0]["result"]["_meta"]["io.modelcontextprotocol/related-task"]["taskId"],
                    "owned-form"
                );
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
                registry
                    .snapshot()
                    .host_ask_tool()
                    .set_handler(Arc::new(|request| {
                        let id = request.questions[0].id.clone().unwrap();
                        Box::pin(async move {
                            Ok(AskResponse {
                                answers: vec![AskAnswer {
                                    question_id: id,
                                    selected: vec!["Decline request".to_string()],
                                    other: None,
                                }],
                                dismissed: false,
                            })
                        })
                    }));
            }
            runtime().block_on(async {
                manager.refresh_tools("form-peer").await.unwrap();
                crate::mcp::reconcile_tools(&manager, &registry);
                registry
                    .snapshot()
                    .get("mcp__form-peer__work")
                    .unwrap()
                    .execute("call", json!({}), None)
                    .await
                    .unwrap();
                let _ = manager.shutdown_all().await;
            });
            let requests = frames(temp.path());
            let response = requests
                .iter()
                .find(|frame| frame["id"] == "form-request")
                .unwrap();
            if matches!(scenario, "url" | "bad-schema") {
                assert_eq!(response["error"]["code"], -32602);
            } else {
                assert_eq!(
                    response["result"]["action"],
                    if scenario == "decline" {
                        "decline"
                    } else {
                        "cancel"
                    }
                );
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
                owner.cancel_with(
                    asupersync::types::CancelKind::User,
                    Some("cancel form owner"),
                );
            };
            let (result, ()) = futures::join!(tool.execute("cancelled", json!({}), None), cancel);
            assert!(result.is_err());
            let id = McpManager::lock(&cancelled_id).clone().unwrap();
            assert!(!picker.channel_ui_request_is_pending(&id));
            assert!(!picker.respond_ui(
                &id,
                AskResponse {
                    answers: Vec::new(),
                    dismissed: true
                }
            ));
            assert_eq!(picker.close_channel_ui(), 0);
        }));
        assert_eq!(
            frames(temp.path())
                .iter()
                .filter(|frame| frame["method"] == "tools/call")
                .count(),
            1
        );
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
                assert!(picker.respond_ui(
                    &second.id,
                    AskResponse {
                        answers: vec![AskAnswer {
                            question_id: "mcp_field".to_string(),
                            selected: Vec::new(),
                            other: Some("continued".to_string())
                        }],
                        dismissed: false,
                    }
                ));
                let review = receiver.recv(owner.cx()).await.unwrap();
                assert!(picker.respond_ui(
                    &review.id,
                    AskResponse {
                        answers: vec![AskAnswer {
                            question_id: "mcp_review".to_string(),
                            selected: vec!["Submit these values".to_string()],
                            other: None
                        }],
                        dismissed: false,
                    }
                ));
            };
            let (output, ()) = futures::join!(tool.execute("continued-call", json!({}), None), ui);
            assert!(!output.unwrap().is_error);
            assert_eq!(picker.close_channel_ui(), 0);
            let _ = manager.shutdown_all().await;
        });
        let requests = frames(temp.path());
        assert_eq!(
            requests
                .iter()
                .filter(|frame| frame["method"] == "tools/call")
                .count(),
            1
        );
        let cancelled: Vec<_> = requests
            .iter()
            .filter(|frame| frame["id"] == "cancelled-form")
            .collect();
        assert_eq!(cancelled.len(), 1);
        let accepted: Vec<_> = requests
            .iter()
            .filter(|frame| frame["id"] == "form-request")
            .collect();
        assert_eq!(accepted.len(), 1);
        assert_eq!(
            accepted[0]["result"]["content"],
            json!({"name":"continued"})
        );
        assert!(!temp.path().join("peer-error").exists());
    }
}

async fn request_context(manager: &McpManager, method: &str) -> Result<Value> {
    match method {
        "resources/read" => {
            manager
                .read_resource("form-peer", "opaque://chosen-resource")
                .await
        }
        "prompts/get" => manager.get_prompt("form-peer", "chosen-prompt", None).await,
        "resources/list" => manager.list_resources("form-peer", None).await,
        "resources/templates/list" => manager.list_resource_templates("form-peer", None).await,
        "prompts/list" => manager.list_prompts("form-peer", None).await,
        "completion/complete" => {
            manager
                .complete_argument(
                    "form-peer",
                    &crate::mcp::McpCompletionReference::Prompt {
                        name: "chosen-prompt".to_string(),
                    },
                    "name",
                    "",
                    None,
                )
                .await
        }
        _ => panic!("unexpected context method"),
    }
}

fn accept_context_form(request: &crate::ask::AskRequest) -> AskResponse {
    let question = &request.questions[0];
    assert!(question.question.contains("form-peer"));
    let review = question.id.as_deref() == Some("mcp_review");
    AskResponse {
        answers: vec![AskAnswer {
            question_id: question.id.clone().unwrap(),
            selected: if review {
                vec!["Submit these values".to_string()]
            } else {
                Vec::new()
            },
            other: (!review).then(|| "Ada".to_string()),
        }],
        dismissed: false,
    }
}

#[test]
fn mounted_resource_and_prompt_forms_reply_without_deadlocking_native_request_lanes() {
    for mode in ["stdio", "http", "http-get", "sse"] {
        for (method, arguments) in [
            (
                "resources/read",
                json!({"action":"read_resource","uri":"opaque://chosen-resource"}),
            ),
            (
                "prompts/get",
                json!({"action":"get_prompt","name":"chosen-prompt"}),
            ),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let (manager, _peer) = fixture(temp.path(), mode, "accept");
            let registry = SharedToolRegistry::new(ToolRegistry::without_builtins(None));
            let shown = Arc::new(AtomicUsize::new(0));
            let ui_shown = Arc::clone(&shown);
            registry
                .snapshot()
                .host_ask_tool()
                .set_handler(Arc::new(move |request| {
                    assert!(Cx::current().is_some(), "context forms retain their owner");
                    ui_shown.fetch_add(1, Ordering::SeqCst);
                    let answer = accept_context_form(&request);
                    Box::pin(async move { Ok(answer) })
                }));
            runtime().block_on(async {
                manager.refresh_tools("form-peer").await.unwrap();
                crate::mcp::reconcile_tools(&manager, &registry);
                let snapshot = registry.snapshot();
                assert!(snapshot.get("ask").is_none());
                let name = crate::mcp::McpContextTool::new("form-peer", Arc::clone(&manager))
                    .name()
                    .to_string();
                let output = snapshot
                    .get(&name)
                    .unwrap()
                    .execute("context-form", arguments, None)
                    .await
                    .unwrap();
                assert!(!output.is_error);
                let _ = manager.shutdown_all().await;
            });
            let requests = frames(temp.path());
            assert_eq!(
                requests
                    .iter()
                    .filter(|frame| frame["method"] == method)
                    .count(),
                1,
                "{mode}/{method}"
            );
            let replies: Vec<_> = requests
                .iter()
                .filter(|frame| frame["id"] == "form-request")
                .collect();
            assert_eq!(replies.len(), 1);
            assert_eq!(
                replies[0]["result"],
                json!({"action":"accept","content":{"name":"Ada"}})
            );
            assert_eq!(shown.load(Ordering::SeqCst), 2, "field and explicit review");
            assert!(!temp.path().join("peer-error").exists());
        }
    }
}

#[test]
fn context_forms_cancel_headless_and_dismissed_ui_and_preserve_explicit_decline() {
    for mode in ["stdio", "http"] {
        for method in ["resources/read", "prompts/get"] {
            for scenario in ["headless", "dismissed", "decline"] {
                let temp = tempfile::tempdir().unwrap();
                let (manager, _peer) = fixture(temp.path(), mode, scenario);
                let registry = SharedToolRegistry::new(ToolRegistry::without_builtins(None));
                if scenario != "headless" {
                    registry
                        .snapshot()
                        .host_ask_tool()
                        .set_handler(Arc::new(move |request| {
                            let id = request.questions[0].id.clone().unwrap();
                            Box::pin(async move {
                                Ok(AskResponse {
                                    answers: if scenario == "decline" {
                                        vec![AskAnswer {
                                            question_id: id,
                                            selected: vec!["Decline request".to_string()],
                                            other: None,
                                        }]
                                    } else {
                                        Vec::new()
                                    },
                                    dismissed: scenario == "dismissed",
                                })
                            })
                        }));
                }
                runtime().block_on(async {
                    manager.refresh_tools("form-peer").await.unwrap();
                    crate::mcp::reconcile_tools(&manager, &registry);
                    request_context(&manager, method).await.unwrap();
                    let _ = manager.shutdown_all().await;
                });
                let requests = frames(temp.path());
                let reply = requests
                    .iter()
                    .find(|frame| frame["id"] == "form-request")
                    .unwrap();
                assert_eq!(
                    reply["result"]["action"],
                    if scenario == "decline" {
                        "decline"
                    } else {
                        "cancel"
                    }
                );
                assert!(reply["result"].get("content").is_none());
                assert_eq!(
                    requests
                        .iter()
                        .filter(|frame| frame["method"] == method)
                        .count(),
                    1
                );
            }
        }
    }
}

#[test]
fn context_discovery_and_completion_never_open_an_installed_picker() {
    for mode in ["stdio", "http"] {
        for method in [
            "resources/list",
            "resources/templates/list",
            "prompts/list",
            "completion/complete",
        ] {
            let temp = tempfile::tempdir().unwrap();
            let (manager, _peer) = fixture(temp.path(), mode, "accept");
            let registry = SharedToolRegistry::new(ToolRegistry::without_builtins(None));
            registry
                .snapshot()
                .host_ask_tool()
                .set_handler(Arc::new(|_| {
                    panic!("discovery and completion must not ask for server data")
                }));
            runtime().block_on(async {
                manager.refresh_tools("form-peer").await.unwrap();
                crate::mcp::reconcile_tools(&manager, &registry);
                request_context(&manager, method).await.unwrap();
                let _ = manager.shutdown_all().await;
            });
            let requests = frames(temp.path());
            let reply = requests
                .iter()
                .find(|frame| frame["id"] == "form-request")
                .unwrap();
            assert_eq!(reply["result"], json!({"action":"cancel"}));
        }
    }
}

#[test]
fn context_cancellation_and_deadline_release_the_exact_pending_picker() {
    for mode in ["stdio", "http"] {
        for method in ["resources/read", "prompts/get"] {
            for scenario in ["cancel", "deadline"] {
                let temp = tempfile::tempdir().unwrap();
                let (manager, _peer) = fixture(temp.path(), mode, scenario);
                let registry = SharedToolRegistry::new(ToolRegistry::without_builtins(None));
                let picker = registry.snapshot().host_ask_tool();
                let (sender, mut receiver) = asupersync::channel::mpsc::channel::<AskUiRequest>(4);
                picker.install_channel_ui(sender);
                let runtime = runtime();
                let owner = AgentCx::from_cx(runtime.request_cx_with_budget(Budget::new()));
                runtime.block_on(async {
                    manager.refresh_tools("form-peer").await.unwrap();
                    crate::mcp::reconcile_tools(&manager, &registry);
                    let ui = async {
                        let request = receiver.recv(owner.cx()).await.unwrap();
                        assert!(picker.channel_ui_request_is_pending(&request.id));
                        if scenario == "cancel" {
                            owner.cancel_with(
                                asupersync::types::CancelKind::User,
                                Some("cancel context form"),
                            );
                        }
                        request.id
                    };
                    let (result, id) =
                        futures::join!(owner.with_current(request_context(&manager, method)), ui);
                    let error = result.expect_err("abandoned context cannot publish a result");
                    assert!(error.to_string().contains(if scenario == "cancel" {
                        "MCP_CANCELLED"
                    } else {
                        "MCP_TIMEOUT"
                    }));
                    assert!(!picker.channel_ui_request_is_pending(&id));
                    assert!(!picker.respond_ui(
                        &id,
                        AskResponse {
                            answers: Vec::new(),
                            dismissed: true
                        }
                    ));
                    assert_eq!(picker.close_channel_ui(), 0);
                    let _ = manager.shutdown_all().await;
                });
                let requests = frames(temp.path());
                assert_eq!(
                    requests
                        .iter()
                        .filter(|frame| frame["method"] == method)
                        .count(),
                    1
                );
                assert!(
                    !requests.iter().any(|frame| frame["id"] == "form-request"
                        && frame["result"]["action"] == "accept")
                );
            }
        }
    }
}

#[test]
fn resumed_context_form_keeps_its_initiating_owner_on_later_polls() {
    use std::future::Future as _;
    use std::task::Context;

    for mode in ["stdio", "http"] {
        let temp = tempfile::tempdir().unwrap();
        let (manager, _peer) = fixture(temp.path(), mode, "accept");
        let registry = SharedToolRegistry::new(ToolRegistry::without_builtins(None));
        registry
            .snapshot()
            .host_ask_tool()
            .set_handler(Arc::new(|request| {
                assert_eq!(Cx::current().unwrap().budget().poll_quota, 321);
                let answer = accept_context_form(&request);
                Box::pin(async move { Ok(answer) })
            }));
        let runtime = runtime();
        let owner =
            AgentCx::from_cx(runtime.request_cx_with_budget(Budget::new().with_poll_quota(321)));
        runtime.block_on(async {
            manager.refresh_tools("form-peer").await.unwrap();
            crate::mcp::reconcile_tools(&manager, &registry);
            let ambient = Cx::current().unwrap().budget();
            let mut request =
                Box::pin(manager.read_resource("form-peer", "opaque://chosen-resource"));
            {
                let _owner = owner.cx().clone().set_current_restricted();
                let mut task = Context::from_waker(futures::task::noop_waker_ref());
                assert!(request.as_mut().poll(&mut task).is_pending());
            }
            assert!(request.await.is_ok());
            assert_eq!(Cx::current().unwrap().budget(), ambient);
            let _ = manager.shutdown_all().await;
        });
        let requests = frames(temp.path());
        assert_eq!(
            requests
                .iter()
                .filter(|frame| frame["method"] == "resources/read")
                .count(),
            1
        );
        assert_eq!(
            requests
                .iter()
                .filter(
                    |frame| frame["id"] == "form-request" && frame["result"]["action"] == "accept"
                )
                .count(),
            1
        );
    }
}

#[test]
fn simultaneous_discovery_form_cannot_borrow_an_active_tools_picker() {
    for scenario in ["context-overlap", "catalog-overlap"] {
        let temp = tempfile::tempdir().unwrap();
        let (manager, _peer) = fixture(temp.path(), "stdio", scenario);
        let registry = SharedToolRegistry::new(ToolRegistry::without_builtins(None));
        registry
            .snapshot()
            .host_ask_tool()
            .set_handler(Arc::new(|_| {
                panic!("an uncorrelated discovery form must not borrow another request's picker")
            }));
        runtime().block_on(async {
            manager.refresh_tools("form-peer").await.unwrap();
            crate::mcp::reconcile_tools(&manager, &registry);
            std::fs::write(temp.path().join("allow-catalog-form"), "ready").unwrap();
            let snapshot = registry.snapshot();
            let tool = snapshot.get("mcp__form-peer__work").unwrap();
            let discovery = async {
                if scenario == "catalog-overlap" {
                    manager
                        .refresh_tools("form-peer")
                        .await
                        .map(|_| Value::Null)
                } else {
                    manager.list_resources("form-peer", None).await
                }
            };
            let (tool_result, list_result) =
                futures::join!(tool.execute("concurrent-tool", json!({}), None), discovery,);
            assert!(!tool_result.unwrap().is_error);
            assert!(list_result.is_ok());
            let _ = manager.shutdown_all().await;
        });
        let requests = frames(temp.path());
        let reply = requests
            .iter()
            .find(|frame| frame["id"] == "form-request")
            .unwrap();
        assert_eq!(reply["result"], json!({"action":"cancel"}));
        assert!(!temp.path().join("peer-error").exists());
    }
}

#[test]
fn expired_http_context_or_form_reply_is_never_reinitialized_or_replayed() {
    for method in ["resources/read", "prompts/get"] {
        for scenario in [
            "context-session-expired",
            "context-auth-required",
            "reply-session-expired",
        ] {
            let temp = tempfile::tempdir().unwrap();
            let (manager, _peer) = fixture(temp.path(), "http", scenario);
            let registry = SharedToolRegistry::new(ToolRegistry::without_builtins(None));
            registry
                .snapshot()
                .host_ask_tool()
                .set_handler(Arc::new(|request| {
                    let answer = accept_context_form(&request);
                    Box::pin(async move { Ok(answer) })
                }));
            runtime().block_on(async {
                manager.refresh_tools("form-peer").await.unwrap();
                crate::mcp::reconcile_tools(&manager, &registry);
                let error = request_context(&manager, method)
                    .await
                    .expect_err("failed context request");
                let expected = match scenario {
                    "context-session-expired" => "MCP_SESSION_EXPIRED",
                    "context-auth-required" => "MCP_AUTH_REQUIRED",
                    _ => "MCP_DELIVERY_INDETERMINATE",
                };
                assert!(
                    error.to_string().contains(expected),
                    "{method}/{scenario}: {error}"
                );
                let _ = manager.shutdown_all().await;
            });
            let requests = frames(temp.path());
            assert_eq!(
                requests
                    .iter()
                    .filter(|frame| frame["method"] == "initialize")
                    .count(),
                1,
                "{method}/{scenario}"
            );
            assert_eq!(
                requests
                    .iter()
                    .filter(|frame| frame["method"] == method)
                    .count(),
                1
            );
            if scenario == "reply-session-expired" {
                let replies: Vec<_> = requests
                    .iter()
                    .filter(|frame| frame["id"] == "form-request")
                    .collect();
                assert_eq!(
                    replies.len(),
                    1,
                    "uncertain form delivery must not be repeated"
                );
                assert_eq!(replies[0]["result"]["action"], "accept");
            }
        }
    }
}

#[test]
fn requests_without_an_available_form_ownership_slot_are_not_dispatched() {
    for mode in ["stdio", "http"] {
        for method in [
            "resources/read",
            "prompts/get",
            "resources/list",
            "tools/call",
            "tools/list",
        ] {
            let temp = tempfile::tempdir().unwrap();
            let (manager, _peer) = fixture(temp.path(), mode, "headless");
            runtime().block_on(async {
                manager.refresh_tools("form-peer").await.unwrap();
                let entry = manager.entry("form-peer").unwrap();
                let transport = McpManager::lock(&entry.transport).clone().unwrap();
                let registry = transport.elicitation_registry().unwrap();
                let before = frames(temp.path())
                    .iter()
                    .filter(|frame| frame["method"] == method)
                    .count();
                let slots: Vec<_> = (0..64)
                    .map(|_| {
                        registry
                            .subscribe(
                                Arc::new(RequestDispatch::new()),
                                transport.progress_generation(),
                                false,
                            )
                            .unwrap()
                    })
                    .collect();
                let request = || async {
                    if method == "tools/call" {
                        manager.call_tool("form-peer", "work", json!({})).await
                    } else if method == "tools/list" {
                        manager
                            .refresh_tools("form-peer")
                            .await
                            .map(|_| Value::Null)
                    } else {
                        request_context(&manager, method).await
                    }
                };
                let error = request().await.expect_err("slot limit");
                assert!(error.to_string().contains("MCP_REQUEST_LIMIT"));
                assert!(transport.is_alive());
                assert_eq!(McpManager::lock(&entry.restarts).count, 0);
                assert_eq!(
                    frames(temp.path())
                        .iter()
                        .filter(|frame| frame["method"] == method)
                        .count(),
                    before,
                );
                drop(slots);
                request().await.unwrap();
                let _ = manager.shutdown_all().await;
            });
            assert_eq!(
                frames(temp.path())
                    .iter()
                    .filter(|frame| frame["method"] == method)
                    .count(),
                if method == "tools/list" { 2 } else { 1 }
            );
        }
    }
}

#[test]
fn catalog_discovery_answers_forms_without_opening_a_picker_on_all_native_transports() {
    for mode in ["stdio", "http", "http-get", "sse"] {
        let temp = tempfile::tempdir().unwrap();
        let (manager, _peer) = fixture(temp.path(), mode, "catalog-form");
        let registry = SharedToolRegistry::new(ToolRegistry::without_builtins(None));
        registry
            .snapshot()
            .host_ask_tool()
            .set_handler(Arc::new(|_| {
                panic!("catalog discovery must not open a picker")
            }));
        crate::mcp::reconcile_tools(&manager, &registry);
        runtime().block_on(async {
            assert_eq!(manager.refresh_tools("form-peer").await.unwrap().len(), 1);
            let _ = manager.shutdown_all().await;
        });
        let requests = frames(temp.path());
        let reply = requests
            .iter()
            .find(|frame| frame["id"] == "form-request")
            .unwrap();
        assert_eq!(reply["result"], json!({"action":"cancel"}));
        assert_eq!(
            requests
                .iter()
                .filter(|frame| frame["method"] == "tools/list")
                .count(),
            1
        );
        assert!(!temp.path().join("peer-error").exists());
    }
}

#[test]
fn abandoning_queued_http_catalog_discovery_preserves_an_active_context_form() {
    use std::future::Future as _;
    use std::task::Context;

    for abandon in ["cancel", "drop"] {
        let temp = tempfile::tempdir().unwrap();
        let (manager, _peer) = fixture(temp.path(), "http", "accept");
        let registry = SharedToolRegistry::new(ToolRegistry::without_builtins(None));
        let picker = registry.snapshot().host_ask_tool();
        let (sender, mut receiver) = asupersync::channel::mpsc::channel::<AskUiRequest>(4);
        picker.install_channel_ui(sender);
        let runtime = runtime();
        let catalog_owner = AgentCx::from_cx(runtime.request_cx_with_budget(Budget::new()));
        runtime.block_on(async {
            manager.refresh_tools("form-peer").await.unwrap();
            crate::mcp::reconcile_tools(&manager, &registry);
            let entry = manager.entry("form-peer").unwrap();
            let transport = McpManager::lock(&entry.transport).clone().unwrap();
            let ui = async {
                let owner = AgentCx::for_current_or_request();
                let field = receiver.recv(owner.cx()).await.unwrap();
                let mut discovery =
                    Box::pin(catalog_owner.with_current(manager.refresh_tools("form-peer")));
                {
                    let mut task = Context::from_waker(futures::task::noop_waker_ref());
                    assert!(discovery.as_mut().poll(&mut task).is_pending());
                }
                if abandon == "cancel" {
                    catalog_owner.cancel_with(
                        asupersync::types::CancelKind::User,
                        Some("cancel queued discovery"),
                    );
                    let error = discovery.await.expect_err("queued discovery cancelled");
                    assert!(error.to_string().contains("MCP_CANCELLED"));
                } else {
                    drop(discovery);
                }
                assert!(
                    transport.is_alive(),
                    "a queued caller must not close the form's connection"
                );
                assert_eq!(McpManager::lock(&entry.restarts).count, 0);
                assert!(picker.channel_ui_request_is_pending(&field.id));
                assert!(picker.respond_ui(
                    &field.id,
                    AskResponse {
                        answers: vec![AskAnswer {
                            question_id: "mcp_field".to_string(),
                            selected: Vec::new(),
                            other: Some("Ada".to_string())
                        }],
                        dismissed: false,
                    }
                ));
                let review = receiver.recv(owner.cx()).await.unwrap();
                assert!(picker.respond_ui(
                    &review.id,
                    AskResponse {
                        answers: vec![AskAnswer {
                            question_id: "mcp_review".to_string(),
                            selected: vec!["Submit these values".to_string()],
                            other: None
                        }],
                        dismissed: false,
                    }
                ));
            };
            let (result, ()) = futures::join!(
                manager.read_resource("form-peer", "opaque://chosen-resource"),
                ui
            );
            assert!(result.is_ok(), "{abandon}: {result:?}");
            assert_eq!(picker.close_channel_ui(), 0);
            let _ = manager.shutdown_all().await;
        });
        let requests = frames(temp.path());
        assert_eq!(
            requests
                .iter()
                .filter(|frame| frame["method"] == "tools/list")
                .count(),
            1,
            "queued discovery was never sent"
        );
        let reply = requests
            .iter()
            .find(|frame| frame["id"] == "form-request")
            .unwrap();
        assert_eq!(
            reply["result"],
            json!({"action":"accept","content":{"name":"Ada"}})
        );
        assert!(!temp.path().join("peer-error").exists());
    }
}
