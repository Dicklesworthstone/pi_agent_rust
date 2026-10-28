//! Independent peers exercise the public manager and mounted context actions.
//! Notifications arrive on stdio, native POST/GET SSE, and legacy GET SSE.

use super::*;
use crate::agent_cx::AgentCx;
use crate::mcp::{ConfiguredServer, McpContextTool, McpDiscovery, McpManager, Provenance};
use crate::tools::Tool as _;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const URI: &str = "repo://src/%2F.rs?rev=a+b#exact";
const PEER: &str = r"
import json, pathlib, queue, sys, threading, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

root = pathlib.Path(sys.argv[1])
mode, scenario = sys.argv[2:4]
log_lock = threading.Lock()
write_lock = threading.Lock()
events = queue.Queue(maxsize=64)
uri = 'repo://src/%2F.rs?rev=a+b#exact'

def record(frame):
    with log_lock:
        with (root / 'calls.jsonl').open('a') as log:
            log.write(json.dumps(frame) + '\n')

def updated(value):
    return {'jsonrpc':'2.0','method':'notifications/resources/updated',
            'params':{'uri':value,'_meta':{'private':'do not expose'}}}

def changes(emit, value):
    emit(updated(value))
    emit(updated(value))
    emit(updated('unrequested://secret'))
    emit({'jsonrpc':'2.0','method':'notifications/resources/list_changed'})
    emit({'jsonrpc':'2.0','method':'notifications/prompts/list_changed'})

def wait_release(name):
    (root / (name + '-pending')).write_text('ready')
    deadline = time.monotonic() + 8
    while not (root / (name + '-release')).exists():
        assert time.monotonic() < deadline, 'test did not release ' + name
        time.sleep(0.005)

def exchange(frame, emit):
    record(frame)
    method, ident = frame.get('method'), frame.get('id')
    if ident is None:
        return
    def reject():
        emit({'jsonrpc':'2.0','id':ident,'error':{'code':-32602,'message':'watch rejected'}})
    if method == 'initialize':
        caps = {'resources':{'subscribe':scenario != 'no-subscribe',
                              'listChanged':scenario != 'no-flags'},
                'prompts':{'listChanged':scenario != 'no-flags'}}
        if scenario == 'bad-capability':
            caps['resources']['subscribe'] = 'yes'
        value = {'protocolVersion':frame['params']['protocolVersion'],'capabilities':caps,
                 'serverInfo':{'name':'updates-peer','version':'1'}}
    elif method == 'tools/list':
        emit({'jsonrpc':'2.0','id':ident,'error':{'code':-32601,'message':'context-only server'}})
        return
    elif method == 'resources/subscribe':
        selected = frame['params']['uri']
        if selected == 'denied://subscribe':
            emit(updated(selected))
            reject()
            return
        if selected == 'pending://subscription':
            emit(updated(selected))
            wait_release('subscribe')
        else:
            changes(emit, selected)
        value = [] if selected == 'bad://ack' else {}
    elif method == 'resources/unsubscribe':
        selected = frame['params']['uri']
        emit(updated(selected))
        if scenario == 'errors' and not (root / 'allow-unsubscribe').exists():
            reject()
            return
        value = {}
    elif method == 'resources/read':
        selected = frame['params']['uri']
        if selected == 'control://emit':
            changes(emit, uri)
        elif selected == 'control://hold':
            wait_release('read')
        value = {'contents':[{'uri':selected,'text':'fresh resource contents'}]}
    elif method == 'resources/list':
        value = {'resources':[{'name':'watched','uri':uri}]}
    elif method == 'prompts/list':
        value = {'prompts':[{'name':'changed-prompt'}]}
    elif method == 'ping':
        value = {}
    else:
        raise RuntimeError('unexpected method: ' + str(method))
    emit({'jsonrpc':'2.0','id':ident,'result':value})

def safe_exchange(frame, emit):
    try:
        exchange(frame, emit)
    except (BrokenPipeError, ConnectionResetError):
        pass
    except Exception as error:
        (root / 'peer-error').write_text(repr(error))
        raise

if mode == 'stdio':
    def emit(frame):
        with write_lock:
            print(json.dumps(frame), flush=True)
    for line in sys.stdin:
        frame = json.loads(line)
        if frame.get('method') in ('resources/subscribe','resources/unsubscribe','resources/read'):
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
            deadline = time.monotonic() + 20
            try:
                while time.monotonic() < deadline:
                    try:
                        value = events.get(timeout=0.05)
                    except queue.Empty:
                        continue
                    self.wfile.write(('data: '+json.dumps(value)+'\n\n').encode())
                    self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                pass
            self.close_connection = True
        def do_DELETE(self):
            self.empty(200)
        def do_POST(self):
            count = int(self.headers.get('Content-Length','0'))
            assert 0 < count <= 65536
            frame = json.loads(self.rfile.read(count))
            if 'id' not in frame:
                record(frame)
                self.empty(202)
                return
            if frame.get('params',{}).get('uri') == 'control://expire':
                record(frame)
                self.empty(404)
                return
            if mode == 'sse':
                self.empty(202)
                threading.Thread(target=safe_exchange,args=(frame,lambda value: events.put(value,timeout=1)),daemon=True).start()
                return
            if mode == 'http' and frame['method'] in ('resources/subscribe','resources/unsubscribe','resources/read'):
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
                self.send_header('Mcp-Session-Id','updates-session')
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
    let script = root.join("updates_peer.py");
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
                name: "updates-peer".to_string(),
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
    (manager, peer)
}

fn runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap()
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

async fn pause() {
    asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(5)).await;
}

async fn marker(root: &Path, name: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    // ubs:ignore[rust.security.path-traversal] Callers pass fixed fixture marker names beneath their own temporary peer directory.
    while !root.join(name).exists() {
        assert!(Instant::now() < deadline, "missing peer marker {name}");
        assert!(!root.join("peer-error").exists());
        pause().await;
    }
}

async fn action(tool: &McpContextTool, input: Value) -> Value {
    let output = tool.execute("watch", input, None).await.unwrap();
    assert!(!output.is_error);
    let text = output
        .content
        .iter()
        .filter_map(|block| match block {
            crate::model::ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert!(!text.contains("do not expose"));
    output.details.unwrap()
}

async fn changes_seen(manager: &McpManager, expected_uri: Option<&str>) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut resources = false;
    let mut prompts = false;
    let mut uris = BTreeSet::new();
    loop {
        let mut result = manager.poll_updates("updates-peer").await.unwrap();
        resources |= result["resources_list_changed"] == true;
        prompts |= result["prompts_list_changed"] == true;
        for uri in result["updated_resources"].as_array().unwrap() {
            uris.insert(uri.as_str().unwrap().to_string());
        }
        if resources && prompts && expected_uri.is_none_or(|uri| uris.contains(uri)) {
            result["resources_list_changed"] = json!(resources);
            result["prompts_list_changed"] = json!(prompts);
            result["updated_resources"] = json!(uris);
            return result;
        }
        assert!(
            Instant::now() < deadline,
            "native update notification did not arrive"
        );
        pause().await;
    }
}

#[test]
fn mounted_context_watches_and_drains_changes_on_all_native_receive_paths() {
    for mode in ["stdio", "http", "http-get", "sse"] {
        let temp = tempfile::tempdir().unwrap();
        let (manager, _peer) = fixture(temp.path(), mode, "normal");
        runtime().block_on(async {
            assert!(manager.trust("updates-peer").await.unwrap().is_empty());
            let tool = McpContextTool::new("updates-peer", Arc::clone(&manager));
            let initial = action(&tool, json!({"action":"poll_updates"})).await;
            assert_eq!(initial["connected"], true);
            assert_eq!(initial["capabilities"]["subscribe"], true);
            assert_eq!(
                action(&tool, json!({"action":"subscribe_resource","uri":URI})).await,
                json!({"uri":URI,"subscribed":true})
            );
            action(&tool, json!({"action":"subscribe_resource","uri":URI})).await;
            let changes = changes_seen(&manager, Some(URI)).await;
            assert_eq!(changes["subscriptions"], json!([URI]));
            assert_eq!(changes["updated_resources"], json!([URI]));
            assert_eq!(
                count(temp.path(), "resources/read"),
                0,
                "notifications must not fetch content"
            );
            let drained = action(&tool, json!({"action":"poll_updates"})).await;
            assert_eq!(drained["updated_resources"], json!([]));
            assert_eq!(drained["resources_list_changed"], false);
            assert_eq!(drained["prompts_list_changed"], false);
            assert_eq!(
                manager.list_resources("updates-peer", None).await.unwrap()["resources"][0]["uri"],
                URI
            );
            assert_eq!(
                manager.list_prompts("updates-peer", None).await.unwrap()["prompts"][0]["name"],
                "changed-prompt"
            );
            assert_eq!(
                manager.read_resource("updates-peer", URI).await.unwrap()["contents"][0]["text"],
                "fresh resource contents"
            );
            assert_eq!(
                action(&tool, json!({"action":"unsubscribe_resource","uri":URI})).await,
                json!({"uri":URI,"subscribed":false})
            );
            action(&tool, json!({"action":"unsubscribe_resource","uri":URI})).await;
            manager
                .read_resource("updates-peer", "control://emit")
                .await
                .unwrap();
            let ignored = changes_seen(&manager, None).await;
            assert_eq!(ignored["subscriptions"], json!([]));
            assert_eq!(ignored["updated_resources"], json!([]));
            manager.shutdown_all().await;
        });
        let requests = frames(temp.path());
        assert_eq!(
            count(temp.path(), "resources/subscribe"),
            1,
            "{mode}: idempotent subscribe"
        );
        assert_eq!(
            count(temp.path(), "resources/unsubscribe"),
            1,
            "{mode}: idempotent unsubscribe"
        );
        for frame in requests.iter().filter(|frame| {
            matches!(
                frame["method"].as_str(),
                Some("resources/subscribe" | "resources/unsubscribe")
            )
        }) {
            assert_eq!(frame["params"], json!({"uri":URI}));
        }
        assert!(!temp.path().join("peer-error").exists(), "{mode}");
    }
}

#[test]
fn native_capability_refusal_is_pre_dispatch_and_list_flags_are_independent() {
    for mode in ["stdio", "http", "sse"] {
        for scenario in ["no-subscribe", "no-flags", "bad-capability"] {
            let temp = tempfile::tempdir().unwrap();
            let (manager, _peer) = fixture(temp.path(), mode, scenario);
            runtime().block_on(async {
                let trusted = manager.trust("updates-peer").await;
                if scenario == "bad-capability" {
                    assert!(trusted.unwrap_err().to_string().contains("MCP_PROTOCOL"));
                } else {
                    trusted.unwrap();
                    if scenario == "no-subscribe" {
                        assert!(
                            manager
                                .subscribe_resource("updates-peer", URI)
                                .await
                                .unwrap_err()
                                .to_string()
                                .contains("MCP_SUBSCRIPTION_UNSUPPORTED")
                        );
                        assert_eq!(count(temp.path(), "resources/subscribe"), 0);
                        manager
                            .read_resource("updates-peer", "control://emit")
                            .await
                            .unwrap();
                        let changes = changes_seen(&manager, None).await;
                        assert_eq!(changes["updated_resources"], json!([]));
                    } else {
                        manager
                            .subscribe_resource("updates-peer", URI)
                            .await
                            .unwrap();
                        let updates = manager.poll_updates("updates-peer").await.unwrap();
                        assert_eq!(updates["updated_resources"], json!([URI]));
                        assert_eq!(updates["resources_list_changed"], false);
                        assert_eq!(updates["prompts_list_changed"], false);
                    }
                }
                manager.shutdown_all().await;
            });
            assert!(!temp.path().join("peer-error").exists());
        }
    }
}

#[test]
fn rejected_mutations_roll_back_without_losing_accepted_native_watches() {
    for mode in ["stdio", "http", "sse"] {
        let temp = tempfile::tempdir().unwrap();
        let (manager, _peer) = fixture(temp.path(), mode, "errors");
        runtime().block_on(async {
            manager.trust("updates-peer").await.unwrap();
            manager
                .subscribe_resource("updates-peer", URI)
                .await
                .unwrap();
            changes_seen(&manager, Some(URI)).await;
            assert!(
                manager
                    .subscribe_resource("updates-peer", "denied://subscribe")
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("MCP_SERVER_ERROR")
            );
            assert!(
                manager
                    .unsubscribe_resource("updates-peer", URI)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("MCP_SERVER_ERROR")
            );
            let restored = manager.poll_updates("updates-peer").await.unwrap();
            assert_eq!(restored["connected"], true);
            assert_eq!(restored["subscriptions"], json!([URI]));
            assert_eq!(restored["updated_resources"], json!([URI]));
            assert_eq!(restored["expired_subscriptions"], json!([]));
            std::fs::write(temp.path().join("allow-unsubscribe"), "yes").unwrap();
            manager
                .unsubscribe_resource("updates-peer", URI)
                .await
                .unwrap();
            assert_eq!(
                manager.poll_updates("updates-peer").await.unwrap()["subscriptions"],
                json!([])
            );
            assert_eq!(
                count(temp.path(), "initialize"),
                1,
                "server errors do not reconnect"
            );
            manager.shutdown_all().await;
        });
        assert!(!temp.path().join("peer-error").exists());
    }
}

#[test]
fn expired_http_session_reports_watches_and_explicit_reconnect_does_not_resubscribe() {
    let temp = tempfile::tempdir().unwrap();
    let (manager, _peer) = fixture(temp.path(), "http", "normal");
    runtime().block_on(async {
        manager.trust("updates-peer").await.unwrap();
        manager
            .subscribe_resource("updates-peer", URI)
            .await
            .unwrap();
        changes_seen(&manager, Some(URI)).await;
        assert!(
            manager
                .read_resource("updates-peer", "control://expire")
                .await
                .unwrap_err()
                .to_string()
                .contains("MCP_SESSION_EXPIRED")
        );
        let expired = manager.poll_updates("updates-peer").await.unwrap();
        assert_eq!(expired["connected"], false);
        assert_eq!(expired["expired_subscriptions"], json!([URI]));
        assert_eq!(
            count(temp.path(), "initialize"),
            1,
            "poll does not reconnect"
        );
        assert_eq!(count(temp.path(), "resources/subscribe"), 1);
        manager.read_resource("updates-peer", URI).await.unwrap();
        let replaced = manager.poll_updates("updates-peer").await.unwrap();
        assert_eq!(replaced["connected"], true);
        assert_eq!(replaced["subscriptions"], json!([]));
        assert_eq!(replaced["expired_subscriptions"], json!([]));
        assert_eq!(count(temp.path(), "initialize"), 2);
        assert_eq!(
            count(temp.path(), "resources/subscribe"),
            1,
            "no silent resubscribe"
        );
        manager.shutdown_all().await;
    });
}

#[test]
fn cancelling_dispatched_subscription_retires_session_and_never_replays() {
    for mode in ["stdio", "http", "sse"] {
        let temp = tempfile::tempdir().unwrap();
        let (manager, _peer) = fixture(temp.path(), mode, "normal");
        let runtime = runtime();
        let owner = AgentCx::from_cx(runtime.request_cx_with_budget(asupersync::Budget::new()));
        runtime.block_on(async {
            manager.trust("updates-peer").await.unwrap();
            manager
                .subscribe_resource("updates-peer", URI)
                .await
                .unwrap();
            changes_seen(&manager, Some(URI)).await;
            let cancel = async {
                marker(temp.path(), "subscribe-pending").await;
                owner.cancel_with(
                    asupersync::types::CancelKind::User,
                    Some("cancel subscription"),
                );
            };
            let (result, ()) = futures::join!(
                owner.with_current(
                    manager.subscribe_resource("updates-peer", "pending://subscription")
                ),
                cancel
            );
            assert!(result.is_err(), "{mode}");
            let retired = manager.poll_updates("updates-peer").await.unwrap();
            assert_eq!(retired["connected"], false);
            assert_eq!(retired["expired_subscriptions"], json!([URI]));
            assert_eq!(retired["subscriptions"], json!([]));
            assert_eq!(count(temp.path(), "initialize"), 1);
            assert_eq!(
                count(temp.path(), "resources/subscribe"),
                2,
                "each explicit mutation sent once"
            );
            std::fs::write(temp.path().join("subscribe-release"), "done").unwrap();
            manager.shutdown_all().await;
        });
    }
}

#[test]
fn cancelling_queued_http_unsubscribe_preserves_active_request_and_watch() {
    let temp = tempfile::tempdir().unwrap();
    let (manager, _peer) = fixture(temp.path(), "http", "normal");
    runtime().block_on(async {
        manager.trust("updates-peer").await.unwrap();
        manager
            .subscribe_resource("updates-peer", URI)
            .await
            .unwrap();
        changes_seen(&manager, Some(URI)).await;
        let cancel_queued = async {
            marker(temp.path(), "read-pending").await;
            let mut queued = Box::pin(manager.unsubscribe_resource("updates-peer", URI));
            assert!(futures::poll!(queued.as_mut()).is_pending());
            drop(queued);
            let retained = manager.poll_updates("updates-peer").await.unwrap();
            assert_eq!(retained["connected"], true);
            assert_eq!(retained["subscriptions"], json!([URI]));
            assert_eq!(retained["expired_subscriptions"], json!([]));
            std::fs::write(temp.path().join("read-release"), "done").unwrap();
        };
        let (read, ()) = futures::join!(
            manager.read_resource("updates-peer", "control://hold"),
            cancel_queued
        );
        read.unwrap();
        assert_eq!(
            count(temp.path(), "resources/unsubscribe"),
            0,
            "queued mutation was never sent"
        );
        manager
            .unsubscribe_resource("updates-peer", URI)
            .await
            .unwrap();
        assert_eq!(
            count(temp.path(), "resources/unsubscribe"),
            1,
            "rollback released the URI reservation"
        );
        manager.shutdown_all().await;
    });
}

#[test]
fn malformed_acknowledgement_and_trust_revocation_clear_live_watch_authority() {
    for mode in ["stdio", "http", "sse"] {
        let temp = tempfile::tempdir().unwrap();
        let (manager, _peer) = fixture(temp.path(), mode, "normal");
        runtime().block_on(async {
            manager.trust("updates-peer").await.unwrap();
            manager
                .subscribe_resource("updates-peer", URI)
                .await
                .unwrap();
            changes_seen(&manager, Some(URI)).await;
            assert!(
                manager
                    .subscribe_resource("updates-peer", "bad://ack")
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("MCP_PROTOCOL")
            );
            let expired = manager.poll_updates("updates-peer").await.unwrap();
            assert_eq!(expired["expired_subscriptions"], json!([URI]));
            assert_eq!(expired["subscriptions"], json!([]));
            manager.deny("updates-peer").await.unwrap();
            let before = frames(temp.path()).len();
            assert!(manager.poll_updates("updates-peer").await.is_err());
            assert!(
                manager
                    .subscribe_resource("updates-peer", URI)
                    .await
                    .is_err()
            );
            assert_eq!(
                frames(temp.path()).len(),
                before,
                "revoked methods perform no remote work"
            );
            manager.shutdown_all().await;
            assert!(manager.poll_updates("updates-peer").await.is_err());
        });
    }
}
