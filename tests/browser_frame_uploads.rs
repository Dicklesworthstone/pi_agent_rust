//! Public uploads over real HTTP/WebSocket transport. The peer checks routing
//! and admission boundaries, not Chromium behavior or a passing DSR gate.

#![forbid(unsafe_code)]
#![recursion_limit = "512"]

use pi::browser::BrowserTool;
use pi::error::Result;
use pi::tools::{Tool, ToolOutput};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    None,
    BlockedAncestor,
    NavigateBeforeRead,
    DirectoryBeforeSelection,
    SelectionReplaced,
    WrongSession,
}

#[derive(Default)]
struct Observed {
    lookups: usize,
    inspections: usize,
    selections: usize,
    clears: usize,
    paths: Vec<String>,
    selected: Vec<Value>,
}

fn accept(listener: &TcpListener) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match listener.accept() {
            Ok((socket, _)) => {
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(30)))
                    .unwrap();
                socket
                    .set_write_timeout(Some(Duration::from_secs(30)))
                    .unwrap();
                return socket;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "upload peer received no connection"
                );
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("upload peer accept: {error}"),
        }
    }
}

fn headers(socket: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        assert!(bytes.len() < 16_384);
        let mut byte = [0];
        socket.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
    }
    String::from_utf8(bytes).unwrap()
}

fn read_frame(socket: &mut TcpStream) -> Option<Value> {
    let mut head = [0; 2];
    match socket.read_exact(&mut head) {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
            ) =>
        {
            return None;
        }
        Err(error) => panic!("upload peer read: {error}"),
    }
    if head[0] & 0xf == 8 {
        return None;
    }
    assert_eq!(head[0], 0x81);
    assert_ne!(head[1] & 0x80, 0);
    let size = match head[1] & 0x7f {
        126 => {
            let mut size = [0; 2];
            socket.read_exact(&mut size).unwrap();
            usize::from(u16::from_be_bytes(size))
        }
        127 => panic!("upload peer received oversized command"),
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

fn tree(session: &str, fault: Fault, observed: &Observed) -> Value {
    let node = |id: &str, parent: Option<&str>, url: &str| {
        let mut frame = json!({"id":id,"loaderId":format!("loader-{id}"),"url":url});
        if let Some(parent) = parent {
            frame["parentId"] = json!(parent);
        }
        json!({"frame":frame})
    };
    let root = match session {
        "page" => {
            let mut root = node("main", None, "https://example.com/root");
            if fault == Fault::NavigateBeforeRead && observed.inspections > 0 {
                root["frame"]["loaderId"] = json!("replacement-loader");
            }
            let url = if fault == Fault::BlockedAncestor {
                "https://blocked.test/private"
            } else {
                "about:srcdoc"
            };
            root["childFrames"] = json!([node("child", Some("main"), url)]);
            root
        }
        "remote-session" => {
            let mut root = node("remote", Some("child"), "https://example.com/remote");
            root["childFrames"] = json!([node("remote-local", Some("remote"), "about:srcdoc")]);
            root
        }
        _ => panic!("unrelated upload renderer: {session}"),
    };
    json!({"frameTree":root})
}

#[allow(clippy::too_many_lines)]
fn serve(socket: &mut TcpStream, selected: &str, fault: Fault) -> Observed {
    let mut observed = Observed::default();
    let mut attached = false;
    let expected_session = if selected.starts_with("remote") {
        "remote-session"
    } else {
        "page"
    };
    for _ in 0..512 {
        let Some(request) = read_frame(socket) else {
            return observed;
        };
        let method = request["method"].as_str().unwrap();
        let session = request["sessionId"].as_str().unwrap_or_default();
        let params = &request["params"];
        let result = match method {
            "Browser.setDownloadBehavior" => {
                assert!(session.is_empty());
                assert_eq!(params["behavior"], "deny");
                json!({})
            }
            "Target.getTargets" => json!({"targetInfos":[{
                "targetId":"page-1","type":"page","url":"https://example.com/root"
            }]}),
            "Target.attachToTarget" => {
                assert_eq!(params["targetId"], "page-1");
                assert_eq!(params["flatten"], true);
                json!({"sessionId":"page"})
            }
            "Target.setAutoAttach" => {
                assert_eq!(params["autoAttach"], true);
                assert_eq!(params["waitForDebuggerOnStart"], false);
                if session == "page" && !attached {
                    attached = true;
                    write_frame(
                        socket,
                        &json!({"method":"Target.attachedToTarget","sessionId":"page",
                        "params":{"sessionId":"remote-session","waitingForDebugger":false,
                            "targetInfo":{"targetId":"remote","type":"iframe"}}}),
                    );
                }
                json!({})
            }
            "Page.getFrameTree" => tree(session, fault, &observed),
            "Page.createIsolatedWorld" => {
                assert_eq!(session, expected_session);
                assert_eq!(params["frameId"], selected);
                json!({"executionContextId":7})
            }
            "DOM.getDocument" => {
                assert_eq!(selected, "main");
                json!({"root":{"nodeId":1}})
            }
            "DOM.querySelector" => {
                assert_eq!(selected, "main");
                assert_eq!(params["selector"], "#file");
                observed.lookups += 1;
                json!({"nodeId":2})
            }
            "DOM.describeNode" => {
                assert_eq!(session, expected_session);
                json!({"node":{"backendNodeId":123}})
            }
            "DOM.resolveNode" => {
                assert_eq!(session, expected_session);
                assert_eq!(params["backendNodeId"], 123);
                assert_eq!(params["executionContextId"], 7);
                json!({"object":{"objectId":"file-handle"}})
            }
            "Runtime.callFunctionOn" => {
                assert_eq!(session, expected_session);
                if params["functionDeclaration"]
                    .as_str()
                    .unwrap()
                    .contains("Document.prototype.querySelector")
                {
                    assert_eq!(params["arguments"][0]["value"], "#file");
                    observed.lookups += 1;
                    json!({"result":{"type":"object","objectId":"file-handle"}})
                } else {
                    assert_eq!(params["objectId"], "file-handle");
                    match params["arguments"][0]["value"].as_str().unwrap() {
                        "file_input" => observed.inspections += 1,
                        "clear_files" => {
                            observed.clears += 1;
                            observed.selected.clear();
                        }
                        action => panic!("unexpected upload helper action: {action}"),
                    }
                    let files = if fault == Fault::SelectionReplaced && observed.selections > 0 {
                        Vec::new()
                    } else {
                        observed.selected.clone()
                    };
                    json!({"result":{"type":"object","value":{
                        "multiple":true,"directory":fault == Fault::DirectoryBeforeSelection && observed.inspections >= 2,
                        "files":files,
                    }}})
                }
            }
            "Runtime.releaseObject" => json!({}),
            "DOM.setFileInputFiles" => {
                assert_eq!(session, expected_session);
                assert_eq!(params["backendNodeId"], 123);
                assert_eq!(
                    observed.inspections, 2,
                    "recheck the original input before exposure"
                );
                observed.selections += 1;
                assert_eq!(observed.selections, 1, "file selection must not replay");
                for path in params["files"].as_array().unwrap() {
                    let path = path.as_str().unwrap();
                    let bytes = std::fs::read(path).unwrap();
                    let name = Path::new(path).file_name().unwrap().to_str().unwrap();
                    assert_eq!(
                        bytes,
                        if name == "first.txt" {
                            b"alpha".as_slice()
                        } else {
                            b"beta".as_slice()
                        }
                    );
                    observed.paths.push(path.to_owned());
                    observed
                        .selected
                        .push(json!({"name":name,"size":bytes.len()}));
                }
                if fault == Fault::WrongSession {
                    write_frame(
                        socket,
                        &json!({"id":request["id"],"sessionId":"wrong-session","result":{}}),
                    );
                    return observed;
                }
                json!({})
            }
            method => panic!("unexpected upload CDP command: {method}"),
        };
        let mut reply = json!({"id":request["id"],"result":result});
        if !session.is_empty() {
            reply["sessionId"] = json!(session);
        }
        write_frame(socket, &reply);
    }
    panic!("upload exhausted protocol fixture budget")
}

fn run(selected: &'static str, fault: Fault, files: Value) -> (Result<ToolOutput>, Observed) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let peer = thread::spawn(move || {
        let mut discovery = accept(&listener);
        assert!(headers(&mut discovery).starts_with("GET /json/version "));
        let body =
            json!({"webSocketDebuggerUrl":format!("ws://{address}/devtools/browser/uploads")})
                .to_string();
        write!(discovery, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        drop(discovery);
        let mut socket = accept(&listener);
        let request = headers(&mut socket);
        let key = request
            .lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case("sec-websocket-key")
                    .then(|| value.trim())
            })
            .unwrap();
        let key = asupersync::net::websocket::compute_accept_key(key);
        write!(socket, "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {key}\r\n\r\n").unwrap();
        serve(&mut socket, selected, fault)
    });
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("first.txt"), b"alpha").unwrap();
    std::fs::write(dir.path().join("second.txt"), b"beta").unwrap();
    let tool = BrowserTool::new(dir.path())
        .with_mock(false)
        .with_cdp_endpoint(format!("http://{address}"))
        .with_domain_allowlist(Some(vec!["example.com".to_owned()]));
    let mut args = json!({"action":"upload","tab":"page-1","selector":"#file","files":files,"timeout_ms":10000});
    if selected != "main" {
        args["frame"] = json!(selected);
    }
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    let result = runtime.block_on(tool.execute("frame-upload", args, None));
    let observed = peer.join().unwrap();
    // The operation socket is gone, but browser File objects may still retain
    // these copies. In particular an unacknowledged selection must retain them.
    for path in &observed.paths {
        assert!(
            Path::new(path).is_file(),
            "selected copies must outlive a call"
        );
    }
    assert_eq!(
        std::fs::read(dir.path().join("first.txt")).unwrap(),
        b"alpha"
    );
    (result, observed)
}

#[test]
fn clear_uses_the_selected_document_and_exact_node_in_every_renderer_shape() {
    for frame in ["main", "child", "remote", "remote-local"] {
        let (result, observed) = run(frame, Fault::None, json!([]));
        let details = result.unwrap().details.unwrap();
        assert_eq!(details["frame_id"], frame);
        assert_eq!(details["files"], json!([]));
        assert_eq!(observed.lookups, 1);
        assert_eq!(observed.inspections, 2);
        assert_eq!(observed.clears, 1);
        assert_eq!(observed.selections, 0);
    }
}

#[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
#[test]
fn staged_uploads_route_to_child_worlds_and_keep_copies_after_success_or_ack_failure() {
    for frame in ["main", "child", "remote", "remote-local"] {
        let (result, observed) = run(frame, Fault::None, json!(["first.txt", "second.txt"]));
        let details = result.unwrap().details.unwrap();
        assert_eq!(details["frame_id"], frame);
        assert_eq!(
            details["files"],
            json!([{"name":"first.txt","size":5},{"name":"second.txt","size":4}])
        );
        assert_eq!(details["staged_session_bytes"], 9);
        assert_eq!(observed.lookups, 1);
        assert_eq!(observed.selections, 1);
    }
    let (result, observed) = run("remote-local", Fault::WrongSession, json!(["first.txt"]));
    assert!(result.is_err());
    assert_eq!(observed.selections, 1);
}

#[test]
fn blocked_ancestry_and_navigation_refuse_before_opening_missing_local_files() {
    for fault in [Fault::BlockedAncestor, Fault::NavigateBeforeRead] {
        let (result, observed) = run("remote-local", fault, json!(["missing.txt"]));
        let error = result.unwrap_err().to_string();
        assert!(
            !error.contains("upload file is missing"),
            "admission must precede file reads: {error}"
        );
        assert!(
            error.contains("blocked") || error.contains("navigated"),
            "{error}"
        );
        assert_eq!(observed.selections, 0);
        assert!(observed.paths.is_empty());
    }
}

#[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
#[test]
fn input_revalidation_prevents_exposure_and_post_selection_changes_are_not_success() {
    let (result, observed) = run(
        "remote-local",
        Fault::DirectoryBeforeSelection,
        json!(["first.txt"]),
    );
    assert!(result.unwrap_err().to_string().contains("directory-upload"));
    assert_eq!(observed.inspections, 2);
    assert_eq!(observed.selections, 0);
    let (result, observed) = run(
        "remote-local",
        Fault::SelectionReplaced,
        json!(["first.txt"]),
    );
    assert!(result.unwrap_err().to_string().contains("did not retain"));
    assert_eq!(observed.selections, 1);
    assert_eq!(observed.lookups, 1);
}

#[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
#[test]
#[ignore = "requires sandbox-capable Chromium; select explicitly in the DSR browser lane"]
fn live_child_uploads_preserve_file_bytes_across_calls_and_never_select_the_parent() {
    use pi::browser::BrowserLaunchOptions;
    async fn call(tool: &BrowserTool, args: Value) -> Value {
        let result = tool.execute("live-frame-upload", args, None).await.unwrap();
        assert!(!result.is_error);
        result.details.unwrap()
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("other")).unwrap();
    std::fs::write(dir.path().join("same.txt"), "first 🦀").unwrap();
    std::fs::write(dir.path().join("other/same.txt"), "second 雪").unwrap();
    let tool = BrowserTool::new(dir.path())
        .with_mock(false)
        .with_launch_options(BrowserLaunchOptions {
            executable_path: std::env::var_os("PI_BROWSER_EXECUTABLE").map(Into::into),
            ..Default::default()
        })
        .with_domain_allowlist(Some(Vec::new()));
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        call(&tool, json!({"action":"open","tab":"uploads","url":"about:blank"})).await;
        call(&tool, json!({"action":"evaluate","tab":"uploads","script":r#"
            new Promise(resolve => {
                document.body.innerHTML = '<input type="file" id="file" multiple>';
                const child = document.createElement('iframe'); child.name = 'file-child';
                child.onload = () => resolve(true);
                child.srcdoc = '<input type="file" id="file" multiple style="display:none">';
                document.body.append(child);
            })
        "#})).await;
        let frames = call(&tool, json!({"action":"list_frames","tab":"uploads"})).await;
        let child = frames["frames"].as_array().unwrap().iter()
            .find(|frame| frame["name"] == "file-child").unwrap()["frame_id"].clone();
        let result = call(&tool, json!({"action":"upload","tab":"uploads","frame":child,
            "selector":"#file","files":["same.txt","other/same.txt"]})).await;
        assert_eq!(result["frame_id"], child);
        assert_eq!(result["file_count"], 2);
        assert_eq!(result["files"][0]["name"], "same.txt");
        assert_eq!(result["files"][1]["name"], "same.txt");
        std::fs::write(dir.path().join("same.txt"), "source changed").unwrap();
        let bytes = call(&tool, json!({"action":"evaluate","tab":"uploads","frame":child,
            "script":"Promise.all(Array.from(document.querySelector('#file').files, file => file.text()))"})).await;
        assert_eq!(bytes["result"], json!(["first 🦀", "second 雪"]));
        let parent = call(&tool, json!({"action":"evaluate","tab":"uploads",
            "script":"document.querySelector('#file').files.length"})).await;
        assert_eq!(parent["result"], 0);
        let cleared = call(&tool, json!({"action":"upload","tab":"uploads","frame":child,
            "selector":"#file","files":[]})).await;
        assert_eq!(cleared["files"], json!([]));
        call(&tool, json!({"action":"evaluate","tab":"uploads","frame":child,
            "script":"document.querySelector('#file').disabled = true"})).await;
        assert!(tool.execute("disabled-frame-file", json!({"action":"upload","tab":"uploads",
            "frame":child,"selector":"#file","files":["same.txt"]}), None).await.is_err());
        call(&tool, json!({"action":"stop"})).await;
    });
}
