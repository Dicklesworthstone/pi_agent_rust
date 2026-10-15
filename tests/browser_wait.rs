//! Public browser wait workflows. Real Chromium tests require explicit DSR selection.
#![forbid(unsafe_code)]
#![recursion_limit = "512"]

use pi::browser::{BrowserLaunchOptions, BrowserTool};
use pi::tools::Tool;
use serde_json::{Value, json};

fn log(case: &str, details: Value) {
    eprintln!("{}", json!({"suite":"browser-wait-public","case":case,"details":details}));
}

#[test]
fn malformed_waits_are_rejected_before_connection() {
    let dir = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(dir.path()).with_mock(false)
        .with_cdp_endpoint("not-a-CDP-endpoint");
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    runtime.block_on(async {
        for args in [
            json!({"action":"wait_for","selector":"#ready","state":"unknown"}),
            json!({"action":"wait_for","selector":"#ready","stable_ms":10001}),
            json!({"action":"wait_for","selector":"#ready","text_match":"exact"}),
            json!({"action":"wait_for","selector":"text/Ready","state":"detached"}),
            json!({"action":"wait_for","selector":"#ready","state":"hidden","text":"Ready"}),
            json!({"action":"wait_for","selector":"#ready","url":"https://ignored.invalid"}),
            json!({"action":"click","selector":"#ready","stable_ms":100}),
        ] {
            let error = tool.execute("invalid-wait", args, None).await.unwrap_err().to_string();
            assert!(error.contains("BROWSER_WAIT_USAGE"), "{error}");
            assert!(!error.contains("CDP endpoint"), "{error}");
        }
    });
    log("preflight", json!({"invalid_calls":7,"network_needed":false}));
}

#[test]
fn extended_waits_do_not_claim_simulated_readiness() {
    let dir = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(dir.path()).with_mock(true);
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    runtime.block_on(async {
        for args in [
            json!({"action":"wait_for","selector":"#ready","state":"hidden"}),
            json!({"action":"wait_for","selector":"#ready","stable_ms":100}),
            json!({"action":"wait_for","selector":"#ready","text":"Ready"}),
        ] {
            assert!(tool.execute("native-wait-required", args, None).await.is_err());
        }
    });
    log("native-only", json!({"simulated_results_refused":3}));
}

#[test]
fn wait_schema_exposes_state_text_and_stability() {
    let dir = tempfile::tempdir().unwrap();
    let schema = BrowserTool::new(dir.path()).parameters();
    assert_eq!(schema["properties"]["state"]["enum"], json!(["visible","hidden","attached","detached"]));
    assert_eq!(schema["properties"]["text_match"]["enum"], json!(["contains","exact"]));
    assert_eq!(schema["properties"]["stable_ms"]["maximum"], 10000);
    log("schema", json!({"typed_conditions":true}));
}

async fn call(tool: &BrowserTool, args: Value) -> Value {
    let output = tool.execute("live-browser-wait", args, None).await.unwrap();
    assert!(!output.is_error);
    output.details.unwrap()
}

#[test]
#[ignore = "requires sandbox-capable Chromium; select explicitly through DSR"]
fn live_waits_follow_replacement_visibility_text_and_frame_scope() {
    let dir = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(dir.path()).with_mock(false)
        .with_launch_options(BrowserLaunchOptions {
            executable_path: std::env::var_os("PI_BROWSER_EXECUTABLE").map(Into::into),
            ..Default::default()
        }).with_domain_allowlist(Some(Vec::new()));
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    runtime.block_on(async {
        call(&tool, json!({"action":"open","tab":"waits","url":"about:blank"})).await;
        call(&tool, json!({"action":"evaluate","tab":"waits","script":r#"
            (()=>{
                document.body.innerHTML='<div id="result">Pending</div><div id="spinner">Loading</div><input type="password" value="PRIVATE-CANARY" id="password">';
                window.nativeInputs=0;document.addEventListener('click',()=>nativeInputs++);
                setTimeout(()=>{result.outerHTML='<div id="result">Ready <strong>now</strong></div>';spinner.style.display='none'},150);
                return true;
            })()
        "#})).await;
        let result = call(&tool, json!({"action":"wait_for","tab":"waits","selector":"#result",
            "text":"Ready now","text_match":"exact","stable_ms":200,"timeout_ms":5000})).await;
        assert_eq!(result["matched"], true);
        assert_eq!(result["textMatched"], true);
        assert!(result["samples"].as_u64().unwrap() >= 2);
        let hidden = call(&tool, json!({"action":"wait_for","tab":"waits","selector":"#spinner","state":"hidden"})).await;
        assert_eq!(hidden["found"], true);
        assert_eq!(hidden["visible"], false);
        let attached = call(&tool, json!({"action":"wait_for","tab":"waits","selector":"#spinner","state":"attached"})).await;
        assert_eq!(attached["matched"], true);
        assert!(tool.execute("hidden-is-not-detached", json!({"action":"wait_for","tab":"waits",
            "selector":"#spinner","state":"detached","timeout_ms":700}), None).await.is_err());
        call(&tool, json!({"action":"evaluate","tab":"waits","script":"spinner.remove(); true"})).await;
        let detached = call(&tool, json!({"action":"wait_for","tab":"waits","selector":"#spinner","state":"detached"})).await;
        assert_eq!(detached["found"], false);
        assert!(tool.execute("private-value", json!({"action":"wait_for","tab":"waits",
            "selector":"#password","text":"PRIVATE-CANARY","timeout_ms":700}), None).await.is_err());
        let inputs = call(&tool, json!({"action":"evaluate","tab":"waits","script":"nativeInputs"})).await;
        assert_eq!(inputs["result"], 0);
        call(&tool, json!({"action":"evaluate","tab":"waits","script":r#"
            (()=>{const host=document.createElement('div');host.id='closed';host.textContent='UNRENDERED-CANARY';
                host.attachShadow({mode:'closed'}).innerHTML='<span>Internal</span>';document.body.append(host);return true})()
        "#})).await;
        assert!(tool.execute("unrendered-shadow-text", json!({"action":"wait_for","tab":"waits",
            "selector":"#closed","text":"UNRENDERED-CANARY","timeout_ms":700}), None).await.is_err());
        call(&tool, json!({"action":"evaluate","tab":"waits","script":r#"
            new Promise(resolve=>{
                const frame=document.createElement('iframe');frame.name='wait-child';
                frame.srcdoc='<div id="result">Child ready</div>';
                frame.onload=()=>resolve(true);document.body.append(frame);
            })
        "#})).await;
        let frames = call(&tool, json!({"action":"list_frames","tab":"waits"})).await;
        let frame = frames["frames"].as_array().unwrap().iter()
            .find(|frame| frame["name"] == "wait-child").unwrap()["frame_id"].clone();
        let child = call(&tool, json!({"action":"wait_for","tab":"waits","frame":frame,
            "selector":"#result","text":"Child ready","text_match":"exact","timeout_ms":5000})).await;
        assert_eq!(child["textMatched"], true);
        assert!(tool.execute("frame-never-parent", json!({"action":"wait_for","tab":"waits","frame":frame,
            "selector":"#result","text":"Ready now","timeout_ms":700}), None).await.is_err());
        call(&tool, json!({"action":"stop"})).await;
        log("live-workflow", json!({"replacement":true,"disappearance":true,"text":true,"frame_isolation":true}));
    });
}

#[test]
fn invalid_postconditions_are_rejected_before_any_trigger_or_connection() {
    let dir = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(dir.path()).with_mock(false)
        .with_cdp_endpoint("not-a-CDP-endpoint");
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    runtime.block_on(async {
        for after in [Value::Null, json!({"selector":"#r","timeout_ms":60000}),
            json!({"selector":"text/Done","state":"detached"}), json!({"selector":"#r","text_match":"exact"})] {
            let error = tool.execute("invalid-after", json!({"action":"click","selector":"#trigger","wait_after":after}), None)
                .await.unwrap_err().to_string();
            assert!(error.contains("BROWSER_WAIT"), "{error}");
            assert!(!error.contains("CDP endpoint"), "{error}");
        }
        let fixture = BrowserTool::new(dir.path()).with_mock(true);
        assert!(fixture.execute("after-native-only", json!({"action":"click","selector":"#trigger",
            "wait_after":{"selector":"#result"}}), None).await.is_err());
    });
    log("postcondition-preflight", json!({"invalid_before_input":true,"native_required":true}));
}

#[test]
fn navigation_preflight_requires_explicit_bounded_main_tab_input() {
    let dir = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(dir.path()).with_mock(false).with_cdp_endpoint("not-a-CDP-endpoint");
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    let base = json!({"action":"click","tab":"app","selector":"#continue",
        "wait_after":{"navigation":{"url":"https://app.example/done"}}});
    runtime.block_on(async {
        for (key, value) in [("frame",json!("child")), ("tab",Value::Null),
            ("button",json!("right")), ("click_count",json!(2)), ("action",json!("type"))] {
            let mut args = base.clone(); args[key] = value;
            let error = tool.execute("navigation-preflight", args, None).await.unwrap_err().to_string();
            assert!(error.contains("BROWSER_NAVIGATION_USAGE"), "{error}");
            assert!(!error.contains("CDP endpoint"));
        }
        let fixture = BrowserTool::new(dir.path()).with_mock(true);
        assert!(fixture.execute("navigation-native-only", base, None).await.is_err());
    });
    log("navigation-preflight", json!({"native_required":true,"invalid_before_connection":5}));
}

#[test]
#[ignore = "requires sandbox-capable Chromium; select explicitly through DSR"]
fn live_postconditions_wait_after_one_input_without_replaying_it() {
    let dir = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(dir.path()).with_mock(false)
        .with_launch_options(BrowserLaunchOptions {
            executable_path: std::env::var_os("PI_BROWSER_EXECUTABLE").map(Into::into),
            ..Default::default()
        }).with_domain_allowlist(Some(Vec::new()));
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    runtime.block_on(async {
        call(&tool, json!({"action":"open","tab":"after","url":"about:blank"})).await;
        call(&tool, json!({"action":"evaluate","tab":"after","script":r#"
            (()=>{
                document.body.innerHTML='<button id="trigger">Save</button><div id="result">Pending</div><input id="name">';
                window.clicks=0;window.inputEvents=0;
                trigger.onclick=()=>{clicks++;setTimeout(()=>result.textContent='Saved',150)};
                document.querySelector('#name').addEventListener('input',()=>{inputEvents++;setTimeout(()=>result.textContent='Edited',150)});
                return true;
            })()
        "#})).await;
        let result = call(&tool, json!({"action":"click","tab":"after","selector":"#trigger",
            "wait_after":{"selector":"#result","text":"Saved","text_match":"exact","stable_ms":200},"timeout_ms":5000})).await;
        assert_eq!(result["wait_after"]["satisfied"], true);
        assert_eq!(result["dispatch_acknowledged"], true);
        assert_eq!(result["delivery_verified"], false);
        assert_eq!(result["wait_after"]["action_replayed"], false);
        let edited = call(&tool, json!({"action":"fill","tab":"after","selector":"#name","text":"Literal input",
            "wait_after":{"selector":"#result","text":"Edited","text_match":"exact"},"timeout_ms":5000})).await;
        assert_eq!(edited["wait_after"]["satisfied"], true);
        let counters = call(&tool, json!({"action":"evaluate","tab":"after","script":"[clicks,inputEvents]"})).await;
        assert_eq!(counters["result"], json!([1,1]));
        assert!(tool.execute("after-timeout", json!({"action":"click","tab":"after","selector":"#trigger",
            "wait_after":{"selector":"#never"},"timeout_ms":1000}), None).await.is_err());
        let count = call(&tool, json!({"action":"evaluate","tab":"after","script":"clicks"})).await;
        assert_eq!(count["result"], 2, "postcondition timeout must not repeat the input");
        call(&tool, json!({"action":"evaluate","tab":"after","script":
            "trigger.onclick=()=>{clicks++;document.body.insertAdjacentHTML('beforeend','<button>Duplicate</button><button>Duplicate</button>')};true"})).await;
        let failed = tool.execute("ambiguous-after", json!({"action":"click","tab":"after","selector":"#trigger",
            "wait_after":{"selector":"text/Duplicate"},"timeout_ms":5000}), None).await.unwrap();
        assert!(failed.is_error);
        let details = failed.details.unwrap();
        assert_eq!(details["dispatch_acknowledged"], true);
        assert_eq!(details["wait_after"]["error_kind"], "ambiguous_selector");
        let count = call(&tool, json!({"action":"evaluate","tab":"after","script":"clicks"})).await;
        assert_eq!(count["result"], 3);
        call(&tool, json!({"action":"stop"})).await;
        log("live-postconditions", json!({"one_trigger":true,"fill":true,"timeout_no_replay":true,"error_retains_action":true}));
    });
}

// These tests drive the native HTTP/WebSocket dispatcher against an independent
// loopback peer. The peer is protocol evidence, not a simulated-Chromium claim.
mod wire {
    use super::*;
    use std::io::{self, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;
    use std::time::{Duration, Instant};

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mode {
        Ready, Missing, Pending, Malformed, Navigate, Cancelled,
        NavDocument, NavHistory, NavReload, NavUnchanged, NavOutside, NavError, NavMismatch, NavCancelled, NavDrift,
    }

    impl Mode {
        fn navigation(self) -> bool {
            matches!(self, Self::NavDocument | Self::NavHistory | Self::NavReload | Self::NavUnchanged |
                Self::NavOutside | Self::NavError | Self::NavMismatch | Self::NavCancelled | Self::NavDrift)
        }

        fn frame(self, completed_input: bool) -> Value {
            let changed = completed_input && self != Self::NavUnchanged;
            let origin = if changed && self == Self::NavOutside { "https://other.example" } else { "https://app.example" };
            let path = if !changed || self == Self::NavReload { "/start" } else { "/done" };
            let loader = if changed && self != Self::NavHistory { "new" } else { "old" };
            let mut frame = json!({"id":"main","loaderId":loader,"url":format!("{origin}{path}"),"securityOrigin":origin});
            if self == Self::NavError && changed { frame["unreachableUrl"] = json!("https://app.example/done"); }
            frame
        }
    }

    #[derive(Default)]
    struct Seen {
        methods: Vec<String>,
        inputs: Vec<String>,
        observations: usize,
        readiness_reads: usize,
    }

    fn accept(listener: &TcpListener) -> TcpStream {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_read_timeout(Some(Duration::from_secs(4))).unwrap();
                    stream.set_write_timeout(Some(Duration::from_secs(4))).unwrap();
                    return stream;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "native wait never connected");
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("wait fixture accept: {error}"),
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
        let mut head = [0; 2];
        match stream.read_exact(&mut head) {
            Ok(()) => {}
            Err(error) if matches!(error.kind(), io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset) => return None,
            Err(error) => panic!("wait fixture frame: {error}"),
        }
        if head[0] & 0x0f == 8 { return None; }
        assert_eq!(head[0], 0x81);
        assert_ne!(head[1] & 0x80, 0);
        let size = match head[1] & 0x7f {
            126 => {
                let mut bytes = [0; 2]; stream.read_exact(&mut bytes).unwrap();
                usize::from(u16::from_be_bytes(bytes))
            }
            127 => {
                let mut bytes = [0; 8]; stream.read_exact(&mut bytes).unwrap();
                usize::try_from(u64::from_be_bytes(bytes)).unwrap()
            }
            size => usize::from(size),
        };
        assert!(size < 256 * 1024);
        let mut mask = [0; 4]; stream.read_exact(&mut mask).unwrap();
        let mut body = vec![0; size]; stream.read_exact(&mut body).unwrap();
        for (index, byte) in body.iter_mut().enumerate() { *byte ^= mask[index % 4]; }
        Some(serde_json::from_slice(&body).unwrap())
    }

    fn reply(stream: &mut TcpStream, value: &Value) -> bool {
        let body = serde_json::to_vec(value).unwrap();
        let mut frame = vec![0x81];
        if body.len() < 126 {
            frame.push(u8::try_from(body.len()).unwrap());
        } else {
            frame.push(126);
            frame.extend_from_slice(&u16::try_from(body.len()).unwrap().to_be_bytes());
        }
        frame.extend_from_slice(&body);
        stream.write_all(&frame).is_ok()
    }

    fn peer(mode: Mode, cancel: Option<pi::agent_cx::AgentCx>) -> (String, thread::JoinHandle<Seen>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let worker = thread::spawn(move || {
            let mut http = accept(&listener);
            assert!(headers(&mut http).starts_with("GET /json/version "));
            let body = json!({"webSocketDebuggerUrl":format!("ws://{address}/devtools/browser/wait-fixture")}).to_string();
            write!(http, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
            drop(http);
            let mut socket = accept(&listener);
            let request_headers = headers(&mut socket);
            assert!(request_headers.starts_with("GET /devtools/browser/wait-fixture "));
            let key = request_headers.lines().find_map(|line| {
                let (key,value) = line.split_once(':')?;
                key.eq_ignore_ascii_case("sec-websocket-key").then(|| value.trim())
            }).unwrap();
            let key = asupersync::net::websocket::compute_accept_key(key);
            write!(socket,"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {key}\r\n\r\n").unwrap();
            let mut seen = Seen::default();
            while let Some(request) = request(&mut socket) {
                assert!(seen.methods.len() < 2048, "unbounded wait protocol loop");
                let method = request["method"].as_str().unwrap();
                let params = &request["params"];
                seen.methods.push(method.into());
                let result = match method {
                    "Browser.setDownloadBehavior" | "Page.bringToFront" | "Runtime.releaseObject" => json!({}),
                    "Target.getTargets" => json!({"targetInfos":[{"targetId":"page","type":"page",
                        "url":if mode.navigation() {"https://app.example/start"} else {"about:blank"},"title":"Wait fixture"}]}),
                    "Target.attachToTarget" => json!({"sessionId":"session"}),
                    "Page.getFrameTree" if mode.navigation() => {
                        let mut frame = mode.frame(seen.inputs.len() >= 2);
                        if mode == Mode::NavDrift && seen.observations > 0 { frame["url"] = json!("https://app.example/elsewhere"); }
                        json!({"frameTree":{"frame":frame}})
                    }
                    "Page.getFrameTree" => json!({"frameTree":{"frame":{
                        "id":"main","loaderId":if mode == Mode::Navigate && !seen.inputs.is_empty() {"new"} else {"old"},"url":"about:blank"
                    }}}),
                    "DOM.getDocument" => json!({"root":{"nodeId":1}}),
                    "DOM.querySelector" => {
                        let id = match params["selector"].as_str() {
                            Some("#trigger") => 10,
                            Some("#ready") => if mode == Mode::Missing { 0 } else { 20 },
                            _ => panic!("unexpected fixture selector"),
                        };
                        json!({"nodeId":id})
                    }
                    "DOM.describeNode" => json!({"node":{"backendNodeId":params["nodeId"],"localName":"div","nodeName":"DIV"}}),
                    "Page.createIsolatedWorld" => json!({"executionContextId":7}),
                    "DOM.resolveNode" => json!({"object":{"objectId":format!("node-{}",params["backendNodeId"])}}),
                    "DOM.scrollIntoViewIfNeeded" => json!({}),
                    "Runtime.evaluate" if mode.navigation() => {
                        assert!(seen.inputs.len() >= 2, "destination probe must follow input");
                        assert_eq!(params["contextId"], 7);
                        assert!(params["expression"].as_str().unwrap().contains("domContentLoadedEventStart"));
                        seen.readiness_reads += 1;
                        if mode == Mode::NavCancelled {
                            cancel.as_ref().unwrap().set_cancel_requested(true);
                            continue;
                        }
                        let address = if mode == Mode::NavMismatch {json!("https://app.example/elsewhere")}
                            else {mode.frame(true)["url"].clone()};
                        json!({"result":{"type":"object","value":{"url":address,
                            "state":if seen.readiness_reads < 3 {"interactive"} else {"complete"},
                            "domContentLoaded":seen.readiness_reads >= 2,"load":seen.readiness_reads >= 3}}})
                    }
                    "Runtime.callFunctionOn" => {
                        let options = &params["arguments"][0]["value"];
                        if options == "point" {
                            json!({"result":{"type":"object","value":{"x":20,"y":30}}})
                        } else {
                            assert!(options.is_object());
                            assert!(options["exact"].is_boolean());
                            if !options["text"].is_null() { assert_eq!(options["text"], "Ready now"); }
                            seen.observations += 1;
                            if mode == Mode::Cancelled {
                                cancel.as_ref().unwrap().set_cancel_requested(true);
                                // Withhold this actual native observation. The
                                // owner's cancellation must retire the socket,
                                // not wait for a fake successful reply.
                                continue;
                            }
                            let visible = mode != Mode::Pending && seen.observations >= 3;
                            let value = if mode == Mode::Malformed {
                                json!({"attached":false,"visible":true,"textMatches":null})
                            } else {
                                json!({"attached":true,"visible":visible,
                                    "textMatches":if options["text"].is_null() {Value::Null} else {json!(visible)}})
                            };
                            json!({"result":{"type":"object","value":value}})
                        }
                    }
                    "Input.dispatchMouseEvent" | "Input.dispatchKeyEvent" => {
                        seen.inputs.push(params["type"].as_str().unwrap().into());
                        json!({})
                    }
                    _ => panic!("unhandled native wait method {method}"),
                };
                let response = json!({"id":request["id"],"sessionId":request["sessionId"],"result":result});
                if !reply(&mut socket, &response) { break; }
            }
            seen
        });
        (format!("http://{address}"), worker)
    }

    fn run(mode: Mode, args: Value) -> (pi::error::Result<pi::tools::ToolOutput>, Seen) {
        let owner = matches!(mode, Mode::Cancelled | Mode::NavCancelled).then(pi::agent_cx::AgentCx::for_request);
        let (endpoint, worker) = peer(mode, owner.clone());
        let dir = tempfile::tempdir().unwrap();
        let tool = BrowserTool::new(dir.path()).with_mock(false).with_cdp_endpoint(endpoint);
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
        let result = if let Some(owner) = owner {
            runtime.block_on(owner.with_current(tool.execute("wire-wait",args,None)))
        } else {
            runtime.block_on(tool.execute("wire-wait",args,None))
        };
        if matches!(mode, Mode::Cancelled | Mode::NavCancelled) {
            // A fresh caller can reacquire both existing ownership lanes and
            // release its retained namespace without another network request.
            let released = runtime.block_on(tool.execute("after-cancel",json!({"action":"release"}),None)).unwrap();
            assert_eq!(released.details.unwrap()["released"],true);
        }
        drop(tool);
        let seen = worker.join().unwrap();
        log("wire-traffic",json!({"methods":seen.methods,"inputs":seen.inputs,"observations":seen.observations,"readiness_reads":seen.readiness_reads}));
        (result, seen)
    }

    #[test]
    fn native_wait_polls_real_transport_until_matching_text_is_stable() {
        let (result, seen) = run(Mode::Ready,json!({"action":"wait_for","tab":"page","selector":"#ready",
            "text":"Ready now","text_match":"exact","stable_ms":150,"timeout_ms":5000}));
        let result = result.unwrap();
        assert!(!result.is_error);
        assert_eq!(result.details.unwrap()["textMatched"],true);
        assert!(seen.observations >= 4);
        assert!(seen.inputs.is_empty());
    }

    #[test]
    fn native_absence_succeeds_but_protocol_failure_never_proves_hidden() {
        let (result, seen) = run(Mode::Missing,json!({"action":"wait_for","tab":"page","selector":"#ready","state":"detached"}));
        let result = result.unwrap();
        assert!(!result.is_error);
        assert_eq!(result.details.unwrap()["found"],false);
        assert_eq!(seen.observations,0);
        let (result, seen) = run(Mode::Malformed,json!({"action":"wait_for","tab":"page","selector":"#ready","state":"hidden"}));
        assert!(result.unwrap_err().to_string().contains("BROWSER_WAIT_PROTOCOL"));
        assert!(seen.inputs.is_empty());
    }

    #[test]
    fn native_postcondition_polls_after_exactly_one_pointer_pair() {
        let (result, seen) = run(Mode::Ready,json!({"action":"click","tab":"page","selector":"#trigger",
            "wait_after":{"selector":"#ready","text":"Ready now"},"timeout_ms":5000}));
        let result = result.unwrap();
        assert!(!result.is_error);
        assert_eq!(result.details.unwrap()["wait_after"]["satisfied"],true);
        assert_eq!(seen.inputs,vec!["mousePressed","mouseReleased"]);
        assert_eq!(seen.observations,3);
    }

    #[test]
    fn hover_postcondition_uses_one_button_free_move_and_the_existing_wait_lane() {
        let (result, seen) = run(Mode::Ready,json!({"action":"hover","tab":"page","selector":"#trigger",
            "wait_after":{"selector":"#ready","text":"Ready now"},"timeout_ms":5000}));
        let result = result.unwrap();
        assert!(!result.is_error);
        let details = result.details.unwrap();
        assert_eq!(details["action"],"hover");
        assert_eq!(details["wait_after"]["satisfied"],true);
        assert_eq!(seen.inputs,vec!["mouseMoved"]);
        assert_eq!(seen.observations,3);
    }

    #[test]
    fn hover_navigation_failure_retains_input_and_stops_the_native_batch() {
        let (result, seen) = run(Mode::Navigate,json!({"action":"batch","steps":[
            {"call":{"action":"hover","tab":"page","selector":"#trigger","wait_after":{"selector":"#ready"}}},
            {"call":{"action":"click","tab":"page","selector":"#trigger"}}
        ],"timeout_ms":5000}));
        let result = result.unwrap();
        assert!(result.is_error);
        let details = result.details.unwrap();
        assert_eq!(details["attempted_steps"],1);
        assert_eq!(details["steps"][0]["details"]["dispatch_acknowledged"],true);
        assert_eq!(details["steps"][0]["details"]["wait_after"]["error_kind"],"document_changed");
        assert_eq!(seen.inputs,vec!["mouseMoved"]);
        assert_eq!(seen.observations,0);
    }

    #[test]
    fn cancelled_hover_postcondition_releases_admission_without_repeating_input() {
        let (result, seen) = run(Mode::Cancelled,json!({"action":"hover","tab":"page","selector":"#trigger",
            "wait_after":{"selector":"#ready"},"timeout_ms":5000}));
        assert!(result.is_err());
        assert_eq!(seen.inputs,vec!["mouseMoved"]);
    }

    #[test]
    fn native_timeout_and_navigation_do_not_replay_completed_input() {
        let (result, seen) = run(Mode::Pending,json!({"action":"click","tab":"page","selector":"#trigger",
            "wait_after":{"selector":"#ready"},"timeout_ms":2000}));
        assert!(result.is_err());
        assert_eq!(seen.inputs,vec!["mousePressed","mouseReleased"]);
        let (result, seen) = run(Mode::Navigate,json!({"action":"click","tab":"page","selector":"#trigger",
            "wait_after":{"selector":"#ready"},"timeout_ms":5000}));
        let result = result.unwrap();
        assert!(result.is_error);
        let details = result.details.unwrap();
        assert_eq!(details["dispatch_acknowledged"],true);
        assert_eq!(details["wait_after"]["error_kind"],"document_changed");
        assert_eq!(seen.inputs,vec!["mousePressed","mouseReleased"]);
        assert_eq!(seen.observations,0,"new document must never be probed");
    }

    #[test]
    fn cancellation_during_a_postcondition_retires_the_connection_and_releases_admission() {
        let (result, seen) = run(Mode::Cancelled,json!({"action":"click","tab":"page","selector":"#trigger",
            "wait_after":{"selector":"#ready"},"timeout_ms":5000}));
        assert!(result.unwrap_err().to_string().contains("cancelled"));
        assert_eq!(seen.inputs,vec!["mousePressed","mouseReleased"]);
        assert_eq!(seen.observations,1);
    }

    #[test]
    fn a_failed_postcondition_stops_the_native_batch_before_later_input() {
        let (result, seen) = run(Mode::Navigate,json!({"action":"batch","timeout_ms":5000,"steps":[
            {"call":{"action":"click","tab":"page","selector":"#trigger","wait_after":{"selector":"#ready"}}},
            {"call":{"action":"click","tab":"page","selector":"#trigger"}}
        ]}));
        let result = result.unwrap();
        assert!(result.is_error);
        let details = result.details.unwrap();
        assert_eq!(details["failed_step"],0);
        assert_eq!(details["attempted_steps"],1);
        assert_eq!(details["remaining_steps"],1);
        assert_eq!(details["steps"][0]["details"]["dispatch_acknowledged"],true);
        assert_eq!(details["steps"][0]["details"]["wait_after"]["error_kind"],"document_changed");
        assert_eq!(seen.inputs,vec!["mousePressed","mouseReleased"]);
        assert_eq!(seen.observations,0);
    }

    fn navigation_call(address: &str, until: &str) -> Value {
        json!({"action":"click","tab":"page","selector":"#trigger","timeout_ms":5000,
            "wait_after":{"navigation":{"url":address,"wait_until":until}}})
    }

    #[test]
    fn navigation_waits_for_events_then_checks_destination_dom_after_one_input() {
        let mut args = navigation_call("https://app.example/done", "load");
        args["wait_after"]["selector"] = json!("#ready");
        args["wait_after"]["text"] = json!("Ready now");
        let (result, seen) = run(Mode::NavDocument, args);
        let result = result.unwrap();
        assert!(!result.is_error);
        let details = result.details.unwrap();
        assert_eq!(details["dispatch_acknowledged"], true);
        let observed = &details["wait_after"]["observation"];
        assert_eq!(observed["navigation"]["same_document"], false);
        assert_eq!(observed["navigation"]["http_success_verified"], false);
        assert_eq!(observed["textMatched"], true);
        assert_eq!(seen.readiness_reads, 3);
        assert_eq!(seen.observations, 3);
        assert_eq!(seen.inputs, vec!["mousePressed", "mouseReleased"]);
    }

    #[test]
    fn history_transitions_and_reload_require_changed_evidence_not_existing_urls() {
        for (mode, target, same) in [(Mode::NavHistory,"https://app.example/done",true),
            (Mode::NavReload,"https://app.example/start",false)] {
            let (result, seen) = run(mode, navigation_call(target, "commit"));
            let result = result.unwrap();
            assert!(!result.is_error);
            assert_eq!(result.details.unwrap()["wait_after"]["observation"]["navigation"]["same_document"],same);
            assert_eq!(seen.readiness_reads, 0, "commit does not inspect the destination DOM");
            assert_eq!(seen.inputs.len(), 2);
        }
        let mut args = navigation_call("https://app.example/start", "commit");
        args["timeout_ms"] = json!(1000);
        let (result, seen) = run(Mode::NavUnchanged, args);
        assert!(result.is_err());
        assert_eq!(seen.inputs.len(), 2);
        assert_eq!(seen.readiness_reads, 0);
    }

    #[test]
    fn cross_origin_destination_is_rejected_before_input() {
        let (result, seen) = run(Mode::NavDocument, navigation_call("https://other.example/done", "load"));
        assert!(result.unwrap_err().to_string().contains("BROWSER_NAVIGATION_SCOPE"));
        assert!(seen.inputs.is_empty());
        assert_eq!(seen.readiness_reads, 0);
        assert_eq!(seen.observations, 0);
    }

    #[test]
    fn unrequested_origin_and_error_documents_do_not_get_dom_inspection() {
        for (mode, kind) in [(Mode::NavOutside,"navigation_scope"), (Mode::NavError,"navigation_failed")] {
            let (result, seen) = run(mode, navigation_call("https://app.example/done", "load"));
            let result = result.unwrap();
            assert!(result.is_error);
            let details = result.details.unwrap();
            assert_eq!(details["dispatch_acknowledged"], true);
            assert_eq!(details["wait_after"]["error_kind"], kind);
            assert_eq!(seen.readiness_reads, 0);
            assert_eq!(seen.observations, 0);
            assert_eq!(seen.inputs.len(), 2);
        }
    }

    #[test]
    fn domcontentloaded_does_not_accept_interactive_before_the_event() {
        let (result, seen) = run(Mode::NavDocument, navigation_call("https://app.example/done", "domcontentloaded"));
        assert!(!result.unwrap().is_error);
        assert_eq!(seen.readiness_reads, 2);
        assert_eq!(seen.inputs.len(), 2);
    }

    #[test]
    fn navigation_world_mismatch_stops_the_batch_without_later_input() {
        let (result, seen) = run(Mode::NavMismatch, json!({"action":"batch","timeout_ms":5000,"steps":[
            {"call":navigation_call("https://app.example/done", "load")},
            {"call":{"action":"click","tab":"page","selector":"#trigger"}}
        ]}));
        let result = result.unwrap();
        assert!(result.is_error);
        let details = result.details.unwrap();
        assert_eq!(details["failed_step"], 0);
        assert_eq!(details["attempted_steps"], 1);
        assert_eq!(details["remaining_steps"], 1);
        assert_eq!(details["steps"][0]["details"]["wait_after"]["error_kind"], "document_changed");
        assert_eq!(seen.inputs.len(), 2);
        assert_eq!(seen.readiness_reads, 1);
        assert_eq!(seen.observations, 0);
    }

    #[test]
    fn navigation_cancellation_releases_admission_without_replaying_input() {
        let (result, seen) = run(Mode::NavCancelled, navigation_call("https://app.example/done", "load"));
        assert!(result.unwrap_err().to_string().contains("cancelled"));
        assert_eq!(seen.inputs.len(), 2);
        assert_eq!(seen.readiness_reads, 1);
        assert_eq!(seen.observations, 0);
    }

    #[test]
    fn history_drift_during_destination_dom_wait_is_not_retained_as_readiness() {
        let mut args = navigation_call("https://app.example/done", "commit");
        args["wait_after"]["selector"] = json!("#ready");
        let (result, seen) = run(Mode::NavDrift, args);
        let result = result.unwrap();
        assert!(result.is_error);
        assert_eq!(result.details.unwrap()["wait_after"]["error_kind"], "document_changed");
        assert_eq!(seen.observations, 1, "do not sample DOM after observing URL drift");
        assert_eq!(seen.inputs.len(), 2);
    }

    #[test]
    fn keypress_navigation_keeps_one_native_key_pair() {
        let mut args = navigation_call("https://app.example/done", "load");
        args["action"] = json!("press"); args["key"] = json!("Enter");
        args.as_object_mut().unwrap().remove("selector");
        let (result, seen) = run(Mode::NavDocument, args);
        assert!(!result.unwrap().is_error);
        assert_eq!(seen.inputs, vec!["keyDown", "keyUp"]);
        assert_eq!(seen.readiness_reads, 3);
    }
}
