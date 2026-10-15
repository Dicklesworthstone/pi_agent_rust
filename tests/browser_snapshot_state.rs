//! Public snapshot workflows through production HTTP/WebSocket transport.
//! This peer supplies recorded-shaped AX responses; it is not live Chromium.
#![forbid(unsafe_code)]

use pi::browser::BrowserTool;
use pi::tools::Tool;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

type Step = (&'static str, Value, Value);

fn accept(listener: &TcpListener) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_read_timeout(Some(Duration::from_secs(8))).unwrap();
                stream.set_write_timeout(Some(Duration::from_secs(8))).unwrap();
                return stream;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "snapshot peer received no connection");
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("snapshot peer accept: {error}"),
        }
    }
}

fn headers(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        assert!(bytes.len() < 16384);
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
    }
    String::from_utf8(bytes).unwrap()
}

fn request(stream: &mut TcpStream) -> Option<Value> {
    let mut first = [0];
    match stream.read(&mut first) {
        Ok(0) => return None,
        Err(error) if matches!(error.kind(), std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe) => return None,
        Ok(_) => {}
        Err(error) => panic!("snapshot peer frame: {error}"),
    }
    let close = first[0] == 0x88;
    assert!(close || first[0] == 0x81);
    let mut second = [0];
    stream.read_exact(&mut second).unwrap();
    assert_ne!(second[0] & 0x80, 0);
    let size = match second[0] & 0x7f {
        126 => { let mut bytes = [0; 2]; stream.read_exact(&mut bytes).unwrap(); usize::from(u16::from_be_bytes(bytes)) }
        127 => { let mut bytes = [0; 8]; stream.read_exact(&mut bytes).unwrap(); usize::try_from(u64::from_be_bytes(bytes)).unwrap() }
        value => usize::from(value),
    };
    assert!(size <= 128 * 1024);
    let mut mask = [0; 4];
    stream.read_exact(&mut mask).unwrap();
    let mut bytes = vec![0; size];
    stream.read_exact(&mut bytes).unwrap();
    for (index, byte) in bytes.iter_mut().enumerate() { *byte ^= mask[index % 4]; }
    if close { None } else { Some(serde_json::from_slice(&bytes).unwrap()) }
}

fn reply(stream: &mut TcpStream, value: &Value) {
    let bytes = serde_json::to_vec(value).unwrap();
    stream.write_all(&[0x81]).unwrap();
    if bytes.len() < 126 {
        stream.write_all(&[u8::try_from(bytes.len()).unwrap()]).unwrap();
    } else {
        stream.write_all(&[126]).unwrap();
        stream.write_all(&u16::try_from(bytes.len()).unwrap().to_be_bytes()).unwrap();
    }
    stream.write_all(&bytes).unwrap();
}

fn peer(scripts: Vec<Vec<Step>>) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let worker = thread::spawn(move || {
        for script in scripts {
            let mut discovery = accept(&listener);
            assert!(headers(&mut discovery).starts_with("GET /json/version "));
            let body = json!({"webSocketDebuggerUrl":format!("ws://{address}/devtools/browser/snapshot")}).to_string();
            write!(discovery, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            drop(discovery);
            let mut socket = accept(&listener);
            let handshake = headers(&mut socket);
            assert!(handshake.starts_with("GET /devtools/browser/snapshot "));
            let key = handshake.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("Sec-WebSocket-Key").then(|| value.trim())
            }).unwrap();
            let accept = asupersync::net::websocket::compute_accept_key(key);
            write!(socket, "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").unwrap();
            for (method, params, result) in script {
                let received = request(&mut socket).expect("missing snapshot command");
                assert_eq!(received["method"], method);
                for (key, expected) in params.as_object().unwrap() {
                    assert_eq!(&received["params"][key], expected, "{method} parameter {key}");
                }
                let mut response = json!({"id":received["id"], "result":result});
                if let Some(session) = received.get("sessionId") {
                    assert_eq!(session, "snapshot-session");
                    response["sessionId"] = session.clone();
                }
                reply(&mut socket, &response);
            }
            assert!(request(&mut socket).is_none(), "observation sent an unexpected extra command");
        }
    });
    (format!("http://{address}"), worker)
}

fn frame(loader: &str) -> Value {
    json!({"frameTree":{"frame":{"id":"main-frame","loaderId":loader,"url":"about:blank"}}})
}

fn node(id: u64, name: &str, checked: &str) -> Value {
    json!({"nodeId":id.to_string(), "backendDOMNodeId":id, "ignored":false,
        "role":{"type":"role","value":"checkbox"}, "name":{"type":"computedString","value":name},
        "value":{"type":"string","value":"PRIVATE-FIELD-VALUE"}, "properties":[
            {"name":"checked","value":{"type":"tristate","value":checked}},
            {"name":"disabled","value":{"type":"boolean","value":false}},
            {"name":"valuetext","value":{"type":"string","value":"PRIVATE-FIELD-VALUE"}}
        ]})
}

fn snapshot(first: bool, loader: &str, final_loader: &str, nodes: Vec<Value>) -> Vec<Step> {
    let mut script = Vec::new();
    if first {
        script.push(("Browser.setDownloadBehavior", json!({"behavior":"deny"}), json!({})));
    }
    script.extend([
        ("Target.getTargets", json!({}), json!({"targetInfos":[{"targetId":"page","type":"page","url":"about:blank","title":"Test"}]})),
        ("Target.attachToTarget", json!({"targetId":"page","flatten":true}), json!({"sessionId":"snapshot-session"})),
        ("Page.getFrameTree", json!({}), frame(loader)),
        ("Accessibility.getFullAXTree", json!({"frameId":"main-frame"}), json!({"nodes":nodes})),
    ]);
    for node in nodes.iter().take(200) {
        script.push(("DOM.describeNode", json!({"backendNodeId":node["backendDOMNodeId"],"depth":0}), json!({"node":{"localName":"input"}})));
    }
    script.push(("Runtime.evaluate", json!({"expression":"({url: location.href, title: document.title})"}), json!({"result":{"type":"object","value":{"url":"about:blank","title":"Test"}}})));
    script.push(("Page.getFrameTree", json!({}), frame(final_loader)));
    script
}

#[test]
fn native_snapshots_report_checked_changes_on_the_same_refs_without_extra_commands() {
    let (endpoint, worker) = peer(vec![
        snapshot(true, "one", "one", vec![node(10, "Approve", "false"), node(20, "Other", "mixed")]),
        snapshot(false, "one", "one", vec![node(20, "Other", "mixed"), node(10, "Approve", "true")]),
    ]);
    let directory = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(directory.path()).with_mock(false).with_cdp_endpoint(endpoint);
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    runtime.block_on(async {
        let initial = tool.execute("first", json!({"action":"snapshot","tab":"page"}), None).await.unwrap();
        let details = initial.details.unwrap();
        assert_eq!(details["element_states"]["@e1"]["states"]["checked"], false);
        assert_eq!(details["element_states"]["@e2"]["states"]["checked"], "mixed");
        assert_eq!(details["snapshot_changes"]["compared"], false);
        let next = tool.execute("second", json!({"action":"ax_tree","tab":"page"}), None).await.unwrap();
        let details = next.details.unwrap();
        assert_eq!(details["element_states"]["@e1"]["states"]["checked"], true);
        assert_eq!(details["snapshot_changes"]["changed"], json!(["@e1"]));
        assert_eq!(details["snapshot_changes"]["unchanged_count"], 1);
        assert_eq!(details["snapshot_changes"]["order_changed"], true);
        assert_eq!(details["ax_tree"]["nodes"][0]["state_observation"]["states"]["checked"], "mixed");
        assert!(details["snapshot"]["summary"].as_str().unwrap().contains("checked=true"));
        assert!(!details.to_string().contains("PRIVATE-FIELD-VALUE"));
    });
    worker.join().unwrap();
}

#[test]
fn failed_snapshot_does_not_advance_history_and_navigation_starts_a_new_baseline() {
    let (endpoint, worker) = peer(vec![
        snapshot(true, "one", "one", vec![node(10, "A", "false")]),
        snapshot(false, "one", "unexpected", vec![node(10, "A", "true")]),
        snapshot(false, "one", "one", vec![node(10, "A", "true")]),
        snapshot(false, "two", "two", vec![node(10, "New document", "true")]),
    ]);
    let directory = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(directory.path()).with_mock(false).with_cdp_endpoint(endpoint);
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    runtime.block_on(async {
        let args = json!({"action":"snapshot","tab":"page"});
        tool.execute("first", args.clone(), None).await.unwrap();
        let error = tool.execute("raced", args.clone(), None).await.unwrap_err();
        assert!(error.to_string().contains("navigated during snapshot"));
        let next = tool.execute("retained", args.clone(), None).await.unwrap().details.unwrap();
        assert_eq!(next["snapshot_changes"]["changed"], json!(["@e1"]));
        let navigated = tool.execute("new", args, None).await.unwrap().details.unwrap();
        assert_eq!(navigated["snapshot_changes"]["compared"], false);
        assert_eq!(navigated["snapshot_changes"]["metadata_changed"], Value::Null);
        assert!(navigated["element_states"].get("@e1").is_none());
    });
    worker.join().unwrap();
}

#[test]
#[ignore = "requires an installed Chromium and explicit DSR live-browser lane"]
fn live_native_snapshots_observe_a_real_checkbox_toggle() {
    let directory = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(directory.path()).with_mock(false);
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    runtime.block_on(async {
        tool.execute("open", json!({"action":"open","tab":"check","url":"about:blank"}), None).await.unwrap();
        tool.execute("fixture", json!({"action":"evaluate","tab":"check","script":"document.body.innerHTML = '<label><input id=check type=checkbox>Approve</label>'"}), None).await.unwrap();
        let initial = tool.execute("before", json!({"action":"snapshot","tab":"check"}), None).await.unwrap().details.unwrap();
        let reference = initial["snapshot"]["elements"].as_array().unwrap().iter().find(|element| element["role"] == "checkbox").unwrap()["ref_id"].as_str().unwrap().to_owned();
        assert_eq!(initial["element_states"][&reference]["states"]["checked"], false);
        tool.execute("toggle", json!({"action":"set_checked","tab":"check","selector":reference,"checked":true}), None).await.unwrap();
        let after = tool.execute("after", json!({"action":"snapshot","tab":"check"}), None).await.unwrap().details.unwrap();
        assert_eq!(after["element_states"][&reference]["states"]["checked"], true);
        assert!(after["snapshot_changes"]["changed"].as_array().unwrap().contains(&json!(reference)));
        tool.execute("stop", json!({"action":"stop"}), None).await.unwrap();
    });
}
