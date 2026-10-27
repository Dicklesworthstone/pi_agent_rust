//! Mounted tools and native transports against an independent MCP task peer.

use super::*;
use crate::mcp::{ConfiguredServer, McpDiscovery, McpTool, Provenance, TrustStore};
use crate::tools::Tool;
use asupersync::{Budget, Cx};
use serde_json::json;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

const PEER: &str = r"
import json, pathlib, sys, threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

root = pathlib.Path(sys.argv[1])
mode, scenario = sys.argv[2:4]
lock = threading.Lock()
token = None
polls = 0

def task(status='working', ident='owned-task'):
    return {'taskId':ident, 'status':status, 'createdAt':'2025-11-25T10:00:00Z',
            'lastUpdatedAt':'2025-11-25T10:00:01Z', 'ttl':60000,
            'pollInterval':60000 if scenario == 'timeout' else 1,
            'statusMessage':'stage\x1b[2J\n\u202e'}

def result():
    value = {'content':[{'type':'text','text':'finished'}],
             'structuredContent':{'count':'wrong' if scenario == 'bad-schema' else 7},
             'isError':scenario == 'failed',
             '_meta':{'io.modelcontextprotocol/related-task':{
                 'taskId':'wrong-task' if scenario == 'wrong-result' else 'owned-task'}}}
    if scenario == 'failed':
        value['structuredContent'] = {'count':'error results need not match outputSchema'}
    return value

def exchange(frame, emit):
    global token, polls
    with lock:
        with (root / 'calls.jsonl').open('a') as log:
            log.write(json.dumps(frame) + '\n')
    method, ident = frame.get('method'), frame.get('id')
    if ident is None:
        return
    if method == 'initialize':
        caps = {'tools':{}}
        if scenario not in ('required-unavailable', 'optional-unavailable'):
            caps['tasks'] = {'cancel':{}, 'requests':{'tools':{'call':{}}}}
        value = {'protocolVersion':'2025-11-25', 'capabilities':caps,
                 'serverInfo':{'name':'task-peer','version':'1'}}
    elif method == 'tools/list':
        support = ('forbidden' if scenario == 'forbidden' else
                   'optional' if scenario.startswith('optional') else 'required')
        value = {'tools':[{'name':'work', 'description':'Task work',
                          'inputSchema':{'type':'object'},
                          'outputSchema':{'type':'object','required':['count'],
                                          'properties':{'count':{'type':'integer'}}},
                          'execution':{'taskSupport':support}}]}
    elif method == 'tools/call':
        if scenario in ('forbidden','optional-unavailable'):
            assert 'task' not in frame['params']
            value = result()
            del value['_meta']
        else:
            assert isinstance(frame['params']['task']['ttl'], int)
            token = frame['params'].get('_meta',{}).get('progressToken')
            status = ('completed' if scenario == 'instant' else
                      'input_required' if scenario == 'input' else 'working')
            value = {'task':task(status)}
            if scenario == 'bad-receipt':
                value['task']['createdAt'] = 'not-a-timestamp'
            if scenario == 'missing-id':
                del value['task']['taskId']
    elif method == 'tasks/get':
        assert frame['params'] == {'taskId':'owned-task'}
        polls += 1
        if token:
            emit({'jsonrpc':'2.0','method':'notifications/progress', 'params':{
                'progressToken':token,'progress':polls,'total':2,'message':'still running'}})
        status = 'working' if polls == 1 else ('failed' if scenario == 'failed' else 'completed')
        value = task(status, 'wrong-task' if scenario == 'wrong-status' else 'owned-task')
    elif method == 'tasks/result':
        assert frame['params'] == {'taskId':'owned-task'}
        if scenario == 'server-error':
            emit({'jsonrpc':'2.0','id':ident,'error':{'code':-32000,'message':'task failed'}})
            return
        value = result()
    elif method == 'tasks/cancel':
        assert frame['params'] == {'taskId':'owned-task'}
        value = task('cancelled')
    elif method == 'ping':
        value = {}
    else:
        raise RuntimeError('unexpected method: ' + str(method))
    emit({'jsonrpc':'2.0','id':ident,'result':value})

if mode == 'stdio':
    def emit(frame):
        print(json.dumps(frame), flush=True)
    for line in sys.stdin:
        exchange(json.loads(line), emit)
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
            self.empty(405)
        def do_DELETE(self):
            self.empty(200)
        def do_POST(self):
            size = int(self.headers.get('Content-Length','0'))
            assert 0 < size <= 65536
            frame = json.loads(self.rfile.read(size))
            replies = []
            exchange(frame, replies.append)
            if 'id' not in frame:
                self.empty(202)
                return
            if scenario == 'expired' and frame['method'] == 'tasks/get':
                self.empty(404)
                return
            is_sse = len(replies) > 1
            payload = (''.join('data: '+json.dumps(reply)+'\n\n' for reply in replies)
                       if is_sse else json.dumps(replies[0])).encode()
            self.send_response(200)
            self.send_header('Content-Type','text/event-stream' if is_sse else 'application/json')
            self.send_header('Content-Length',str(len(payload)))
            if frame['method'] == 'initialize':
                self.send_header('Mcp-Session-Id','task-session')
            self.end_headers()
            self.wfile.write(payload)
    server = ThreadingHTTPServer(('127.0.0.1',0), Handler)
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
    let script = root.join("task_peer.py");
    std::fs::write(&script, PEER).expect("write peer");
    let args = vec![
        "-u".to_string(),
        script.display().to_string(),
        root.display().to_string(),
        mode.to_string(),
        scenario.to_string(),
    ];
    let mut peer = None;
    let url = if mode == "http" {
        peer = Some(Peer(
            Command::new("python3")
                .args(&args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .spawn()
                .expect("start HTTP peer"),
        ));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !root.join("port").exists() {
            assert!(Instant::now() < deadline, "peer did not bind");
            assert!(peer.as_mut().unwrap().0.try_wait().unwrap().is_none());
            std::thread::sleep(Duration::from_millis(5));
        }
        let port = std::fs::read_to_string(root.join("port")).unwrap();
        Some(format!("http://127.0.0.1:{}/mcp", port.trim()))
    } else {
        None
    };
    let manager = Arc::new(McpManager::new(
        root,
        root,
        McpDiscovery {
            servers: vec![ConfiguredServer {
                name: "task-peer".to_string(),
                command: (mode == "stdio").then(|| "python3".to_string()),
                args: if mode == "stdio" { args } else { Vec::new() },
                env: Vec::new(),
                url,
                headers: Vec::new(),
                oauth: None,
                transport_hint: Some(mode.to_string()),
                enabled: true,
                cwd: None,
                timeout_ms: Some(5000),
                provenance: Provenance::ProjectPi,
                source_file: root.join("mcp.json"),
            }],
            warnings: Vec::new(),
        },
    ));
    let entry = manager.entry("task-peer").unwrap();
    let fingerprint = manager.trust_fingerprint_for(&entry);
    let identity = entry.config.execution_identity(root).unwrap();
    let mut trust = TrustStore::load(&manager.inner.trust_path).unwrap();
    if let Some(identity) = identity {
        trust
            .acknowledge_execution("task-peer", &fingerprint, "operator", identity)
            .unwrap();
    } else {
        trust
            .acknowledge("task-peer", &fingerprint, "operator")
            .unwrap();
    }
    (manager, peer)
}

fn runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("runtime")
}

fn frames(root: &Path) -> Vec<Value> {
    std::fs::read_to_string(root.join("calls.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn count(root: &Path, method: &str) -> usize {
    frames(root)
        .iter()
        .filter(|frame| frame["method"] == method)
        .count()
}

async fn wait_for_task_cancel(root: &Path) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while count(root, "tasks/cancel") == 0 && Instant::now() < deadline {
        let now = Cx::current()
            .and_then(|cx| cx.timer_driver())
            .map_or_else(asupersync::time::wall_now, |timer| timer.now());
        asupersync::time::sleep(now, Duration::from_millis(10)).await;
    }
    assert_eq!(count(root, "tasks/cancel"), 1, "owned cleanup was not delivered");
}

#[test]
fn mounted_task_tools_await_real_results_and_keep_progress_over_stdio_and_http() {
    for mode in ["stdio", "http"] {
        for scenario in ["required", "optional", "instant", "input", "failed"] {
            let temp = tempfile::tempdir().unwrap();
            let (manager, _peer) = fixture(temp.path(), mode, scenario);
            let runtime = runtime();
            let updates = Arc::new(Mutex::new(Vec::new()));
            runtime.block_on(async {
                let metadata = manager.refresh_tools("task-peer").await.unwrap();
                let tool = McpTool::new("task-peer", &metadata[0], Arc::clone(&manager));
                let captured = Arc::clone(&updates);
                let output = tool
                    .execute(
                        "request-1",
                        json!({}),
                        Some(Box::new(move |update| {
                            assert!(Cx::current().is_some(), "calling owner remains current");
                            McpManager::lock(&captured).push(update);
                        })),
                    )
                    .await
                    .unwrap();
                assert_eq!(output.is_error, scenario == "failed");
                assert!(format!("{:?}", output.content).contains("finished"));
                assert_eq!(count(temp.path(), "tools/call"), 1);
                assert_eq!(count(temp.path(), "tasks/result"), 1);
                assert_eq!(count(temp.path(), "tasks/cancel"), 0);
                assert_eq!(
                    count(temp.path(), "tasks/get"),
                    if matches!(scenario, "instant" | "input") { 0 } else { 2 }
                );
                let _ = manager.shutdown_all().await;
            });
            let updates = McpManager::lock(&updates);
            assert!(!updates.is_empty());
            assert!(updates.iter().any(|update| update.details.as_ref().is_some_and(|details| {
                details["mcpTask"]["status"].is_string()
            })));
            if !matches!(scenario, "instant" | "input") {
                assert!(updates.iter().any(|update| {
                    update.details.as_ref().is_some_and(|details| {
                        details["mcpProgress"]["progress"].is_number()
                    })
                }), "the original progress token stopped routing after task creation");
            }
            for update in updates.iter() {
                if let Some(message) = update.details.as_ref().and_then(|details| {
                    details["mcpTask"]["statusMessage"].as_str()
                }) {
                    assert!(!message.chars().any(char::is_control));
                    assert!(!message.contains('\u{202e}'));
                    assert!(message.contains("\\u{1b}"));
                }
            }
        }
    }
}

#[test]
fn task_negotiation_keeps_ordinary_calls_ordinary_and_rejects_unsupported_required_tools() {
    for scenario in ["forbidden", "optional-unavailable", "required-unavailable"] {
        let temp = tempfile::tempdir().unwrap();
        let (manager, _peer) = fixture(temp.path(), "stdio", scenario);
        runtime().block_on(async {
            let result = manager.call_tool("task-peer", "work", json!({})).await;
            if scenario == "required-unavailable" {
                assert!(result.unwrap_err().to_string().contains("MCP_TASK_UNSUPPORTED"));
                assert_eq!(count(temp.path(), "tools/call"), 0);
            } else {
                assert_eq!(result.unwrap()["structuredContent"]["count"], 7);
                assert_eq!(count(temp.path(), "tools/call"), 1);
            }
            assert_eq!(count(temp.path(), "tasks/get"), 0);
            assert_eq!(count(temp.path(), "tasks/result"), 0);
            let _ = manager.shutdown_all().await;
        });
    }
}

#[test]
fn task_contract_failures_never_replay_the_originating_tool_call() {
    for scenario in [
        "bad-receipt", "missing-id", "wrong-status", "wrong-result", "bad-schema",
        "server-error", "expired",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let (manager, _peer) = fixture(temp.path(), "http", scenario);
        runtime().block_on(async {
            let error = manager
                .call_tool("task-peer", "work", json!({}))
                .await
                .expect_err("invalid result must not appear as success");
            let expected = match scenario {
                "bad-schema" => "MCP_OUTPUT_INVALID",
                "server-error" => "MCP_SERVER_ERROR",
                _ => "MCP_DELIVERY_INDETERMINATE",
            };
            assert!(error.to_string().contains(expected), "{scenario}: {error}");
            if matches!(scenario, "bad-receipt" | "wrong-status" | "wrong-result") {
                wait_for_task_cancel(temp.path()).await;
            }
            assert_eq!(count(temp.path(), "tools/call"), 1);
            let calls = frames(temp.path());
            for frame in calls.iter().filter(|frame| frame["method"] == "tasks/cancel") {
                assert_eq!(frame["params"]["taskId"], "owned-task");
            }
            if matches!(scenario, "bad-schema" | "server-error" | "missing-id") {
                assert_eq!(count(temp.path(), "tasks/cancel"), 0);
            }
            assert_eq!(count(temp.path(), "notifications/cancelled"), 0);
            let _ = manager.shutdown_all().await;
        });
    }
}

#[test]
fn task_wait_keeps_the_initial_deadline_and_cancellation_owner() {
    for cancel in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let (manager, _peer) = fixture(temp.path(), "http", "timeout");
        let runtime = runtime();
        let owner = AgentCx::from_cx(runtime.request_cx_with_budget(Budget::new()));
        let abort_owner = owner.cx().clone();
        let callback = move |_: ToolUpdate| {
            if cancel {
                abort_owner.cancel_with(asupersync::types::CancelKind::User, Some("task test"));
            }
        };
        runtime.block_on(async {
            let result = owner.with_current(async {
                let entry = manager.entry("task-peer").unwrap();
                let (transport, contract) = manager
                    .prepare_tool_call(&owner, &entry, "work", Duration::from_secs(5))
                    .await
                    .unwrap();
                manager.execute_tool_call(
                    &entry,
                    &transport,
                    "work",
                    &json!({}),
                    Duration::from_millis(200),
                    contract.output_schema,
                    contract.task_support,
                    contract.transport_generation,
                    Some(&callback),
                ).await
            }).await;
            let message = result.unwrap_err().to_string();
            assert!(message.contains(if cancel { "MCP_CANCELLED" } else { "MCP_TIMEOUT" }), "{message}");
            wait_for_task_cancel(temp.path()).await;
            assert_eq!(count(temp.path(), "tools/call"), 1);
            assert_eq!(count(temp.path(), "tasks/get"), 0);
            assert_eq!(count(temp.path(), "tasks/result"), 0);
            assert_eq!(count(temp.path(), "tasks/cancel"), 1);
            assert_eq!(count(temp.path(), "notifications/cancelled"), 0);
            let _ = manager.shutdown_all().await;
        });
    }
}
