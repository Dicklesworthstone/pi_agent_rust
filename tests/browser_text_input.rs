//! Public native input transport regressions; the loopback peer is not Chromium.
#![forbid(unsafe_code)]
#![recursion_limit = "512"]

use pi::browser::BrowserTool;
use pi::tools::{Tool, ToolOutput};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

const TEXT: &str = "PRIVATE-CANARY 🦀";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    None, FocusLost, MalformedFocus, LookupNavigation, EditNavigation,
    InsertNavigation, DownFailure, UpFailure, DownNavigation, RevertedFill,
}

#[derive(Default, Debug)]
struct Seen {
    queries: usize,
    focus_checks: usize,
    edits: usize,
    inserts: usize,
    downs: usize,
    ups: usize,
    fronts: usize,
    changed: bool,
}

fn accept(listener: &TcpListener) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match listener.accept() {
            Ok(socket) => {
                socket.set_nonblocking(false).unwrap();
                socket.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
                socket.set_write_timeout(Some(Duration::from_secs(10))).unwrap();
                socket.set_nodelay(true).unwrap();
                return socket;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "text peer connection deadline");
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("text peer accept: {error}"),
        }
    }
}

fn headers(socket: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        assert!(bytes.len() < 16_384);
        let mut byte = [0]; socket.read_exact(&mut byte).unwrap(); bytes.push(byte[0]);
    }
    String::from_utf8(bytes).unwrap()
}

fn receive(socket: &mut TcpStream) -> Option<Value> {
    let mut head = [0; 2];
    match socket.read_exact(&mut head) {
        Ok(()) => {}
        Err(error) if matches!(error.kind(), std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset) => return None,
        Err(error) => panic!("text peer receive: {error}"),
    }
    if head[0] & 0x0f == 8 { return None; }
    assert_eq!(head[0], 0x81); assert_ne!(head[1] & 0x80, 0);
    let count = match head[1] & 0x7f {
        126 => { let mut bytes = [0; 2]; socket.read_exact(&mut bytes).unwrap(); usize::from(u16::from_be_bytes(bytes)) }
        127 => panic!("unexpected large input command"),
        value => usize::from(value),
    };
    let mut mask = [0; 4]; socket.read_exact(&mut mask).unwrap();
    let mut bytes = vec![0; count]; socket.read_exact(&mut bytes).unwrap();
    for (index, byte) in bytes.iter_mut().enumerate() { *byte ^= mask[index % 4]; }
    Some(serde_json::from_slice(&bytes).unwrap())
}

fn send(socket: &mut TcpStream, value: &Value) {
    let bytes = serde_json::to_vec(value).unwrap();
    let mut frame = vec![0x81];
    if bytes.len() < 126 { frame.push(u8::try_from(bytes.len()).unwrap()); }
    else { frame.push(126); frame.extend_from_slice(&u16::try_from(bytes.len()).unwrap().to_be_bytes()); }
    frame.extend(bytes); socket.write_all(&frame).unwrap();
}

#[allow(clippy::too_many_lines)]
fn serve(socket: &mut TcpStream, scoped: bool, fault: Fault) -> Seen {
    let mut seen = Seen::default();
    let context = if scoped { 2 } else { 1 };
    for _ in 0..1000 {
        let Some(request) = receive(socket) else { return seen; };
        let method = request["method"].as_str().unwrap();
        let params = &request["params"];
        let mut failed = false;
        let result = match method {
            "Browser.setDownloadBehavior" => {
                assert_eq!(params["behavior"], "deny"); json!({})
            }
            "Target.getTargets" => json!({"targetInfos":[{"targetId":"page-1","type":"page","url":"about:blank"}]}),
            "Target.attachToTarget" => {
                assert_eq!(params["targetId"], "page-1"); json!({"sessionId":"page"})
            }
            "Target.setAutoAttach" => {
                assert_eq!(request["sessionId"], "page"); assert_eq!(params["waitForDebuggerOnStart"], false); json!({})
            }
            "Page.getFrameTree" => json!({"frameTree":{
                "frame":{"id":"main","loaderId":if seen.changed { "changed" } else { "initial" },"url":"about:blank"},
                "childFrames":[{"frame":{"id":"child","parentId":"main","loaderId":"child-loader","url":"about:srcdoc"}}]
            }}),
            "Page.bringToFront" => { seen.fronts += 1; json!({}) }
            "Page.createIsolatedWorld" => {
                assert_eq!(params["frameId"], if scoped { "child" } else { "main" });
                json!({"executionContextId":context})
            }
            "DOM.getDocument" => { assert!(!scoped); json!({"root":{"nodeId":1}}) }
            "DOM.querySelector" => {
                assert!(!scoped); assert_eq!(params["selector"], "#input");
                seen.queries += 1; json!({"nodeId":2})
            }
            "DOM.describeNode" => {
                if fault == Fault::LookupNavigation { seen.changed = true; }
                json!({"node":{"backendNodeId":7}})
            }
            "DOM.resolveNode" => {
                assert_eq!(params["backendNodeId"], 7); assert_eq!(params["executionContextId"], context);
                json!({"object":{"objectId":"original"}})
            }
            "Runtime.callFunctionOn" => {
                let function = params["functionDeclaration"].as_str().unwrap();
                if function.contains("Document.prototype.querySelector") {
                    assert!(scoped); assert_eq!(params["arguments"][0]["value"], "#input");
                    seen.queries += 1;
                    json!({"result":{"type":"object","objectId":"original"}})
                } else {
                    assert!(function.starts_with("function piBrowserElement("));
                    assert_eq!(params["objectId"], "original");
                    let value = match params["arguments"][0]["value"].as_str().unwrap() {
                        "edit" | "focus" => {
                            seen.edits += 1;
                            if fault == Fault::EditNavigation { seen.changed = true; }
                            json!(true)
                        }
                        "focused" => {
                            seen.focus_checks += 1;
                            if fault == Fault::MalformedFocus { json!("true") }
                            else { json!(fault != Fault::FocusLost) }
                        }
                        "verify_fill" => json!(fault != Fault::RevertedFill),
                        action => panic!("unexpected element action {action}"),
                    };
                    json!({"result":{"type":if value.is_boolean() {"boolean"} else {"string"},"value":value}})
                }
            }
            "Runtime.releaseObject" => { assert_eq!(params["objectId"], "original"); json!({}) }
            "DOM.scrollIntoViewIfNeeded" => { assert_eq!(params["backendNodeId"], 7); json!({}) }
            "Input.insertText" => {
                assert_eq!(params["text"], TEXT); seen.inserts += 1;
                if fault == Fault::InsertNavigation { seen.changed = true; }
                json!({})
            }
            "Input.dispatchKeyEvent" => {
                assert_eq!(request["sessionId"], "page");
                if params["type"] == "keyDown" {
                    seen.downs += 1; failed = fault == Fault::DownFailure;
                    if fault == Fault::DownNavigation { seen.changed = true; }
                } else {
                    assert_eq!(params["type"], "keyUp");
                    assert!(params.get("text").is_none()); assert!(params.get("commands").is_none());
                    seen.ups += 1; failed = fault == Fault::UpFailure;
                }
                json!({})
            }
            _ => panic!("unexpected text command {method}"),
        };
        let mut response = if failed {
            json!({"id":request["id"],"error":{"code":-32000,"message":TEXT}})
        } else { json!({"id":request["id"],"result":result}) };
        if request.get("sessionId").is_some() { response["sessionId"] = request["sessionId"].clone(); }
        send(socket, &response);
    }
    panic!("text peer command budget exceeded")
}

fn run(action: &str, scoped: bool, fault: Fault, empty: bool) -> (pi::error::Result<ToolOutput>, Seen) {
    eprintln!("{}", json!({"suite":"browser_text_input","action":action,"frame":scoped,"fault":format!("{fault:?}"),"empty":empty}));
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let peer = thread::spawn(move || {
        let mut discovery = accept(&listener);
        assert!(headers(&mut discovery).starts_with("GET /json/version "));
        let body = json!({"webSocketDebuggerUrl":format!("ws://{address}/devtools/browser/text")}).to_string();
        write!(discovery,"HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        drop(discovery);
        let mut socket = accept(&listener);
        let request = headers(&mut socket);
        let key = request.lines().find_map(|line| {
            let (name,value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("sec-websocket-key").then(|| value.trim())
        }).unwrap();
        let accept = asupersync::net::websocket::compute_accept_key(key);
        write!(socket,"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").unwrap();
        serve(&mut socket, scoped, fault)
    });
    let dir = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(dir.path()).with_mock(false).with_cdp_endpoint(format!("http://{address}"));
    let mut args = json!({"action":action,"tab":"page-1","selector":"#input","timeout_ms":8000});
    if action == "press" { args["key"] = json!("Ctrl+a"); }
    else { args["text"] = json!(if empty { "" } else { TEXT }); }
    if scoped { args["frame"] = json!("child"); }
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    let result = runtime.block_on(tool.execute("text-transport", args, None));
    (result, peer.join().unwrap())
}

#[test]
fn text_and_targeted_keys_share_one_focus_contract_on_main_and_child_documents() {
    for scoped in [false, true] { for action in ["type", "fill", "press"] {
        let (result, seen) = run(action, scoped, Fault::None, false);
        assert!(!result.unwrap().is_error);
        assert_eq!(seen.queries, 1); assert_eq!(seen.edits, 1);
        assert_eq!(seen.focus_checks, 1); assert_eq!(seen.fronts, 1);
        assert_eq!(seen.inserts, usize::from(action != "press"));
        assert_eq!((seen.downs, seen.ups), if action == "press" { (1,1) } else { (0,0) });
    }}
}

#[test]
fn focus_loss_and_malformed_focus_never_deliver_the_requested_text_or_key() {
    for scoped in [false,true] { for action in ["type","fill","press"] {
        for fault in [Fault::FocusLost,Fault::MalformedFocus] {
            let (result,seen) = run(action,scoped,fault,false);
            assert!(result.is_err()); assert_eq!((seen.inserts,seen.downs,seen.ups),(0,0,0));
            assert_eq!(seen.queries,1);
        }
    }}
}

#[test]
fn navigation_during_lookup_or_edit_does_not_authorize_a_replacement_document() {
    for scoped in [false,true] { for fault in [Fault::LookupNavigation,Fault::EditNavigation] {
        let (result,seen) = run("fill",scoped,fault,false);
        assert!(result.is_err()); assert_eq!((seen.inserts,seen.downs),(0,0));
        assert_eq!(seen.queries,1);
    }}
}

#[test]
fn failed_keydown_and_keyup_never_replay_and_do_not_reflect_transport_payloads() {
    for scoped in [false,true] { for fault in [Fault::DownFailure,Fault::UpFailure] {
        let (result,seen) = run("press",scoped,fault,false);
        let failure = result.unwrap_err().to_string();
        assert!(failure.contains("BROWSER_KEY_INTERRUPTED")); assert!(!failure.contains(TEXT));
        assert_eq!((seen.downs,seen.ups),(1,1));
    }}
}

#[test]
fn keyup_reaches_the_original_page_even_when_keydown_navigates_the_child() {
    let (result,seen) = run("press",true,Fault::DownNavigation,false);
    assert!(result.is_err()); assert_eq!((seen.downs,seen.ups),(1,1));
    let (result,seen) = run("press",false,Fault::DownNavigation,false);
    assert!(!result.unwrap().is_error()); assert_eq!((seen.downs,seen.ups),(1,1));
}

#[test]
fn rejected_or_navigated_text_is_not_reported_as_a_successful_fill_or_replayed() {
    for scoped in [false,true] { for fault in [Fault::InsertNavigation,Fault::RevertedFill] {
        let (result,seen) = run("fill",scoped,fault,false);
        assert!(result.is_err()); assert_eq!(seen.inserts,1); assert_eq!(seen.queries,1);
    }}
}

#[test]
fn empty_fills_use_the_same_guarded_paired_backspace_path() {
    for scoped in [false,true] {
        let (result,seen) = run("fill",scoped,Fault::None,true);
        assert!(!result.unwrap().is_error()); assert_eq!(seen.inserts,0);
        assert_eq!((seen.downs,seen.ups),(1,1)); assert_eq!(seen.focus_checks,1);
    }
}
