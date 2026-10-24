//! Public debug-tool loopback attachment against a deterministic DAP peer.
//! This exercises the real Rust registry/transport, not a language debugger.

mod common;

use common::TestHarness;
use pi::debug::DebugTool;
use pi::jobs::JobSessionScope;
use pi::lsp::jsonrpc::encode_frame;
use pi::tools::{Tool, ToolRegistry};
use serde_json::{Value, json};
use std::io::{BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn run<F: std::future::Future>(future: F) -> F::Output {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .unwrap()
        .block_on(future)
}

fn finish(harness: &TestHarness) {
    harness.log().info(
        "verify",
        "loopback protocol assertions passed; not a live debugger test",
    );
    let path = harness.temp_path("debug-attach.jsonl");
    harness.write_jsonl_logs(&path).unwrap();
    let text = std::fs::read_to_string(path).unwrap();
    assert!(common::logging::validate_jsonl_v2_only(&text).is_empty());
}

fn select(scope: &JobSessionScope, id: &str) {
    let id = id.to_string();
    scope.bind(Arc::new(move || {
        let id = id.clone();
        Box::pin(async move { Some(id) })
    }));
}

async fn call(tool: &dyn Tool, input: Value) -> Value {
    let output = tool.execute("attach-case", input, None).await.unwrap();
    assert!(!output.is_error, "{output:?}");
    output.details.unwrap()
}

fn send(stream: &mut TcpStream, sequence: &mut u64, mut message: Value) {
    *sequence += 1;
    message["seq"] = json!(*sequence);
    stream.write_all(&encode_frame(&message)).unwrap();
}

#[allow(clippy::needless_pass_by_value)]
fn reply(stream: &mut TcpStream, sequence: &mut u64, request: &Value, body: Value) {
    send(
        stream,
        sequence,
        json!({"type":"response","request_seq":request["seq"],
        "command":request["command"],"success":true,"body":body}),
    );
}

/// How long the peer waits for each request before declaring the client stuck.
const REQUEST_PATIENCE: Duration = Duration::from_secs(30);

// A read timeout only means "nothing yet". Retry it until one deadline for the
// whole request, so a slow client is not mistaken for a broken one.
fn read_patiently(
    reader: &mut BufReader<TcpStream>,
    buffer: &mut [u8],
    deadline: Instant,
) -> usize {
    loop {
        match reader.read(buffer) {
            Ok(read) => return read,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) =>
            {
                assert!(
                    Instant::now() < deadline,
                    "DAP client sent no complete request within {REQUEST_PATIENCE:?}"
                );
            }
            Err(error) => panic!("DAP peer read: {error}"),
        }
    }
}

// A small independent peer parser. Do not import Pi's crate-private decoder
// or discard its over-read scratch between requests. BufReader retains any
// buffered bytes beyond this exact header/body for the following request.
fn read_request(reader: &mut BufReader<TcpStream>) -> Option<Value> {
    let deadline = Instant::now() + REQUEST_PATIENCE;
    let mut header = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        if read_patiently(reader, &mut byte, deadline) == 0 {
            assert!(header.is_empty(), "EOF during DAP header");
            return None;
        }
        header.push(byte[0]);
        assert!(header.len() <= 4096, "oversized DAP header");
        if header.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let text = std::str::from_utf8(&header).unwrap();
    let length: usize = text
        .lines()
        .find_map(|line| line.strip_prefix("Content-Length: "))
        .unwrap()
        .parse()
        .unwrap();
    assert!(length > 0 && length <= 2 * 1024 * 1024);
    let mut bytes = vec![0; length];
    let mut filled = 0;
    while filled < length {
        let read = read_patiently(reader, &mut bytes[filled..], deadline);
        assert!(read > 0, "EOF during DAP body");
        filled += read;
    }
    Some(serde_json::from_slice(&bytes).unwrap())
}

fn peer(reject_attach: bool) -> (u16, std::thread::JoinHandle<Vec<Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let worker = std::thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let start = Instant::now();
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        start.elapsed() < Duration::from_secs(5),
                        "client never connected"
                    );
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("accept: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_millis(250)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut sequence = 0;
        let mut requests = Vec::new();
        let mut attach = None;
        while let Some(request) = read_request(&mut reader) {
            requests.push(request.clone());
            match request["command"].as_str().unwrap() {
                "initialize" => reply(
                    &mut stream,
                    &mut sequence,
                    &request,
                    json!({
                        "supportsConfigurationDoneRequest":true,"supportTerminateDebuggee":true,
                    }),
                ),
                "attach" if reject_attach => send(
                    &mut stream,
                    &mut sequence,
                    json!({
                        "type":"response","request_seq":request["seq"],"command":"attach",
                        "success":false,"message":"fixture attach refused",
                    }),
                ),
                "attach" => {
                    attach = Some(request.clone());
                    send(
                        &mut stream,
                        &mut sequence,
                        json!({"type":"event","event":"initialized"}),
                    );
                }
                "setBreakpoints" => {
                    let entries: Vec<_> = request["arguments"]["breakpoints"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|entry| json!({"verified":true,"line":entry["line"]}))
                        .collect();
                    reply(
                        &mut stream,
                        &mut sequence,
                        &request,
                        json!({"breakpoints":entries}),
                    );
                }
                "configurationDone" => {
                    send(
                        &mut stream,
                        &mut sequence,
                        json!({"type":"event","event":"stopped",
                        "body":{"threadId":7,"allThreadsStopped":true,"reason":"breakpoint"}}),
                    );
                    reply(&mut stream, &mut sequence, &request, json!({}));
                    reply(
                        &mut stream,
                        &mut sequence,
                        attach.as_ref().unwrap(),
                        json!({}),
                    );
                }
                "threads" => reply(
                    &mut stream,
                    &mut sequence,
                    &request,
                    json!({"threads":[{"id":7,"name":"attached"}]}),
                ),
                "disconnect" => reply(&mut stream, &mut sequence, &request, json!({})),
                other => panic!("unexpected DAP request {other}"),
            }
        }
        requests
    });
    (port, worker)
}

#[test]
fn port_attach_uses_registry_ownership_and_configures_before_running() {
    let harness = TestHarness::new("port_attach_uses_registry_ownership");
    let root = harness.temp_path(".");
    std::fs::write(root.join("app.py"), "answer = 42\n").unwrap();
    // Port attachment must work with no installed/registered local adapter.
    let registry = ToolRegistry::from_tools(vec![Box::new(
        DebugTool::new(&root, None).with_adapters(Vec::new()),
    )]);
    let scope = registry.job_session_scope();
    let tool = registry.get("debug").unwrap();
    assert!(
        tool.effects().processes(),
        "attachment must not weaken debugger approval effects"
    );
    assert!(tool.parameters()["properties"].get("port").is_some());
    let (port, peer) = peer(false);
    run(async {
        select(&scope, "attached-owner");
        let attached = call(
            tool,
            json!({"action":"attach","port":port,
            "pathMappings":[{"localRoot":".","remoteRoot":"/srv/app"}],
            "initialBreakpoints":[{"file":"app.py","line":1}]}),
        )
        .await;
        assert_eq!(attached["transport"], "tcp");
        assert_eq!(attached["adapterProcessOwned"], false);
        assert_eq!(attached["pathMappings"][0]["remoteRoot"], "/srv/app");
        select(&scope, "other-owner");
        assert_eq!(
            call(tool, json!({"action":"sessions"})).await["sessions"],
            json!([])
        );
        let error = tool
            .execute("foreign", json!({"action":"threads"}), None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("DAP_NO_SESSION"));
        select(&scope, "attached-owner");
        assert_eq!(
            call(tool, json!({"action":"threads"})).await["threads"][0]["name"],
            "attached"
        );
        let disconnected = call(tool, json!({"action":"disconnect"})).await;
        assert_eq!(disconnected["debuggeeTerminationRequested"], false);
        assert_eq!(
            call(tool, json!({"action":"sessions"})).await["sessions"],
            json!([])
        );
    });
    let requests = peer.join().unwrap();
    let commands: Vec<_> = requests
        .iter()
        .map(|item| item["command"].as_str().unwrap())
        .collect();
    assert_eq!(
        commands,
        [
            "initialize",
            "attach",
            "setBreakpoints",
            "configurationDone",
            "threads",
            "disconnect"
        ]
    );
    assert_eq!(requests[1]["arguments"]["subProcess"], false);
    assert!(requests[1]["arguments"].get("processId").is_none());
    assert_eq!(
        requests.last().unwrap()["arguments"]["terminateDebuggee"],
        false
    );
    finish(&harness);
}

#[test]
fn rejected_external_attach_does_not_publish_a_session_and_can_retry_explicitly() {
    let harness = TestHarness::new("rejected_external_attach_can_retry");
    let tool = DebugTool::new(&harness.temp_path("."), None).with_adapters(Vec::new());
    let (port, rejected) = peer(true);
    run(async {
        let error = tool
            .execute("rejected", json!({"action":"attach","port":port}), None)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("fixture attach refused"),
            "{error}"
        );
        assert_eq!(
            call(&tool, json!({"action":"sessions"})).await["sessions"],
            json!([])
        );
    });
    let requests = rejected.join().unwrap();
    assert_eq!(requests.len(), 2);
    let (port, accepted) = peer(false);
    run(async {
        call(&tool, json!({"action":"attach","port":port})).await;
        call(&tool, json!({"action":"disconnect"})).await;
    });
    accepted.join().unwrap();
    finish(&harness);
}

#[test]
fn invalid_endpoint_and_source_mapping_fail_before_connecting() {
    let harness = TestHarness::new("invalid_attach_preflight_no_connect");
    let tool = DebugTool::new(&harness.temp_path("."), None);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    run(async {
        for input in [
            json!({"action":"attach","port":port,"pid":123}),
            json!({"action":"attach","port":port,"adapter":"dlv"}),
            json!({"action":"attach","port":port,"host":"192.0.2.1"}),
            json!({"action":"attach","port":port,"pathMappings":[{"localRoot":"does-not-exist","remoteRoot":"/srv"}]}),
        ] {
            assert!(tool.execute("invalid", input, None).await.is_err());
        }
        assert_eq!(
            call(&tool, json!({"action":"sessions"})).await["sessions"],
            json!([])
        );
    });
    listener.set_nonblocking(true).unwrap();
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    finish(&harness);
}
