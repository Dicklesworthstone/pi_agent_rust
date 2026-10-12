//! Exercise the public browser tool through real HTTP/WebSocket transport.
//! The peer validates CDP routing, not Chromium's renderer or native input;
//! a live Chromium run and the DSR quality gate remain separate obligations.

use super::*;
use crate::browser::BrowserTool;
use crate::tools::Tool;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    None,
    WrongSession,
    Navigate,
    Detach,
    BlockedAncestor,
}

#[derive(Default)]
struct Observed {
    worlds: usize,
    evaluations: usize,
    inputs: usize,
    text: String,
}

fn headers(socket: &mut TcpStream) -> String {
    socket.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    socket.set_write_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        assert!(bytes.len() < 16384);
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
            Ok((socket, _)) => {
                socket.set_nonblocking(false).unwrap();
                return socket;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "frame peer received no connection");
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("frame peer accept: {error}"),
        }
    }
}

fn read_frame(socket: &mut TcpStream) -> Option<Value> {
    let mut head = [0; 2];
    match socket.read_exact(&mut head) {
        Ok(()) => {}
        Err(error) if matches!(error.kind(), std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset) => return None,
        Err(error) => panic!("frame peer receive: {error}"),
    }
    if head[0] & 0xf == 8 {
        return None;
    }
    assert_eq!(head[0], 0x81);
    assert_ne!(head[1] & 0x80, 0);
    let size = match head[1] & 0x7f {
        126 => {
            let mut bytes = [0; 2];
            socket.read_exact(&mut bytes).unwrap();
            usize::from(u16::from_be_bytes(bytes))
        }
        127 => panic!("unexpected oversized frame request"),
        size => usize::from(size),
    };
    let mut mask = [0; 4];
    socket.read_exact(&mut mask).unwrap();
    let mut bytes = vec![0; size];
    socket.read_exact(&mut bytes).unwrap();
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte ^= mask[index % 4];
    }
    Some(serde_json::from_slice(&bytes).unwrap())
}

fn write_frame(socket: &mut TcpStream, value: &Value) {
    let bytes = serde_json::to_vec(value).unwrap();
    let mut frame = vec![0x81];
    if bytes.len() < 126 {
        frame.push(u8::try_from(bytes.len()).unwrap());
    } else {
        frame.push(126);
        frame.extend_from_slice(&u16::try_from(bytes.len()).unwrap().to_be_bytes());
    }
    frame.extend(bytes);
    socket.write_all(&frame).unwrap();
}

fn attached(parent: &str, session: &str, target: &str) -> Value {
    json!({"method":"Target.attachedToTarget", "sessionId":parent,
        "params":{"sessionId":session,"waitingForDebugger":false,
            "targetInfo":{"targetId":target,"type":"iframe"}}})
}

fn frame_tree(session: &str, fault: Fault, after_world: bool) -> Value {
    let node = |id: &str, parent: Option<&str>, url: &str| {
        let mut frame = json!({"id":id,"loaderId":format!("loader-{id}"),"url":url,"name":id});
        if let Some(parent) = parent {
            frame["parentId"] = json!(parent);
        }
        json!({"frame":frame})
    };
    let tree = match session {
        "page" => node("main", None, "https://example.com/root"),
        "remote-session" => {
            let url = if fault == Fault::BlockedAncestor {
                "https://blocked.test/PRIVATE-CANARY"
            } else {
                "https://example.com/remote"
            };
            let mut remote = node("remote", Some("main"), url);
            if fault == Fault::Navigate && after_world {
                remote["frame"]["loaderId"] = json!("replacement-loader");
            }
            remote["childFrames"] = json!([node("inside", Some("remote"), "about:srcdoc")]);
            remote
        }
        "nested-session" => node("nested", Some("inside"), "https://example.com/form"),
        _ => panic!("foreign renderer queried: {session}"),
    };
    json!({"frameTree":tree})
}

#[allow(clippy::too_many_lines)]
fn serve(socket: &mut TcpStream, fault: Fault) -> Observed {
    let mut observed = Observed::default();
    let mut configured = std::collections::BTreeSet::new();
    for _ in 0..512 {
        let Some(request) = read_frame(socket) else { return observed };
        let method = request["method"].as_str().unwrap();
        let session = request["sessionId"].as_str().unwrap_or_default();
        let params = &request["params"];
        let result = match method {
            "Browser.setDownloadBehavior" => {
                assert!(session.is_empty());
                assert_eq!(params["behavior"], "deny");
                json!({})
            }
            "Target.getTargets" => json!({"targetInfos":[
                {"type":"page","targetId":"page-1","url":"https://example.com/root"},
                {"type":"page","targetId":"other-page","url":"https://example.com/root"},
                {"type":"iframe","targetId":"foreign-frame","url":"https://example.com/form"},
            ]}),
            "Target.attachToTarget" => {
                assert!(session.is_empty());
                assert_eq!(params, &json!({"targetId":"page-1","flatten":true}));
                json!({"sessionId":"page"})
            }
            "Target.setAutoAttach" => {
                assert_eq!(params["autoAttach"], true);
                assert_eq!(params["flatten"], true);
                assert_eq!(params["waitForDebuggerOnStart"], false);
                assert_eq!(params["filter"], json!([{"type":"iframe","exclude":false},{"exclude":true}]));
                if configured.insert(session.to_owned()) {
                    match session {
                        "page" => {
                            write_frame(socket, &attached("page", "remote-session", "remote"));
                            write_frame(socket, &attached("other-page", "foreign-session", "foreign-frame"));
                        }
                        "remote-session" => write_frame(socket, &attached("remote-session", "nested-session", "nested")),
                        "nested-session" => {}
                        _ => panic!("auto-attached unrelated target"),
                    }
                }
                json!({})
            }
            "Page.getFrameTree" => frame_tree(session, fault, observed.worlds > 0),
            "Page.createIsolatedWorld" => {
                assert_eq!(session, "nested-session");
                assert_eq!(params["frameId"], "nested");
                observed.worlds += 1;
                if fault == Fault::Detach {
                    write_frame(socket, &json!({"method":"Target.detachedFromTarget",
                        "sessionId":"remote-session","params":{"sessionId":"nested-session"}}));
                    return observed;
                }
                json!({"executionContextId":7})
            }
            "Runtime.evaluate" => {
                assert_eq!(session, "nested-session");
                assert_eq!(params["contextId"], 7);
                observed.evaluations += 1;
                if fault == Fault::WrongSession {
                    write_frame(socket, &json!({"id":request["id"],"sessionId":"foreign-session",
                        "result":{"result":{"value":"PRIVATE-CANARY"}}}));
                    return observed;
                }
                if params["expression"].as_str().unwrap().contains("location.href") {
                    json!({"result":{"type":"object","value":{"url":"https://example.com/form","title":"Remote form"}}})
                } else {
                    json!({"result":{"type":"string","value":"REMOTE-CANARY"}})
                }
            }
            "Accessibility.getFullAXTree" => {
                assert_eq!(session, "nested-session");
                assert_eq!(params["frameId"], "nested");
                json!({"nodes":[{"nodeId":"ax-1","backendDOMNodeId":123,
                    "role":{"value":"textbox"},"name":{"value":"Remote field"},"ignored":false}]})
            }
            "DOM.describeNode" => {
                assert_eq!(session, "nested-session");
                json!({"node":{"backendNodeId":123,"localName":"input","nodeName":"INPUT"}})
            }
            "DOM.resolveNode" => {
                assert_eq!(session, "nested-session");
                assert_eq!(params["executionContextId"], 7);
                json!({"object":{"objectId":"field-handle"}})
            }
            "Runtime.callFunctionOn" => {
                assert_eq!(session, "nested-session");
                if params["functionDeclaration"].as_str().unwrap().contains("Document.prototype.querySelector") {
                    assert_eq!(params["executionContextId"], 7);
                    json!({"result":{"type":"object","objectId":"field-handle"}})
                } else {
                    assert_eq!(params["objectId"], "field-handle");
                    let result = if params["arguments"][0]["value"] == "verify_fill" {
                        params["arguments"][1]["value"]["text"].as_str() == Some(observed.text.as_str())
                    } else { true };
                    json!({"result":{"type":"boolean","value":result}})
                }
            }
            "DOM.scrollIntoViewIfNeeded" | "Runtime.releaseObject" => {
                assert_eq!(session, "nested-session");
                json!({})
            }
            "Page.bringToFront" => {
                assert_eq!(session, "page");
                json!({})
            }
            "Input.insertText" => {
                assert_eq!(session, "page");
                observed.inputs += 1;
                observed.text = params["text"].as_str().unwrap().to_owned();
                json!({})
            }
            "Input.dispatchKeyEvent" => {
                assert_eq!(session, "page");
                observed.inputs += 1;
                json!({})
            }
            _ => panic!("unexpected CDP command: {method}"),
        };
        let mut response = json!({"id":request["id"],"result":result});
        if !session.is_empty() {
            response["sessionId"] = json!(session);
        }
        write_frame(socket, &response);
    }
    panic!("frame operation exceeded fixture command budget")
}

fn peer(fault: Fault) -> (String, thread::JoinHandle<Observed>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let mut discovery = accept(&listener);
        assert!(headers(&mut discovery).starts_with("GET /json/version "));
        let body = json!({"webSocketDebuggerUrl":format!("ws://{address}/devtools/browser/frames")}).to_string();
        write!(discovery, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        drop(discovery);
        let mut socket = accept(&listener);
        let request = headers(&mut socket);
        let key = request.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("sec-websocket-key").then(|| value.trim())
        }).unwrap();
        let key = asupersync::net::websocket::compute_accept_key(key);
        write!(socket, "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {key}\r\n\r\n").unwrap();
        serve(&mut socket, fault)
    });
    (format!("http://{address}"), handle)
}

fn run(fault: Fault, mut args: Value) -> (Result<ToolOutput>, Observed) {
    args["tab"] = json!("page-1");
    args["timeout_ms"] = json!(5000);
    let (endpoint, handle) = peer(fault);
    let dir = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(dir.path()).with_mock(false)
        .with_cdp_endpoint(endpoint)
        .with_domain_allowlist(Some(vec!["example.com".into()]));
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    let result = runtime.block_on(tool.execute("cross-process-frames", args, None));
    drop(tool);
    (result, handle.join().unwrap())
}

#[test]
fn public_list_and_evaluation_follow_nested_owned_renderer_sessions() {
    let (result, observed) = run(Fault::None, json!({"action":"list_frames"}));
    let details = result.unwrap().details.unwrap();
    let frames = details["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 4);
    assert!(!frames.iter().any(|frame| frame["frame_id"] == "foreign-frame"));
    assert_eq!(frames.iter().find(|frame| frame["frame_id"] == "nested").unwrap()["out_of_process"], true);
    assert_eq!(observed.worlds, 0);
    let (result, observed) = run(Fault::None, json!({"action":"evaluate","frame":"nested","script":"document.title"}));
    assert_eq!(result.unwrap().details.unwrap()["result"], "REMOTE-CANARY");
    assert_eq!(observed.evaluations, 1);
    assert_eq!(observed.inputs, 0);
}

#[test]
fn public_remote_snapshot_and_fill_use_the_selected_dom_not_the_root() {
    let (result, _) = run(Fault::None, json!({"action":"snapshot","frame":"nested"}));
    let details = result.unwrap().details.unwrap();
    assert_eq!(details["snapshot"]["title"], "Remote form");
    assert_eq!(details["snapshot"]["elements"][0]["text"], "Remote field");
    assert_eq!(details["frame_id"], "nested");
    let text = "  literal \"🦀\"  ";
    let (result, observed) = run(Fault::None, json!({"action":"fill","frame":"nested","selector":"#field","text":text}));
    assert!(!result.unwrap().is_error);
    assert_eq!(observed.inputs, 1);
    assert_eq!(observed.text, text);
}

#[test]
fn remote_wait_and_focused_keypress_keep_document_and_input_routes_separate() {
    let (result, observed) = run(Fault::None, json!({"action":"wait_for","frame":"nested","selector":"#field"}));
    assert_eq!(result.unwrap().details.unwrap()["found"], true);
    assert_eq!(observed.inputs, 0);
    let (result, observed) = run(Fault::None, json!({"action":"press","frame":"nested","selector":"#field","key":"End"}));
    assert!(!result.unwrap().is_error);
    assert!(observed.inputs > 0);
}

#[test]
fn remote_navigation_detach_and_response_mixups_never_fall_back_to_main() {
    for fault in [Fault::Navigate, Fault::Detach, Fault::WrongSession] {
        let (result, observed) = run(fault, json!({"action":"evaluate","frame":"nested","script":"document.title"}));
        let error = result.unwrap_err().to_string();
        assert!(!error.contains("PRIVATE-CANARY"));
        assert_eq!(observed.evaluations, usize::from(fault == Fault::WrongSession));
        assert_eq!(observed.inputs, 0);
    }
}

#[test]
fn blocked_ancestors_and_foreign_frames_are_rejected_before_entering_a_world() {
    for (fault, frame) in [(Fault::BlockedAncestor, "nested"), (Fault::None, "foreign-frame")] {
        let (result, observed) = run(fault, json!({"action":"evaluate","frame":frame,"script":"document.title"}));
        assert!(result.is_err());
        assert_eq!(observed.worlds, 0);
        assert_eq!(observed.evaluations, 0);
        assert_eq!(observed.inputs, 0);
    }
}

#[test]
fn remote_pointer_actions_never_dispatch_unprojected_coordinates() {
    for action in ["click", "scroll"] {
        let (result, observed) = run(Fault::None, json!({"action":action,"frame":"nested","selector":"#field"}));
        assert!(result.unwrap_err().to_string().contains("page-coordinate projection"));
        assert_eq!(observed.inputs, 0);
    }
}
