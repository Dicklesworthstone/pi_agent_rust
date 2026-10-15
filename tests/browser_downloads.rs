//! Real public-tool HTTP/WebSocket fixtures for download admission/publication.
//! The peer checks routing, filesystem bytes and cleanup; it is not Chromium.
#![forbid(unsafe_code)]
#![recursion_limit = "512"]

use pi::browser::BrowserTool;
use pi::error::Result;
use pi::tools::{Tool, ToolOutput};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

const PAYLOAD: &[u8] = b"file\0\xff!";

#[derive(Clone, Copy, Debug)]
enum Scene { Main, Local, Remote, Inside, Nested }

impl Scene {
    const fn frame(self) -> &'static str {
        match self {
            Self::Main => "main", Self::Local => "local", Self::Remote => "remote",
            Self::Inside => "inside", Self::Nested => "nested",
        }
    }
    const fn session(self) -> &'static str {
        match self {
            Self::Main | Self::Local => "page",
            Self::Remote | Self::Inside => "remote-session",
            Self::Nested => "nested-session",
        }
    }
    const fn context(self) -> u64 {
        match self { Self::Main => 1, Self::Local => 2, Self::Remote => 3, Self::Inside => 4, Self::Nested => 5 }
    }
    const fn point(self) -> (f64, f64) {
        match self {
            Self::Main | Self::Local => (50.0, 40.0),
            Self::Remote | Self::Inside => (150.0, 120.0),
            Self::Nested => (250.0, 200.0),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    None, Duplicate, ForeignFrame, BeforeClick, SecondDuringProgress, LateExtra,
    ChangedTerminal, ChangedIdentity, PoisonedRelease, Navigated, BlockedSource,
    BlockedAncestor, EnableFailure, ResetFailure, PressFailure, ByteMismatch,
    ZeroMismatch, Empty, MissingFile, ExistingDestination,
}

#[derive(Default, Debug)]
struct Observed {
    presses: usize,
    releases: usize,
    queries: usize,
    policies: Vec<String>,
    cancelled: Vec<String>,
    directory: Option<PathBuf>,
    poisoned: bool,
    reset: bool,
}

fn accept(listener: &TcpListener) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match listener.accept() {
            Ok((socket, _)) => {
                socket.set_nonblocking(false).unwrap();
                socket.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
                socket.set_write_timeout(Some(Duration::from_secs(15))).unwrap();
                socket.set_nodelay(true).unwrap();
                return socket;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "download peer received no connection");
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("download peer accept failed: {error}"),
        }
    }
}

fn headers(socket: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        assert!(bytes.len() < 16384);
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
        Err(error) if matches!(error.kind(), std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset) => return None,
        Err(error) => panic!("download peer receive: {error}"),
    }
    if head[0] & 0x0f == 8 { return None; }
    assert_eq!(head[0], 0x81);
    assert_ne!(head[1] & 0x80, 0);
    let size = match head[1] & 0x7f {
        126 => {
            let mut bytes = [0; 2]; socket.read_exact(&mut bytes).unwrap();
            usize::from(u16::from_be_bytes(bytes))
        }
        127 => panic!("unexpected oversized download command"),
        size => usize::from(size),
    };
    let mut mask = [0; 4]; socket.read_exact(&mut mask).unwrap();
    let mut bytes = vec![0; size]; socket.read_exact(&mut bytes).unwrap();
    for (index, byte) in bytes.iter_mut().enumerate() { *byte ^= mask[index % 4]; }
    Some(serde_json::from_slice(&bytes).unwrap())
}

fn write_frame(socket: &mut TcpStream, value: &Value) {
    let bytes = serde_json::to_vec(value).unwrap();
    let mut frame = vec![0x81];
    if bytes.len() < 126 { frame.push(u8::try_from(bytes.len()).unwrap()); }
    else {
        frame.push(126);
        frame.extend_from_slice(&u16::try_from(bytes.len()).unwrap().to_be_bytes());
    }
    frame.extend(bytes);
    socket.write_all(&frame).unwrap();
}

fn start(guid: &str, frame: &str) -> Value {
    json!({"method":"Browser.downloadWillBegin","params":{
        "guid":guid,"frameId":frame,"url":"https://example.com/export.bin","suggestedFilename":"export.bin"
    }})
}

fn progress(guid: &str, state: &str, received: u64) -> Value {
    json!({"method":"Browser.downloadProgress","params":{
        "guid":guid,"state":state,"receivedBytes":received,"totalBytes":received
    }})
}

fn attached(parent: &str, session: &str, target: &str) -> Value {
    json!({"method":"Target.attachedToTarget","sessionId":parent,"params":{
        "sessionId":session,"waitingForDebugger":false,"targetInfo":{"type":"iframe","targetId":target}
    }})
}

fn frame_tree(session: &str, fault: Fault, reset: bool) -> Value {
    let node = |id: &str, parent: Option<&str>, url: &str| {
        let mut frame = json!({"id":id,"loaderId":format!("loader-{id}"),"url":url,"name":id});
        if let Some(parent) = parent { frame["parentId"] = json!(parent); }
        json!({"frame":frame})
    };
    let tree = match session {
        "page" => {
            let url = if fault == Fault::BlockedSource { "https://blocked.test/PRIVATE-CANARY" }
                else { "https://example.com/root" };
            let mut root = node("main", None, url);
            if fault == Fault::Navigated && reset { root["frame"]["loaderId"] = json!("replacement"); }
            root["childFrames"] = json!([node("local", Some("main"), "about:srcdoc")]);
            root
        }
        "remote-session" => {
            let url = if fault == Fault::BlockedAncestor { "https://blocked.test/PRIVATE-CANARY" }
                else { "https://example.com/remote" };
            let mut root = node("remote", Some("main"), url);
            root["childFrames"] = json!([node("inside", Some("remote"), "about:srcdoc")]);
            root
        }
        "nested-session" => node("nested", Some("inside"), "https://example.com/nested"),
        _ => panic!("queried unrelated frame session"),
    };
    json!({"frameTree":tree})
}

#[allow(clippy::too_many_lines)]
fn serve(socket: &mut TcpStream, scene: Scene, fault: Fault, output: &std::path::Path) -> Observed {
    let mut observed = Observed::default();
    let mut configured = BTreeSet::new();
    for _ in 0..2000 {
        let Some(request) = read_frame(socket) else { return observed; };
        let method = request["method"].as_str().unwrap();
        let session = request["sessionId"].as_str().unwrap_or_default();
        let params = &request["params"];
        let mut failed = false;
        let mut deferred = false;
        let result = match method {
            "Browser.setDownloadBehavior" => {
                assert!(session.is_empty());
                assert_eq!(params["eventsEnabled"], true);
                let behavior = params["behavior"].as_str().unwrap();
                observed.policies.push(behavior.into());
                if behavior == "allowAndName" {
                    assert!(!output.exists(), "never enable capture for an existing destination");
                    observed.directory = Some(params["downloadPath"].as_str().unwrap().into());
                    failed = fault == Fault::EnableFailure;
                    if fault == Fault::BeforeClick { write_frame(socket, &start("job", scene.frame())); }
                } else {
                    assert_eq!(behavior, "deny");
                    if observed.directory.is_some() {
                        observed.reset = true;
                        assert!(!output.exists(), "publication preceded restoration of deny");
                        match fault {
                            Fault::LateExtra => write_frame(socket, &start("late-job", scene.frame())),
                            Fault::ChangedTerminal => write_frame(socket, &progress("job", "canceled", 7)),
                            Fault::ChangedIdentity => {
                                let mut changed = start("job", scene.frame());
                                changed["params"]["url"] = json!("https://blocked.test/PRIVATE-CANARY");
                                write_frame(socket, &changed);
                            }
                            _ => {}
                        }
                        failed = fault == Fault::ResetFailure;
                    }
                }
                json!({})
            }
            "Browser.cancelDownload" => {
                assert!(session.is_empty());
                assert!(observed.reset, "restore deny before cancelling transfers");
                let guid = params["guid"].as_str().unwrap();
                assert_ne!(guid, "existing", "must not cancel a pre-existing browser transfer");
                observed.cancelled.push(guid.to_owned());
                json!({})
            }
            "Target.getTargets" => {
                assert!(session.is_empty());
                write_frame(socket, &start("existing", "other-page-frame"));
                json!({"targetInfos":[
                    {"type":"page","targetId":"page-1","url":"https://example.com/root"},
                    {"type":"page","targetId":"other-page","url":"https://example.com/root"}
                ]})
            }
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
                        "page" => write_frame(socket, &attached("page", "remote-session", "remote")),
                        "remote-session" => write_frame(socket, &attached("remote-session", "nested-session", "nested")),
                        "nested-session" => {}
                        _ => panic!("configured unrelated renderer"),
                    }
                }
                json!({})
            }
            "Page.getFrameTree" => frame_tree(session, fault, observed.reset),
            "Page.bringToFront" => { assert_eq!(session, "page"); json!({}) }
            "Page.createIsolatedWorld" => {
                let (expected_session, context) = match params["frameId"].as_str().unwrap() {
                    "main" => ("page", 1), "local" => ("page", 2),
                    "remote" => ("remote-session", 3), "inside" => ("remote-session", 4),
                    "nested" => ("nested-session", 5), _ => panic!("unrelated world"),
                };
                assert_eq!(session, expected_session);
                json!({"executionContextId":context})
            }
            "DOM.getDocument" => { assert!(matches!(scene, Scene::Main)); json!({"root":{"nodeId":1}}) }
            "DOM.querySelector" => {
                assert!(matches!(scene, Scene::Main));
                assert_eq!(session, "page"); assert_eq!(params["selector"], "#export");
                observed.queries += 1; json!({"nodeId":2})
            }
            "DOM.describeNode" => {
                assert_eq!(session, scene.session());
                json!({"node":{"backendNodeId":123,"localName":"a","nodeName":"A"}})
            }
            "DOM.resolveNode" => {
                assert_eq!(session, scene.session()); assert_eq!(params["executionContextId"], scene.context());
                let object = match params["backendNodeId"].as_u64().unwrap() {
                    123 => "target", 124 => "hit", _ => panic!("resolved replacement node"),
                };
                json!({"object":{"objectId":object}})
            }
            "Runtime.callFunctionOn" => {
                assert_eq!(session, scene.session());
                let function = params["functionDeclaration"].as_str().unwrap();
                if function.contains("Document.prototype.querySelector") {
                    assert_eq!(params["executionContextId"], scene.context());
                    assert_eq!(params["arguments"][0]["value"], "#export");
                    observed.queries += 1;
                    json!({"result":{"type":"object","objectId":"target"}})
                } else if function.starts_with("function(hit)") {
                    assert_eq!(params["objectId"], "target");
                    assert_eq!(params["arguments"][0]["objectId"], "hit");
                    json!({"result":{"type":"boolean","value":true}})
                } else {
                    assert!(function.starts_with("function piBrowserElement("));
                    assert_eq!(params["objectId"], "target");
                    assert_eq!(params["arguments"][0]["value"], "point");
                    json!({"result":{"type":"object","value":{"x":50,"y":40}}})
                }
            }
            "Runtime.releaseObject" => {
                assert_eq!(session, scene.session());
                if fault == Fault::PoisonedRelease && !observed.poisoned {
                    observed.poisoned = true;
                    write_frame(socket, &start("job", scene.frame()));
                    let mut bad = progress("job", "completed", 7);
                    bad["params"]["totalBytes"] = json!("PRIVATE-CANARY");
                    write_frame(socket, &bad);
                }
                json!({})
            }
            "DOM.scrollIntoViewIfNeeded" => { assert_eq!(session, scene.session()); json!({}) }
            "DOM.getContentQuads" => {
                assert_eq!(session, scene.session()); assert_eq!(params["backendNodeId"], 123);
                json!({"quads":[[40,20,60,20,60,60,40,60]]})
            }
            "Page.getLayoutMetrics" => {
                assert_eq!(session, scene.session());
                json!({"cssLayoutViewport":{"clientWidth":800,"clientHeight":600,"pageX":0,"pageY":0}})
            }
            "Runtime.evaluate" => {
                assert!(params["expression"].as_str().unwrap().contains("innerWidth"));
                let context = match session { "page" => 1, "remote-session" => 3, "nested-session" => 5, _ => panic!("unrelated viewport") };
                assert_eq!(params["contextId"], context);
                json!({"result":{"type":"object","value":{
                    "width":800,"height":600,"visibleWidth":800,"visibleHeight":600,
                    "scrollX":0,"scrollY":0,"scale":1,"offsetX":0,"offsetY":0
                }}})
            }
            "DOM.getFrameOwner" => {
                let (expected, backend) = match params["frameId"].as_str().unwrap() {
                    "remote" => ("page", 501), "nested" => ("remote-session", 502), _ => panic!("unrelated owner"),
                };
                assert_eq!(session, expected); json!({"backendNodeId":backend})
            }
            "DOM.getBoxModel" => {
                assert_eq!(params["backendNodeId"], if session == "page" { 501 } else { 502 });
                json!({"model":{"content":[100,80,900,80,900,680,100,680]}})
            }
            "DOM.getNodeForLocation" => {
                assert_eq!(params["ignorePointerEventsNone"], false);
                let (x, y, backend, frame) = if session == scene.session() { (50,40,124,scene.frame()) }
                    else if session == "remote-session" { (150,120,502,"inside") }
                    else if matches!(scene, Scene::Nested) { (250,200,501,"main") }
                    else { (150,120,501,"main") };
                assert_eq!(params["x"], x); assert_eq!(params["y"], y);
                json!({"backendNodeId":backend,"frameId":frame})
            }
            "Input.dispatchMouseEvent" => {
                assert_eq!(session, "page");
                let (x, y) = scene.point();
                assert_eq!(params["x"].as_f64(), Some(x)); assert_eq!(params["y"].as_f64(), Some(y));
                assert_eq!(params["clickCount"], 1); assert_eq!(params["button"], "left");
                if params["type"] == "mousePressed" {
                    observed.presses += 1; assert_eq!(params["buttons"], 1);
                    assert_eq!(observed.presses, 1, "download click must never be replayed");
                    failed = fault == Fault::PressFailure;
                } else {
                    assert_eq!(params["type"], "mouseReleased"); assert_eq!(params["buttons"], 0);
                    observed.releases += 1; assert_eq!(observed.releases, 1);
                    let bytes = if fault == Fault::Empty { &[][..] } else { PAYLOAD };
                    if fault != Fault::MissingFile {
                        std::fs::write(observed.directory.as_ref().unwrap().join("job"), bytes).unwrap();
                    }
                    let frame = if fault == Fault::ForeignFrame { "unrelated-frame" } else { scene.frame() };
                    write_frame(socket, &start("job", frame));
                    if fault == Fault::Duplicate { write_frame(socket, &start("job", frame)); }
                    let size = match fault { Fault::ByteMismatch => 6, Fault::ZeroMismatch | Fault::Empty => 0, _ => 7 };
                    if fault == Fault::SecondDuringProgress {
                        write_frame(socket, &progress("job", "inProgress", 4)); deferred = true;
                    } else {
                        write_frame(socket, &progress("job", "completed", size));
                        if fault == Fault::Duplicate {
                            write_frame(socket, &start("job", frame));
                            write_frame(socket, &progress("job", "completed", size));
                        }
                    }
                }
                json!({})
            }
            _ => panic!("unexpected download command: {method}"),
        };
        let mut response = if failed {
            json!({"id":request["id"],"error":{"code":-32000,"message":"fixture command failed"}})
        } else { json!({"id":request["id"],"result":result}) };
        if !session.is_empty() { response["sessionId"] = json!(session); }
        write_frame(socket, &response);
        if deferred {
            write_frame(socket, &start("second-job", scene.frame()));
            write_frame(socket, &progress("job", "completed", 7));
        }
    }
    panic!("download fixture command budget exhausted")
}

fn peer(scene: Scene, fault: Fault, output: PathBuf) -> (String, thread::JoinHandle<Observed>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let mut discovery = accept(&listener);
        assert!(headers(&mut discovery).starts_with("GET /json/version "));
        let body = json!({"webSocketDebuggerUrl":format!("ws://{address}/devtools/browser/downloads")}).to_string();
        write!(discovery, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        drop(discovery);
        let mut socket = accept(&listener);
        let request = headers(&mut socket);
        let key = request.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("sec-websocket-key").then(|| value.trim())
        }).unwrap();
        let accept = asupersync::net::websocket::compute_accept_key(key);
        write!(socket, "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").unwrap();
        serve(&mut socket, scene, fault, &output)
    });
    (format!("http://{address}"), handle)
}

fn run(scene: Scene, fault: Fault) -> (Result<ToolOutput>, Observed, Option<Vec<u8>>) {
    eprintln!("{}", json!({"suite":"browser_downloads","scene":format!("{scene:?}"),"fault":format!("{fault:?}"),"phase":"start"}));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("result.bin");
    if fault == Fault::ExistingDestination { std::fs::write(&path, b"original").unwrap(); }
    let (endpoint, handle) = peer(scene, fault, path.clone());
    let tool = BrowserTool::new(dir.path()).with_mock(false).with_cdp_endpoint(endpoint)
        .with_domain_allowlist(Some(vec!["example.com".into()]));
    let mut args = json!({"action":"download","tab":"page-1","selector":"#export","output_path":"result.bin","timeout_ms":10000});
    if !matches!(scene, Scene::Main) { args["frame"] = json!(scene.frame()); }
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    let result = runtime.block_on(tool.execute("download-transport", args, None));
    drop(tool);
    let observed = handle.join().unwrap();
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => panic!("download output read: {error}"),
    };
    if let Some(directory) = &observed.directory { assert!(!directory.exists(), "private capture directory leaked"); }
    eprintln!("{}", json!({"suite":"browser_downloads","scene":format!("{scene:?}"),"fault":format!("{fault:?}"),"phase":"done","published":bytes.is_some(),"presses":observed.presses,"releases":observed.releases}));
    (result, observed, bytes)
}

#[test]
fn public_downloads_preserve_bytes_across_local_and_nested_renderer_documents() {
    for scene in [Scene::Main, Scene::Local, Scene::Remote, Scene::Inside, Scene::Nested] {
        let (result, observed, bytes) = run(scene, Fault::None);
        let result = result.unwrap(); assert!(!result.is_error);
        let details = result.details.unwrap();
        assert_eq!(details["frame_id"], scene.frame()); assert_eq!(details["size_bytes"], 7);
        assert_eq!(bytes.as_deref(), Some(PAYLOAD));
        assert_eq!((observed.presses, observed.releases, observed.queries), (1,1,1));
        assert_eq!(observed.policies, ["deny","allowAndName","deny"]);
        assert!(observed.cancelled.is_empty());
    }
}

#[test]
fn duplicate_notifications_and_genuinely_empty_transfers_remain_valid() {
    for fault in [Fault::Duplicate, Fault::Empty] {
        let (result, observed, bytes) = run(Scene::Nested, fault);
        assert!(!result.unwrap().is_error);
        assert_eq!(bytes.as_deref(), Some(if fault == Fault::Empty { &[][..] } else { PAYLOAD }));
        assert_eq!((observed.presses, observed.releases), (1,1));
    }
}

#[test]
fn foreign_late_rebound_and_poisoned_transfers_never_publish() {
    for fault in [Fault::ForeignFrame, Fault::SecondDuringProgress, Fault::LateExtra,
        Fault::ChangedTerminal, Fault::ChangedIdentity, Fault::PoisonedRelease, Fault::Navigated] {
        let (result, observed, bytes) = run(Scene::Nested, fault);
        let error = result.unwrap_err().to_string();
        assert!(!error.contains("PRIVATE-CANARY"), "{fault:?}: {error}");
        assert!(bytes.is_none(), "{fault:?}");
        assert_eq!((observed.presses, observed.releases), (1,1));
        assert!(observed.reset); assert!(!observed.cancelled.is_empty());
    }
}

#[test]
fn capture_enablement_failure_and_preclick_transfers_restore_deny_without_input() {
    for fault in [Fault::EnableFailure, Fault::BeforeClick] {
        let (result, observed, bytes) = run(Scene::Nested, fault);
        assert!(result.is_err()); assert!(bytes.is_none());
        assert_eq!((observed.presses, observed.releases), (0,0));
        assert_eq!(observed.policies, ["deny","allowAndName","deny"]);
        if fault == Fault::BeforeClick { assert_eq!(observed.cancelled, ["job"]); }
    }
}

#[test]
fn failed_input_cleanup_policy_restoration_and_file_integrity_are_not_success() {
    for fault in [Fault::PressFailure, Fault::ResetFailure, Fault::ByteMismatch, Fault::ZeroMismatch, Fault::MissingFile] {
        let (result, observed, bytes) = run(Scene::Nested, fault);
        assert!(result.is_err(), "{fault:?}"); assert!(bytes.is_none());
        assert_eq!((observed.presses, observed.releases), (1,1));
        assert_eq!(observed.policies, ["deny","allowAndName","deny"]);
        assert_eq!(observed.cancelled, ["job"]);
    }
}

#[test]
fn blocked_documents_and_existing_destinations_precede_capture_enablement() {
    for (scene, fault) in [(Scene::Main, Fault::BlockedSource), (Scene::Nested, Fault::BlockedAncestor),
        (Scene::Nested, Fault::ExistingDestination)] {
        let (result, observed, bytes) = run(scene, fault);
        assert!(result.is_err());
        assert_eq!((observed.presses, observed.releases), (0,0));
        assert_eq!(observed.policies, ["deny"]);
        if fault == Fault::ExistingDestination { assert_eq!(bytes.as_deref(), Some(&b"original"[..])); }
        else { assert!(bytes.is_none()); }
    }
}

#[test]
fn frame_download_preflight_and_mock_refusal_do_not_connect() {
    let dir = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(dir.path()).with_mock(false).with_cdp_endpoint("not-a-CDP-endpoint");
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    runtime.block_on(async {
        for args in [
            json!({"action":"download","frame":"child","selector":"#link"}),
            json!({"action":"download","tab":"app","frame":"child"}),
            json!({"action":"download","tab":"app","frame":null,"selector":"#link"}),
            json!({"action":"download","tab":"app","frame":"child","selector":"#link","files":[]}),
            json!({"action":"download","tab":"app","frame":"child","selector":"#link","output_path":"../escape"}),
        ] {
            let error = tool.execute("invalid-download", args, None).await.unwrap_err().to_string();
            assert!(!error.contains("CDP endpoint"), "{error}");
        }
        let mock = BrowserTool::new(dir.path()).with_mock(true);
        assert!(mock.execute("mock-download", json!({"action":"download","tab":"app","frame":"child","selector":"#link"}), None).await.is_err());
    });
    assert!(tool.parameters()["properties"]["frame"]["description"].as_str().unwrap().contains("download"));
}
