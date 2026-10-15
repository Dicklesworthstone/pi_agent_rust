//! Exercise the actual BrowserTool HTTP/WebSocket route, not a simulated browser.
//! This deterministic protocol peer is not proof of live Chromium behavior.
#![forbid(unsafe_code)]
#![recursion_limit = "256"]

use pi::browser::BrowserTool;
use pi::tools::Tool;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

fn headers(socket: &mut TcpStream) -> String {
    socket.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    socket.set_write_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        assert!(bytes.len() < 16 * 1024);
        let mut byte = [0];
        socket.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
    }
    String::from_utf8(bytes).unwrap()
}

fn accept(listener: &TcpListener) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match listener.accept() {
            Ok((socket, _)) => return socket,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "no browser connection");
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("accept: {error}"),
        }
    }
}

fn read_frame(socket: &mut TcpStream) -> Value {
    let mut head = [0; 2];
    socket.read_exact(&mut head).unwrap();
    assert_eq!(head[0], 0x81);
    assert_ne!(head[1] & 0x80, 0);
    let size = match head[1] & 0x7f {
        126 => {
            let mut size = [0; 2];
            socket.read_exact(&mut size).unwrap();
            usize::from(u16::from_be_bytes(size))
        }
        127 => {
            let mut size = [0; 8];
            socket.read_exact(&mut size).unwrap();
            usize::try_from(u64::from_be_bytes(size)).unwrap()
        }
        size => usize::from(size),
    };
    assert!(size < 1024 * 1024);
    let mut mask = [0; 4];
    socket.read_exact(&mut mask).unwrap();
    let mut bytes = vec![0; size];
    socket.read_exact(&mut bytes).unwrap();
    for (i, byte) in bytes.iter_mut().enumerate() { *byte ^= mask[i % 4]; }
    serde_json::from_slice(&bytes).unwrap()
}

fn write_frame(socket: &mut TcpStream, value: &Value) {
    let bytes = serde_json::to_vec(value).unwrap();
    if bytes.len() < 126 {
        socket.write_all(&[0x81, u8::try_from(bytes.len()).unwrap()]).unwrap();
    } else {
        let length = u16::try_from(bytes.len()).unwrap();
        socket.write_all(&[0x81, 126]).unwrap();
        socket.write_all(&length.to_be_bytes()).unwrap();
    }
    socket.write_all(&bytes).unwrap();
}

fn reply(socket: &mut TcpStream, method: &str, result: Value) -> Value {
    let request = read_frame(socket);
    assert_eq!(request["method"], method);
    if method.starts_with("Page.") || method.starts_with("Runtime.") || method.starts_with("Network.") {
        assert_eq!(request["sessionId"], "session");
    }
    write_frame(socket, &json!({"id":request["id"],"sessionId":request["sessionId"],"result":result}));
    request
}

fn event(socket: &mut TcpStream, method: &str, params: Value) {
    write_frame(socket, &json!({"sessionId":"session","method":method,"params":params}));
}

#[derive(Clone, Copy)]
enum Scenario { Success, ActionError, Passive, NetworkRefused, ScopeLost, ConsoleOnly, Settled }

fn peer(scenario: Scenario) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let mut discovery = accept(&listener);
        assert!(headers(&mut discovery).starts_with("GET /json/version "));
        let body = json!({"webSocketDebuggerUrl":format!("ws://{addr}/devtools/browser/fixture")}).to_string();
        write!(discovery, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        drop(discovery);
        let mut socket = accept(&listener);
        let handshake = headers(&mut socket);
        let key = handshake.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("sec-websocket-key").then(|| value.trim())
        }).unwrap();
        let accept = asupersync::net::websocket::compute_accept_key(key);
        write!(socket, "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").unwrap();
        reply(&mut socket, "Browser.setDownloadBehavior", json!({}));
        reply(&mut socket, "Target.getTargets", json!({"targetInfos":[{
            "targetId":"page","type":"page","url":"https://example.com/app","title":"fixture"
        }]}));
        reply(&mut socket, "Target.attachToTarget", json!({"sessionId":"session"}));
        reply(&mut socket, "Page.getFrameTree", json!({"frameTree":{"frame":{
            "id":"main","loaderId":"loader","url":"https://example.com/app"
        }}}));
        reply(&mut socket, "Page.enable", json!({}));
        let runtime = read_frame(&mut socket);
        assert_eq!(runtime["method"], "Runtime.enable");
        assert_eq!(runtime["sessionId"], "session");
        for (id, frame) in [(1, "main"), (2, "child")] {
            event(&mut socket, "Runtime.executionContextCreated", json!({"context":{
                "id":id,"origin":"https://example.com","auxData":{"frameId":frame,"isDefault":true}
            }}));
        }
        event(&mut socket, "Runtime.consoleAPICalled", json!({
            "type":"log","executionContextId":1,"args":[{"type":"string","value":"buffered setup"}]
        }));
        write_frame(&mut socket, &json!({"id":runtime["id"],"sessionId":"session","result":{}}));
        if !matches!(scenario, Scenario::ConsoleOnly) {
        let network = read_frame(&mut socket);
        assert_eq!(network["method"], "Network.enable");
        assert_eq!(network["sessionId"], "session");
        assert_eq!(network["params"]["maxPostDataSize"], 0);
        if matches!(scenario, Scenario::NetworkRefused) {
            write_frame(&mut socket, &json!({"id":network["id"],"sessionId":"session","error":{"code":-32601,"message":"unsupported"}}));
        } else {
            if matches!(scenario, Scenario::ScopeLost) {
                event(&mut socket, "Page.frameNavigated", json!({"frame":{"id":"main","url":"https://forbidden.test/"}}));
            }
            write_frame(&mut socket, &json!({"id":network["id"],"sessionId":"session","result":{}}));
        }
        }
        if !matches!(scenario, Scenario::Passive | Scenario::NetworkRefused | Scenario::ScopeLost) {
            let request = read_frame(&mut socket);
            assert_eq!(request["method"], "Runtime.evaluate");
            assert_eq!(request["params"]["expression"], "triggerOnce()");
            for (session, context, text) in [("session",1,"during action"),("session",2,"PRIVATE-CHILD"),("other",1,"PRIVATE-SESSION")] {
                write_frame(&mut socket, &json!({"sessionId":session,"method":"Runtime.consoleAPICalled","params":{
                    "type":"error","executionContextId":context,"args":[{"type":"string","value":text}]
                }}));
            }
            event(&mut socket, "Network.requestWillBeSent", json!({
                "requestId":"r1","frameId":"main","documentURL":"https://example.com/app",
                "timestamp":1,"type":"Fetch","request":{"url":"https://example.com/api?PRIVATE#PRIVATE","method":"GET","headers":{"Cookie":"PRIVATE"}}
            }));
            event(&mut socket, "Network.responseReceived", json!({
                "requestId":"r1","frameId":"main","timestamp":1.25,
                "response":{"url":"https://example.com/api?PRIVATE","status":503,"headers":{"Set-Cookie":"PRIVATE"}}
            }));
            event(&mut socket, "Network.loadingFinished", json!({"requestId":"r1","timestamp":1.5,"encodedDataLength":99}));
            event(&mut socket, "Network.requestWillBeSent", json!({
                "requestId":"r2","frameId":"main","documentURL":"https://example.com/app",
                "timestamp":2,"type":"Fetch","request":{"url":"https://example.com/offline","method":"GET"}
            }));
            event(&mut socket, "Network.loadingFailed", json!({"requestId":"r2","timestamp":2.5,"errorText":"net::ERR_CONNECTION_REFUSED"}));
            if matches!(scenario, Scenario::ActionError) {
                write_frame(&mut socket, &json!({"id":request["id"],"sessionId":"session","result":{
                    "exceptionDetails":{"text":"Uncaught","exception":{"description":"trigger failed"}}
                }}));
            } else {
                write_frame(&mut socket, &json!({"id":request["id"],"sessionId":"session","result":{"result":{"type":"boolean","value":true}}}));
            }
            if matches!(scenario, Scenario::Settled) {
                event(&mut socket, "Runtime.consoleAPICalled", json!({
                    "type":"warn","executionContextId":1,"args":[{"type":"string","value":"after action"}]
                }));
            }
        }
        // A successful return or setup refusal must never replay a command,
        // issue a body fetch, or leave a background socket reader alive.
        let mut end = [0];
        match socket.read(&mut end) {
            Ok(0) => {},
            Ok(_) => assert_eq!(end[0] & 0x0f, 8, "unexpected extra browser command"),
            Err(error) => assert!(matches!(error.kind(), std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted), "socket remained live: {error}"),
        }
    });
    (format!("http://{addr}"), handle)
}

fn run(scenario: Scenario) -> pi::tools::ToolOutput {
    let (endpoint, handle) = peer(scenario);
    let dir = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(dir.path()).with_mock(false).with_cdp_endpoint(endpoint)
        .with_domain_allowlist(Some(vec!["example.com".into()]));
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    let mut args = json!({"action":"evaluate","tab":"page","script":"triggerOnce()",
        "diagnostics":{"network":true,"settle_ms":0},"timeout_ms":5000});
    if matches!(scenario, Scenario::Passive) {
        args["action"] = json!("diagnostics");
        args.as_object_mut().unwrap().remove("script");
    }
    if matches!(scenario, Scenario::ConsoleOnly) {
        args["diagnostics"]["network"] = json!(false);
    }
    if matches!(scenario, Scenario::Settled) {
        args["diagnostics"]["settle_ms"] = json!(100);
    }
    let result = runtime.block_on(tool.execute("browser-diagnostics", args, None)).unwrap();
    handle.join().unwrap();
    eprintln!("{}", json!({"schema":"pi.test.browser-diagnostics.v1","is_error":result.is_error,
        "completion":result.details.as_ref().unwrap()["diagnostics"]["completion"]}));
    result
}

#[test]
fn capture_precedes_action_and_correlates_http_and_transport_failures_without_replay() {
    let result = run(Scenario::Success);
    assert!(!result.is_error);
    let details = result.details.as_ref().unwrap();
    assert_eq!(details["result"], true);
    let report = &details["diagnostics"];
    assert_eq!(report["console"].as_array().unwrap().len(), 2);
    assert_eq!(report["console"][0]["phase"], "setup");
    assert_eq!(report["console"][1]["phase"], "action");
    assert_eq!(report["network"]["requests"][0]["response"]["status"].as_f64(), Some(503.0));
    assert_eq!(report["network"]["requests"][1]["error_code"], "net::ERR_CONNECTION_REFUSED");
    assert!(!report.to_string().contains("PRIVATE"));
}

#[test]
fn original_action_failure_keeps_diagnostics_without_becoming_success() {
    let result = run(Scenario::ActionError);
    assert!(result.is_error);
    let report = &result.details.as_ref().unwrap()["diagnostics"];
    assert_eq!(report["completion"], "action_error");
    assert_eq!(report["action_started"], true);
    assert_eq!(report["action_succeeded"], false);
    assert_eq!(report["network"]["requests"].as_array().unwrap().len(), 2);
}

#[test]
fn passive_capture_never_executes_a_page_script() {
    let result = run(Scenario::Passive);
    assert!(!result.is_error);
    assert_eq!(result.details.as_ref().unwrap()["diagnostics"]["network"]["requests"], json!([]));
}

#[test]
fn domain_setup_failure_prevents_original_action_and_is_not_a_fake_empty_capture() {
    let result = run(Scenario::NetworkRefused);
    assert!(result.is_error);
    assert_eq!(result.details.as_ref().unwrap()["diagnostics"]["action_started"], false);
}

#[test]
fn forbidden_navigation_during_setup_prevents_action_start() {
    let result = run(Scenario::ScopeLost);
    assert!(result.is_error);
    assert_eq!(result.details.as_ref().unwrap()["diagnostics"]["completion"], "scope_lost");
    assert_eq!(result.details.as_ref().unwrap()["diagnostics"]["action_started"], false);
}

#[test]
fn disabled_network_does_not_enable_the_domain_or_retain_unsolicited_requests() {
    let result = run(Scenario::ConsoleOnly);
    assert!(!result.is_error);
    let report = &result.details.as_ref().unwrap()["diagnostics"];
    assert!(report.get("network").is_none());
    assert_eq!(report["console"].as_array().unwrap().len(), 2);
}

#[test]
fn fixed_settle_window_receives_events_after_the_action_reply() {
    let result = run(Scenario::Settled);
    assert!(!result.is_error);
    let report = &result.details.as_ref().unwrap()["diagnostics"];
    assert_eq!(report["completion"], "window_elapsed");
    assert_eq!(report["console"].as_array().unwrap().len(), 3);
    assert_eq!(report["console"][2]["phase"], "settle");
}
