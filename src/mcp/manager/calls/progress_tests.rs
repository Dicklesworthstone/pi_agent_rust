//! Real mounted-tool -> manager -> native transport workflows against an
//! independently implemented, bounded protocol peer. Python is test-only.

use super::*;
use crate::mcp::{ConfiguredServer, McpDiscovery, McpTool, McpToolMeta, Provenance, TrustStore};
use crate::tools::Tool;
use asupersync::{Budget, Cx};
use serde_json::json;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

const PEER: &str = r#"
import json, pathlib, queue, sys, threading, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

root = pathlib.Path(sys.argv[1])
mode = sys.argv[2]
get_messages = queue.Queue(maxsize=16)
get_ready = threading.Event()
get_stopped = threading.Event()


def exchange(frame, emit):
    method = frame.get('method')
    ident = frame.get('id')
    if ident is None:
        return
    if method == 'initialize':
        result = {'protocolVersion':'2025-06-18', 'capabilities':{'tools':{}},
                  'serverInfo':{'name':'progress-peer', 'version':'1'}}
    elif method == 'tools/list':
        result = {'tools':[{'name':'work', 'description':'Work', 'inputSchema':{'type':'object'}}]}
    elif method == 'ping':
        result = {}
    elif method == 'tools/call':
        (root / 'request.json').write_text(json.dumps(frame))
        with (root / 'calls.jsonl').open('a') as log:
            log.write(json.dumps(frame) + '\n')
        token = frame['params'].get('_meta', {}).get('progressToken')
        def report(value, selected=token):
            emit({'jsonrpc':'2.0', 'method':'notifications/progress', 'params':{
                'progressToken':selected, 'progress':value, 'total':2,
                'message':'phase\x1b[2J\n\u202e'}})
        if mode == 'timeout':
            for count in range(500):
                report(count)
                time.sleep(0.01)
            raise RuntimeError('request did not respect its deadline')
        if token is not None:
            assert isinstance(token, str)
            for value in (1, 2):
                report(value, 'not-this-request')
                report(value)
                report(value)  # duplicate, not another callback
                report(value - 1)  # regression
                until = time.monotonic() + 5
                ack = root / ('ack-' + str(ident) + '-' + str(value))
                while not ack.exists():
                    if time.monotonic() >= until:
                        raise RuntimeError('progress callback never acknowledged live status')
                    time.sleep(0.005)
        result = {'content':[{'type':'text', 'text':'done'}], 'isError':False}
    else:
        raise RuntimeError('unexpected method: ' + repr(method))
    emit({'jsonrpc':'2.0', 'id':ident, 'result':result})

if mode not in ('http', 'http-get'):
    def emit(frame):
        print(json.dumps(frame), flush=True)
    for line in sys.stdin:
        exchange(json.loads(line), emit)
else:
    class Handler(BaseHTTPRequestHandler):
        protocol_version = 'HTTP/1.1'
        def log_message(self, *args):
            pass
        def do_GET(self):
            if mode != 'http-get':
                self.send_response(405)
                self.send_header('Content-Length', '0')
                self.end_headers()
                return
            assert self.headers.get('Mcp-Session-Id') == 'progress-session'
            self.send_response(200)
            self.send_header('Content-Type', 'text/event-stream')
            self.send_header('Connection', 'close')
            self.end_headers()
            self.wfile.flush()
            get_ready.set()
            until = time.monotonic() + 15
            while not get_stopped.is_set() and time.monotonic() < until:
                try:
                    value = get_messages.get(timeout=0.1)
                except queue.Empty:
                    continue
                # No JSON-RPC response may appear on this unsolicited stream.
                assert 'method' in value and 'id' not in value
                self.wfile.write(('data: ' + json.dumps(value) + '\n\n').encode())
                self.wfile.flush()
            self.close_connection = True
        def do_DELETE(self):
            get_stopped.set()
            self.send_response(200)
            self.send_header('Content-Length', '0')
            self.end_headers()
        def do_POST(self):
            count = int(self.headers.get('Content-Length', '0'))
            assert 0 < count <= 65536
            frame = json.loads(self.rfile.read(count))
            if 'id' not in frame:
                self.send_response(202)
                self.send_header('Content-Length', '0')
                self.end_headers()
                return
            if frame['method'] == 'tools/call' and mode == 'http':
                self.send_response(200)
                self.send_header('Content-Type', 'text/event-stream')
                self.send_header('Connection', 'close')
                self.end_headers()
                def emit(value):
                    self.wfile.write(('data: ' + json.dumps(value) + '\n\n').encode())
                    self.wfile.flush()
                exchange(frame, emit)
                self.close_connection = True
            else:
                replies = []
                def emit(value):
                    if value.get('method') == 'notifications/progress':
                        assert mode == 'http-get' and get_ready.wait(5)
                        # Every notification travels on one stream only.
                        get_messages.put(value, timeout=5)
                    else:
                        replies.append(value)
                exchange(frame, emit)
                assert len(replies) == 1
                payload = json.dumps(replies[0]).encode()
                self.send_response(200)
                self.send_header('Content-Type', 'application/json')
                self.send_header('Content-Length', str(len(payload)))
                if frame['method'] == 'initialize':
                    self.send_header('Mcp-Session-Id', 'progress-session')
                self.end_headers()
                self.wfile.write(payload)
    server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    (root / 'port.tmp').write_text(str(server.server_port))
    (root / 'port.tmp').replace(root / 'port')
    server.serve_forever()
"#;

struct PeerChild(Child);

impl Drop for PeerChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn configured(root: &Path, mode: &str) -> (ConfiguredServer, Option<PeerChild>) {
    let script = root.join("progress_peer.py");
    std::fs::write(&script, PEER).unwrap();
    let args = vec![
        "-u".to_string(),
        script.display().to_string(),
        root.display().to_string(),
        mode.to_string(),
    ];
    let mut child = None;
    let is_http = matches!(mode, "http" | "http-get");
    let url = if is_http {
        child = Some(PeerChild(
            Command::new("python3")
                .args(&args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .spawn()
                .expect("Python protocol peer"),
        ));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !root.join("port").exists() {
            assert!(Instant::now() < deadline, "HTTP peer did not start");
            assert!(
                child.as_mut().unwrap().0.try_wait().unwrap().is_none(),
                "HTTP peer exited"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let port = std::fs::read_to_string(root.join("port")).unwrap();
        Some(format!("http://127.0.0.1:{}/mcp", port.trim()))
    } else {
        None
    };
    (
        ConfiguredServer {
            name: "progress".to_string(),
            command: (!is_http).then(|| "python3".to_string()),
            args: if is_http { Vec::new() } else { args },
            env: Vec::new(),
            url,
            headers: Vec::new(),
            oauth: None,
            transport_hint: Some(if is_http { "http" } else { "stdio" }.to_string()),
            enabled: true,
            cwd: None,
            timeout_ms: None,
            provenance: Provenance::ProjectPi,
            source_file: root.join("mcp.json"),
        },
        child,
    )
}

fn manager(root: &Path, config: ConfiguredServer) -> (Arc<McpManager>, Arc<ServerEntry>) {
    let manager = Arc::new(McpManager::new(
        root,
        root,
        McpDiscovery {
            servers: vec![config],
            warnings: Vec::new(),
        },
    ));
    let entry = manager.entry("progress").unwrap();
    // Mirror the public trust operation instead of bypassing its executable
    // binding. A fingerprint-only approval is not sufficient for stdio.
    let identity = entry
        .config
        .execution_identity(&entry.effective_cwd(&manager.inner.cwd))
        .expect("resolve the fixture's executable identity");
    let fingerprint = manager.trust_fingerprint_for(&entry);
    let mut store = TrustStore::load(&manager.inner.trust_path).unwrap();
    if let Some(execution) = identity {
        store
            .acknowledge_execution("progress", &fingerprint, "operator", execution)
            .unwrap();
    } else {
        store
            .acknowledge("progress", &fingerprint, "operator")
            .unwrap();
    }
    (manager, entry)
}

fn runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap()
}

#[cfg(unix)]
#[test]
fn mounted_tools_report_live_progress_over_stdio_and_http_without_changing_results() {
    for mode in ["stdio", "http", "http-get"] {
        let temp = tempfile::tempdir().unwrap();
        let (config, _peer) = configured(temp.path(), mode);
        let (manager, entry) = manager(temp.path(), config);
        let meta = McpToolMeta {
            name: "work".to_string(),
            description: "Work".to_string(),
            input_schema: json!({"type":"object"}),
            output_schema: None,
        };
        let tool = McpTool::new("progress", &meta, Arc::clone(&manager));
        let runtime = runtime();
        let owner = AgentCx::from_cx(runtime.request_cx_with_budget(Budget::new()));
        let budget = owner.cx().budget();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let updates = Arc::clone(&seen);
        let root = temp.path().to_path_buf();
        let callback = move |update: ToolUpdate| {
            assert_eq!(Cx::current().expect("callback owner").budget(), budget);
            let details = update.details.unwrap()["mcpProgress"].clone();
            let count = details["progress"].as_u64().unwrap();
            assert!([1, 2].contains(&count));
            assert!(
                !details["message"]
                    .as_str()
                    .unwrap()
                    .chars()
                    .any(char::is_control)
            );
            updates.lock().unwrap().push(details);
            let request: Value =
                serde_json::from_str(&std::fs::read_to_string(root.join("request.json")).unwrap())
                    .unwrap();
            let id = request["id"].as_u64().unwrap();
            std::fs::write(root.join(format!("ack-{id}-{count}")), "observed").unwrap();
        };
        runtime.block_on(owner.with_current(async {
            let result = tool
                .execute(
                    "call-1",
                    json!({"_meta":{"progressToken":"model-literal"}}),
                    Some(Box::new(callback)),
                )
                .await;
            let transport = McpManager::lock(&entry.transport).clone();
            if let Some(transport) = transport {
                transport.close().await;
            }
            let result = result.expect("native tool completes after progress acknowledgement");
            assert!(!result.is_error);
            assert_eq!(result.content.len(), 1);
            let crate::model::ContentBlock::Text(text) = &result.content[0] else {
                panic!("expected native text output");
            };
            assert_eq!(text.text, "done");
        }));
        let seen = seen.lock().unwrap();
        assert_eq!(
            seen.len(),
            2,
            "duplicate/regressing/foreign progress escaped: {mode}"
        );
        assert_eq!(seen[0]["progress"], 1);
        assert_eq!(seen[1]["progress"], 2);
        let request: Value = serde_json::from_str(
            &std::fs::read_to_string(temp.path().join("request.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            request["params"]["arguments"]["_meta"]["progressToken"],
            "model-literal"
        );
        assert!(
            request["params"]["_meta"]["progressToken"]
                .as_str()
                .unwrap()
                .starts_with("pi-mcp-")
        );
    }
}

#[cfg(unix)]
#[test]
fn calls_without_an_observer_do_not_request_progress() {
    let temp = tempfile::tempdir().unwrap();
    let (config, _peer) = configured(temp.path(), "stdio");
    let (manager, entry) = manager(temp.path(), config);
    let runtime = runtime();
    let owner = AgentCx::from_cx(runtime.request_cx_with_budget(Budget::new()));
    runtime.block_on(owner.with_current(async {
        let result = manager.call_tool("progress", "work", json!({})).await;
        let transport = McpManager::lock(&entry.transport).clone();
        if let Some(transport) = transport {
            transport.close().await;
        }
        assert_eq!(result.unwrap()["content"][0]["text"], "done");
    }));
    let request: Value =
        serde_json::from_str(&std::fs::read_to_string(temp.path().join("request.json")).unwrap())
            .unwrap();
    assert!(request["params"].get("_meta").is_none());
}

#[cfg(unix)]
#[test]
fn continuous_progress_does_not_restart_the_manager_deadline() {
    let temp = tempfile::tempdir().unwrap();
    let (config, _peer) = configured(temp.path(), "timeout");
    let (manager, entry) = manager(temp.path(), config);
    let runtime = runtime();
    let owner = AgentCx::from_cx(runtime.request_cx_with_budget(Budget::new()));
    let observed = AtomicUsize::new(0);
    let callback = |_update| {
        observed.fetch_add(1, Ordering::SeqCst);
    };
    runtime.block_on(owner.with_current(async {
        let (transport, contract) = manager
            .prepare_tool_call(&owner, &entry, "work", DEFAULT_MCP_TIMEOUT)
            .await
            .unwrap();
        let began = Instant::now();
        let result = manager
            .execute_tool_call(
                &entry,
                &transport,
                "work",
                &json!({}),
                Duration::from_millis(250),
                contract.output_schema,
                Some(&callback),
            )
            .await;
        transport.close().await;
        assert!(result.unwrap_err().to_string().contains("MCP_TIMEOUT"));
        assert!(began.elapsed() < Duration::from_secs(5));
        assert!(
            observed.load(Ordering::SeqCst) > 0,
            "peer progress never reached the caller"
        );
    }));
}

/// The mounted callback can stop its owning native turn. No second callback,
/// successful result, reconnect or replay may escape the cancelled owner.
#[cfg(unix)]
#[test]
fn a_live_progress_callback_can_cancel_its_native_call_without_replay() {
    let temp = tempfile::tempdir().unwrap();
    let (config, _peer) = configured(temp.path(), "timeout");
    let (manager, entry) = manager(temp.path(), config);
    let runtime = runtime();
    let owner = AgentCx::from_cx(runtime.request_cx_with_budget(Budget::new()));
    let callback_owner = owner.clone();
    let observed = Arc::new(AtomicUsize::new(0));
    let observed_by_callback = Arc::clone(&observed);
    let callback = move |_update: ToolUpdate| {
        observed_by_callback.fetch_add(1, Ordering::SeqCst);
        callback_owner.cancel_with(
            asupersync::types::CancelKind::User,
            Some("stop after observing real server progress"),
        );
    };
    let meta = McpToolMeta {
        name: "work".to_string(),
        description: "Work".to_string(),
        input_schema: json!({"type":"object"}),
        output_schema: None,
    };
    let tool = McpTool::new("progress", &meta, Arc::clone(&manager));
    runtime.block_on(owner.with_current(async {
        // Keep a reference to the actual connection so cleanup is observable
        // even after cancellation removes the manager's published generation.
        let (transport, _) = manager
            .prepare_tool_call(&owner, &entry, "work", DEFAULT_MCP_TIMEOUT)
            .await
            .unwrap();
        let result = tool
            .execute("cancelled-call", json!({}), Some(Box::new(callback)))
            .await;
        let alive = transport.is_alive();
        transport.abort();
        assert!(result.unwrap_err().to_string().contains("MCP_CANCELLED"));
        assert!(
            !alive,
            "cancellation must retire this dispatched stdio request"
        );
        assert!(McpManager::lock(&entry.transport).is_none());
        assert!(McpManager::lock(&entry.tools_cache).is_none());
        assert_eq!(McpManager::lock(&entry.restarts).count, 1);
    }));
    assert_eq!(observed.load(Ordering::SeqCst), 1);
    let calls = std::fs::read_to_string(temp.path().join("calls.jsonl")).unwrap();
    assert_eq!(
        calls.lines().count(),
        1,
        "a cancelled call must not be replayed"
    );
}

#[cfg(unix)]
#[test]
fn observing_progress_cannot_admit_an_untrusted_stdio_server() {
    let temp = tempfile::tempdir().unwrap();
    let (config, _peer) = configured(temp.path(), "stdio");
    let manager = Arc::new(McpManager::new(
        temp.path(),
        temp.path(),
        McpDiscovery {
            servers: vec![config],
            warnings: Vec::new(),
        },
    ));
    let entry = manager.entry("progress").unwrap();
    let runtime = runtime();
    let owner = AgentCx::from_cx(runtime.request_cx_with_budget(Budget::new()));
    let result = runtime.block_on(owner.with_current(manager.call_tool_with_updates(
        "progress",
        "work",
        json!({}),
        Some(Box::new(|_| panic!("untrusted progress"))),
    )));
    assert!(result.unwrap_err().to_string().contains("MCP_TRUST"));
    assert!(McpManager::lock(&entry.transport).is_none());
    assert!(!temp.path().join("request.json").exists());
    assert!(!temp.path().join("calls.jsonl").exists());
}
