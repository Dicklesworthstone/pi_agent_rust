//! Exercise the production HTTP/WebSocket path against a loopback CDP peer.
//! These are protocol fixtures, not claims of live Chromium coverage.
#![forbid(unsafe_code)]

use pi::browser::BrowserTool;
use pi::tools::Tool;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

type Script = Vec<(&'static str, Value)>;
const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAEklEQVR4nGP4z8DAAMIM/4EAAB/uBfsL2WiLAAAAAElFTkSuQmCC";

fn headers(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        assert!(bytes.len() < 16384);
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
    }
    String::from_utf8(bytes).unwrap()
}

fn accept(listener: &TcpListener) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match listener.accept() {
            Ok((stream, _)) => return stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "CDP fixture received no connection"
                );
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("CDP fixture accept failed: {error}"),
        }
    }
}

fn read_frame(stream: &mut TcpStream) -> Value {
    let mut head = [0; 2];
    stream.read_exact(&mut head).unwrap();
    assert_eq!(head[0], 0x81, "client sends one final text frame");
    assert_ne!(head[1] & 0x80, 0, "client frames must be masked");
    let size = match head[1] & 0x7f {
        126 => {
            let mut size = [0; 2];
            stream.read_exact(&mut size).unwrap();
            usize::from(u16::from_be_bytes(size))
        }
        127 => {
            let mut size = [0; 8];
            stream.read_exact(&mut size).unwrap();
            usize::try_from(u64::from_be_bytes(size)).unwrap()
        }
        size => usize::from(size),
    };
    assert!(size < 1024 * 1024);
    let mut mask = [0; 4];
    stream.read_exact(&mut mask).unwrap();
    let mut bytes = vec![0; size];
    stream.read_exact(&mut bytes).unwrap();
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte ^= mask[index % 4];
    }
    serde_json::from_slice(&bytes).unwrap()
}

fn write_frame(stream: &mut TcpStream, value: &Value) {
    let bytes = serde_json::to_vec(value).unwrap();
    stream.write_all(&[0x81]).unwrap();
    if bytes.len() < 126 {
        stream
            .write_all(&[u8::try_from(bytes.len()).unwrap()])
            .unwrap();
    } else {
        stream.write_all(&[126]).unwrap();
        stream
            .write_all(&u16::try_from(bytes.len()).unwrap().to_be_bytes())
            .unwrap();
    }
    stream.write_all(&bytes).unwrap();
}

fn check_parameters(request: &Value, response: &mut Value) {
    let fields = response.as_object_mut().unwrap();
    if let Some(expected) = fields.remove("_params") {
        for (name, value) in expected.as_object().unwrap() {
            assert_eq!(&request["params"][name], value, "parameter {name}");
        }
    }
    if let Some(expected) = fields.remove("_upload_contents") {
        assert_eq!(request["method"], "DOM.setFileInputFiles");
        let paths = request["params"]["files"].as_array().unwrap();
        let contents = expected.as_array().unwrap();
        assert_eq!(paths.len(), contents.len());
        for (path, content) in paths.iter().zip(contents) {
            assert_eq!(
                std::fs::read(path.as_str().unwrap()).unwrap(),
                content.as_str().unwrap().as_bytes()
            );
        }
        if let Some(destination) = fields.remove("_capture_paths_to") {
            std::fs::write(
                destination.as_str().unwrap(),
                serde_json::to_vec(paths).unwrap(),
            )
            .unwrap();
        }
    }
}

fn peer(script: Script) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let handle = thread::spawn(move || {
        let mut discovery = accept(&listener);
        assert!(headers(&mut discovery).starts_with("GET /json/version "));
        let body = json!({"webSocketDebuggerUrl": format!("ws://{addr}/devtools/browser/fixture")})
            .to_string();
        write!(discovery, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        drop(discovery);
        let mut socket = accept(&listener);
        let request = headers(&mut socket);
        assert!(request.starts_with("GET /devtools/browser/fixture "));
        let key = request
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("sec-websocket-key")
                    .then(|| value.trim())
            })
            .unwrap();
        let accept = asupersync::net::websocket::compute_accept_key(key);
        write!(socket, "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").unwrap();
        for (method, mut response) in script {
            let request = read_frame(&mut socket);
            assert_eq!(request["method"], method);
            if method.starts_with("Runtime.")
                || method.starts_with("Page.")
                || method.starts_with("DOM.")
            {
                assert_eq!(request["sessionId"], "session-1");
            }
            check_parameters(&request, &mut response);
            // An asynchronous event before the reply must not be mistaken for it.
            write_frame(
                &mut socket,
                &json!({"method": "Target.targetInfoChanged", "params": {}}),
            );
            if method == "Runtime.evaluate" {
                let expression = request["params"]["expression"].as_str().unwrap();
                assert!(matches!(
                    expression,
                    "({answer: 6 * 7})" | "({url: location.href, title: document.title})"
                ));
            }
            // Chromium may publish DOMContentLoaded before the Page.navigate
            // reply. The adapter must retain it and correlate the loader ID.
            if method == "Page.navigate" && response["result"]["loaderId"].is_string() {
                for loader in [json!("old-loader"), response["result"]["loaderId"].clone()] {
                    write_frame(
                        &mut socket,
                        &json!({
                            "sessionId": "session-1", "method": "Page.lifecycleEvent",
                            "params": {"frameId": response["result"]["frameId"],
                                       "loaderId": loader, "name": "DOMContentLoaded"}
                        }),
                    );
                }
            }
            response["id"] = request["id"].clone();
            write_frame(&mut socket, &response);
        }
    });
    (format!("http://{addr}"), handle)
}

fn attached_script() -> Script {
    vec![
        (
            "Browser.setDownloadBehavior",
            json!({"result": {}, "_params":{"behavior":"deny","eventsEnabled":true}}),
        ),
        (
            "Target.getTargets",
            json!({"result": {"targetInfos": [
                {"targetId": "page-1", "type": "page", "url": "https://example.com/", "title": "Real peer"}
            ]}}),
        ),
        (
            "Target.attachToTarget",
            json!({"result": {"sessionId": "session-1"}}),
        ),
    ]
}

#[test]
fn native_browser_evaluates_over_http_and_masked_websocket_not_canned_script_matching() {
    let mut script = attached_script();
    script.push((
        "Runtime.evaluate",
        json!({"result": {"result": {"type": "object", "value": {"answer": 42}}}}),
    ));
    let (endpoint, handle) = peer(script);
    let dir = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(dir.path())
        .with_mock(false)
        .with_cdp_endpoint(endpoint);
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    let result = runtime
        .block_on(tool.execute(
            "native-eval",
            json!({
                "action": "evaluate", "tab": "page-1", "script": "({answer: 6 * 7})"
            }),
            None,
        ))
        .unwrap();
    assert_eq!(result.details.as_ref().unwrap()["result"]["answer"], 42);
    assert_eq!(result.details.as_ref().unwrap()["backend"], "cdp");
    handle.join().unwrap();
}

#[test]
fn native_browser_surfaces_protocol_errors() {
    let mut script = attached_script();
    script.push((
        "Runtime.evaluate",
        json!({"error": {"code": -32000, "message": "Execution context destroyed"}}),
    ));
    let (endpoint, handle) = peer(script);
    let dir = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(dir.path())
        .with_mock(false)
        .with_cdp_endpoint(endpoint);
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    let error = runtime
        .block_on(tool.execute(
            "native-error",
            json!({
                "action": "evaluate", "tab": "page-1", "script": "({answer: 6 * 7})"
            }),
            None,
        ))
        .unwrap_err();
    assert!(error.to_string().contains("Execution context destroyed"));
    handle.join().unwrap();
}

#[test]
fn native_browser_never_falls_back_to_mock_when_endpoint_is_invalid() {
    let dir = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(dir.path())
        .with_mock(false)
        .with_cdp_endpoint("http://example.com:9222");
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    let error = runtime
        .block_on(tool.execute("no-fallback", json!({"action": "screenshot"}), None))
        .unwrap_err();
    assert!(error.to_string().contains("loopback"));
    assert!(!dir.path().join("screenshots").exists());
}

#[test]
fn native_screenshot_preserves_peer_pixels_and_returns_an_image_block() {
    use base64::Engine as _;
    use pi::model::ContentBlock;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(PNG)
        .unwrap();
    let mut script = attached_script();
    script.push(("Page.captureScreenshot", json!({"result": {"data": PNG}})));
    let (endpoint, handle) = peer(script);
    let dir = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(dir.path())
        .with_mock(false)
        .with_cdp_endpoint(endpoint);
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    let result = runtime
        .block_on(tool.execute(
            "native-screenshot",
            json!({
                "action": "screenshot", "tab": "page-1", "output_path": "capture.png"
            }),
            None,
        ))
        .unwrap();
    assert_eq!(
        std::fs::read(dir.path().join("capture.png")).unwrap(),
        bytes
    );
    assert!(result.content.iter().any(|block| matches!(block,
        ContentBlock::Image(image) if image.mime_type == "image/png" && image.data == PNG)));
    handle.join().unwrap();
}

#[test]
fn native_navigation_handles_load_events_that_arrive_before_the_command_reply() {
    let mut script = attached_script();
    script.extend([
        ("Page.enable", json!({"result": {}})),
        ("Page.setLifecycleEventsEnabled", json!({"result": {}})),
        (
            "Page.navigate",
            json!({"result": {"frameId": "frame-1", "loaderId": "new-loader"}}),
        ),
        (
            "Runtime.evaluate",
            json!({"result": {"result": {"type": "object", "value": {
                "url": "about:blank", "title": "Loaded target"
            }}}}),
        ),
    ]);
    let (endpoint, handle) = peer(script);
    let dir = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(dir.path())
        .with_mock(false)
        .with_cdp_endpoint(endpoint);
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    let result = runtime
        .block_on(tool.execute(
            "native-navigation",
            json!({
                "action": "goto", "tab": "page-1", "url": "about:blank", "timeout_ms": 3000
            }),
            None,
        ))
        .unwrap();
    let details = result.details.as_ref().unwrap();
    assert_eq!(details["loaded"], true);
    assert_eq!(details["title"], "Loaded target");
    assert_eq!(details["url"], "about:blank");
    assert!(
        details.get("status").is_none(),
        "do not manufacture an HTTP status"
    );
    handle.join().unwrap();
}

#[test]
fn native_navigation_refusals_are_not_reported_as_loaded_pages() {
    let mut script = attached_script();
    script.extend([
        ("Page.enable", json!({"result": {}})),
        ("Page.setLifecycleEventsEnabled", json!({"result": {}})),
        (
            "Page.navigate",
            json!({"result": {"errorText": "net::ERR_BLOCKED_BY_ADMINISTRATOR"}}),
        ),
    ]);
    let (endpoint, handle) = peer(script);
    let dir = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(dir.path())
        .with_mock(false)
        .with_cdp_endpoint(endpoint);
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    let error = runtime
        .block_on(tool.execute(
            "native-navigation-refused",
            json!({
                "action": "goto", "tab": "page-1", "url": "about:blank"
            }),
            None,
        ))
        .unwrap_err();
    assert!(error.to_string().contains("ERR_BLOCKED_BY_ADMINISTRATOR"));
    handle.join().unwrap();
}

#[test]
fn native_browser_does_not_invent_results_after_peer_disconnects() {
    let (endpoint, handle) = peer(attached_script());
    let dir = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(dir.path())
        .with_mock(false)
        .with_cdp_endpoint(endpoint);
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    let error = runtime
        .block_on(tool.execute(
            "native-disconnected",
            json!({
                "action": "evaluate", "tab": "page-1", "script": "({answer: 6 * 7})"
            }),
            None,
        ))
        .unwrap_err();
    assert!(error.to_string().contains("CDP"));
    handle.join().unwrap();
}

#[test]
fn native_page_exports_send_options_and_preserve_returned_bytes() {
    use base64::Engine as _;
    // This tests the wire/container contract, not PDF rendering correctness.
    let pdf = b"%PDF-1.7\nprotocol fixture only\n%%EOF\n";
    let pdf_data = base64::engine::general_purpose::STANDARD.encode(pdf);
    for is_pdf in [false, true] {
        let mut script = attached_script();
        let args = if is_pdf {
            script.push((
                "Page.printToPDF",
                json!({
                    "_params":{"transferMode":"ReturnAsBase64","preferCSSPageSize":true,
                               "landscape":true,"printBackground":false,"pageRanges":"1-3,5"},
                    "result":{"data":pdf_data}
                }),
            ));
            json!({"action":"print_pdf","tab":"page-1","output_path":"report.pdf",
                   "landscape":true,"print_background":false,"page_ranges":"1-3,5"})
        } else {
            script.push((
                "Page.getLayoutMetrics",
                json!({"result":{
                    "cssContentSize":{"x":0,"y":0,"width":2,"height":2}
                }}),
            ));
            script.push(("Page.captureScreenshot", json!({
                "_params":{"captureBeyondViewport":true,"clip":{"x":0.0,"y":0.0,"width":2.0,"height":2.0,"scale":1}},
                "result":{"data":PNG}
            })));
            json!({"action":"screenshot","tab":"page-1","output_path":"full.png","full_page":true})
        };
        let (endpoint, handle) = peer(script);
        let dir = tempfile::tempdir().unwrap();
        let tool = BrowserTool::new(dir.path())
            .with_mock(false)
            .with_cdp_endpoint(endpoint);
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let result = runtime
            .block_on(tool.execute("export", args, None))
            .unwrap();
        let expected = if is_pdf {
            pdf.to_vec()
        } else {
            base64::engine::general_purpose::STANDARD
                .decode(PNG)
                .unwrap()
        };
        let name = if is_pdf { "report.pdf" } else { "full.png" };
        assert_eq!(std::fs::read(dir.path().join(name)).unwrap(), expected);
        assert_eq!(
            result.details.as_ref().unwrap()["preview_included"],
            !is_pdf
        );
        handle.join().unwrap();
    }
}

#[test]
fn native_export_failure_never_clobbers_or_publishes_partial_output() {
    for existing in [false, true] {
        let mut script = attached_script();
        if !existing {
            script.push((
                "Page.captureScreenshot",
                json!({"result":{"data":"bm90IGEgcG5n"}}),
            ));
        }
        let (endpoint, handle) = peer(script);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("capture.png");
        if existing {
            std::fs::write(&path, b"original").unwrap();
        }
        let tool = BrowserTool::new(dir.path())
            .with_mock(false)
            .with_cdp_endpoint(endpoint);
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        assert!(
            runtime
                .block_on(tool.execute(
                    "bad-export",
                    json!({
                        "action":"screenshot","tab":"page-1","output_path":"capture.png"
                    }),
                    None
                ))
                .is_err()
        );
        if existing {
            assert_eq!(std::fs::read(path).unwrap(), b"original");
        } else {
            assert!(!path.exists());
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        }
        handle.join().unwrap();
    }
}

fn frame_reply(url: &str) -> (&'static str, Value) {
    (
        "Page.getFrameTree",
        json!({"result":{"frameTree":{"frame":{
            "id":"frame-1","loaderId":"loader-1","url":url
        }}}}),
    )
}

fn file_control_script(action: &str, files: &Value) -> Script {
    vec![
        frame_reply("https://example.com/"),
        (
            "Page.createIsolatedWorld",
            json!({"result":{"executionContextId":4}}),
        ),
        (
            "DOM.resolveNode",
            json!({"_params":{"backendNodeId":7,"executionContextId":4},"result":{"object":{"objectId":"file-object"}}}),
        ),
        (
            "Runtime.callFunctionOn",
            json!({
                "_params":{"objectId":"file-object","arguments":[{"value":action},{"value":{}}],"returnByValue":true},
                "result":{"result":{"type":"object","value":{"multiple":true,"directory":false,"files":files}}}
            }),
        ),
        ("Runtime.releaseObject", json!({"result":{}})),
    ]
}

fn upload_prefix() -> Script {
    let mut script = attached_script();
    script.push(frame_reply("https://example.com/"));
    script.extend([
        ("DOM.getDocument", json!({"result":{"root":{"nodeId":1}}})),
        (
            "DOM.querySelector",
            json!({"_params":{"nodeId":1,"selector":"#upload"},"result":{"nodeId":2}}),
        ),
        (
            "DOM.describeNode",
            json!({"result":{"node":{"backendNodeId":7}}}),
        ),
    ]);
    script.extend(file_control_script(
        "file_input",
        &json!([{ "name":"previous.txt", "size":3 }]),
    ));
    script.push(frame_reply("https://example.com/"));
    script
}

#[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
#[test]
fn native_upload_selects_private_bytes_and_retains_them_until_tool_drop() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("upload.txt"), b"real upload").unwrap();
    let capture = dir.path().join("captured-paths.json");
    let mut script = upload_prefix();
    script.push((
        "DOM.setFileInputFiles",
        json!({
            "_params":{"backendNodeId":7},"_upload_contents":["real upload"],
            "_capture_paths_to":capture.display().to_string(),"result":{}
        }),
    ));
    script.extend(file_control_script(
        "file_input",
        &json!([{ "name":"upload.txt", "size":11 }]),
    ));
    script.push(frame_reply("https://example.com/"));
    let (endpoint, handle) = peer(script);
    let tool = BrowserTool::new(dir.path())
        .with_mock(false)
        .with_cdp_endpoint(endpoint);
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    let result = runtime
        .block_on(tool.execute(
            "upload",
            json!({
                "action":"upload","tab":"page-1","selector":"#upload","files":["upload.txt"]
            }),
            None,
        ))
        .unwrap();
    assert_eq!(result.details.as_ref().unwrap()["file_count"], 1);
    handle.join().unwrap();
    let paths: Vec<std::path::PathBuf> =
        serde_json::from_slice(&std::fs::read(capture).unwrap()).unwrap();
    assert_eq!(paths.len(), 1);
    assert_ne!(paths[0], dir.path().join("upload.txt"));
    std::fs::write(dir.path().join("upload.txt"), b"later source change").unwrap();
    assert_eq!(std::fs::read(&paths[0]).unwrap(), b"real upload");
    drop(tool);
    assert!(
        !paths[0].exists(),
        "the session must own upload staging cleanup"
    );
}

#[test]
fn empty_upload_uses_native_clear_instead_of_noop_empty_cdp_file_list() {
    let mut script = upload_prefix();
    // No DOM.setFileInputFiles call is accepted here: Chromium 144 ignores [].
    script.extend(file_control_script("clear_files", &json!([])));
    script.push(frame_reply("https://example.com/"));
    let (endpoint, handle) = peer(script);
    let dir = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(dir.path())
        .with_mock(false)
        .with_cdp_endpoint(endpoint);
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    let result = runtime
        .block_on(tool.execute(
            "clear-upload",
            json!({
                "action":"upload","tab":"page-1","selector":"#upload","files":[]
            }),
            None,
        ))
        .unwrap();
    assert_eq!(result.details.as_ref().unwrap()["file_count"], 0);
    handle.join().unwrap();
}

#[test]
fn upload_rechecks_current_origin_before_reading_any_local_file() {
    let mut script = attached_script();
    // Target.getTargets passed the allowlist, but the document has since changed.
    script.push(frame_reply("https://blocked.example/"));
    let (endpoint, handle) = peer(script);
    let dir = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(dir.path())
        .with_mock(false)
        .with_cdp_endpoint(endpoint)
        .with_domain_allowlist(Some(vec!["example.com".into()]));
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    let error = runtime
        .block_on(tool.execute(
            "blocked-upload",
            json!({
                "action":"upload","tab":"page-1","selector":"#upload","files":["does-not-exist.txt"]
            }),
            None,
        ))
        .unwrap_err();
    assert!(error.to_string().contains("allowlist"), "{error}");
    handle.join().unwrap();
}
