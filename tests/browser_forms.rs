//! Public form actions and an explicitly selected real-Chromium scenario.
//! The ignored scenario never silently passes when Chromium is unavailable.
#![forbid(unsafe_code)]
#![recursion_limit = "512"]

use pi::browser::{BrowserLaunchOptions, BrowserTool};
use pi::tools::Tool;
use serde_json::{Value, json};

#[test]
fn selection_schema_and_preflight_work_without_a_browser_connection() {
    let dir = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(dir.path()).with_mock(false)
        .with_cdp_endpoint("not-a-CDP-endpoint");
    let schema = tool.parameters();
    assert!(schema["properties"]["action"]["enum"].as_array().unwrap().contains(&json!("select_option")));
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    runtime.block_on(async {
        for args in [
            json!({"action":"select_option","selector":"#s"}),
            json!({"action":"select_option","selector":"#s","options":null}),
            json!({"action":"select_option","selector":"#s","options":[{"index":-1}]}),
            json!({"action":"select_option","selector":"#s","options":[],"text":"ignored"}),
            json!({"action":"select_option","selector":"#s","options":[],"dialog_response":{}}),
        ] {
            let error = tool.execute("form-preflight", args, None).await.unwrap_err().to_string();
            assert!(!error.contains("CDP endpoint"), "{error}");
        }
    });
}

#[test]
fn selection_never_reports_mock_success() {
    let dir = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(dir.path()).with_mock(true);
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    assert!(runtime.block_on(tool.execute("form-mock", json!({
        "action":"select_option","selector":"#s","options":[{"value":"a"}],
    }), None)).is_err());
}

#[test]
fn checked_schema_preflight_and_mock_refusal_require_explicit_state() {
    let dir = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(dir.path()).with_mock(false)
        .with_cdp_endpoint("not-a-CDP-endpoint");
    let schema = tool.parameters();
    assert!(schema["properties"]["action"]["enum"].as_array().unwrap().contains(&json!("set_checked")));
    assert_eq!(schema["properties"]["checked"]["type"], "boolean");
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    runtime.block_on(async {
        for args in [
            json!({"action":"set_checked","selector":"#c"}),
            json!({"action":"set_checked","selector":"#c","checked":"false"}),
            json!({"action":"set_checked","selector":"#c","checked":true,"options":[]}),
            json!({"action":"set_checked","selector":"#c","checked":true,"frame":"child"}),
        ] {
            let error = tool.execute("checked-preflight", args, None).await.unwrap_err().to_string();
            assert!(!error.contains("CDP endpoint"), "{error}");
        }
        let mock = BrowserTool::new(dir.path()).with_mock(true);
        assert!(mock.execute("checked-mock", json!({"action":"set_checked","selector":"#c","checked":true}), None).await.is_err());
    });
}

async fn call(tool: &BrowserTool, args: Value) -> Value {
    let result = tool.execute("form-live", args, None).await.unwrap();
    assert!(!result.is_error);
    result.details.unwrap()
}

#[test]
#[ignore = "requires sandbox-capable Chromium; select explicitly in the DSR browser lane"]
fn live_selection_preserves_options_events_idempotency_and_frame_scope() {
    let dir = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(dir.path()).with_mock(false)
        .with_launch_options(BrowserLaunchOptions {
            executable_path: std::env::var_os("PI_BROWSER_EXECUTABLE").map(Into::into),
            ..Default::default()
        }).with_domain_allowlist(Some(Vec::new()));
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    runtime.block_on(async {
        call(&tool, json!({"action":"open","tab":"forms","url":"about:blank"})).await;
        call(&tool, json!({"action":"evaluate","tab":"forms","script":r#"
            (() => {
                document.body.innerHTML='<select id="choice" multiple><option value="a" selected>A</option><option value="b">B</option><option value="c">C</option><option value="x">Duplicate</option><option value="x">Duplicate</option><option value="d" disabled>D</option></select>';
                window.events=[];
                for (const kind of ['input','change']) choice.addEventListener(kind,e=>events.push(e.type));
                return true;
            })()
        "#})).await;
        let result = call(&tool, json!({"action":"select_option","tab":"forms","selector":"#choice",
            "options":[{"label":"C"},{"value":"b"}]})).await;
        assert_eq!(result["selected"][0]["index"], 1);
        assert_eq!(result["selected"][1]["index"], 2);
        assert_eq!(result["changed"], true);
        assert_eq!(result["input_method"], "dom-selection");
        let result = call(&tool, json!({"action":"select_option","tab":"forms","selector":"#choice",
            "options":[{"index":2},{"index":1}]})).await;
        assert_eq!(result["changed"], false);
        let result = call(&tool, json!({"action":"evaluate","tab":"forms","script":"events"})).await;
        assert_eq!(result["result"], json!(["input","change"]));
        for options in [json!([{"value":"a"},{"value":"missing"}]),
            json!([{"value":"x"}]), json!([{"value":"d"}]), json!([{"index":1},{"value":"b"}])] {
            assert!(tool.execute("invalid-selection", json!({"action":"select_option","tab":"forms",
                "selector":"#choice","options":options}), None).await.is_err());
            let value = call(&tool, json!({"action":"evaluate","tab":"forms",
                "script":"Array.from(choice.selectedOptions,o=>o.value)"})).await;
            assert_eq!(value["result"], json!(["b","c"]));
        }
        let result = call(&tool, json!({"action":"select_option","tab":"forms","selector":"#choice","options":[]})).await;
        assert_eq!(result["selected"], json!([]));

        call(&tool, json!({"action":"evaluate","tab":"forms","script":r#"
            new Promise(resolve=>{
                const f=document.createElement('iframe');f.name='form-child';
                f.srcdoc='<select id="choice"><option value="parent">Parent</option><option value="child">Child</option></select>';
                f.onload=()=>resolve(true);document.body.append(f);
            })
        "#})).await;
        let frames = call(&tool, json!({"action":"list_frames","tab":"forms"})).await;
        let frame = frames["frames"].as_array().unwrap().iter()
            .find(|frame| frame["name"] == "form-child").unwrap()["frame_id"].clone();
        let result = call(&tool, json!({"action":"select_option","tab":"forms","frame":frame,
            "selector":"#choice","options":[{"value":"child"}]})).await;
        assert_eq!(result["selected"][0]["value"], "child");
        let parent = call(&tool, json!({"action":"evaluate","tab":"forms","script":"choice.selectedOptions.length"})).await;
        assert_eq!(parent["result"], 0);

        call(&tool, json!({"action":"evaluate","tab":"forms","script":
            "choice.addEventListener('change',()=>{choice.selectedIndex=0});true"
        })).await;
        assert!(tool.execute("overridden-selection", json!({"action":"select_option","tab":"forms",
            "selector":"#choice","options":[{"value":"b"}]}), None).await.is_err());
        call(&tool, json!({"action":"stop"})).await;
    });
}

#[test]
#[ignore = "requires sandbox-capable Chromium; select explicitly in the DSR browser lane"]
fn live_checked_state_uses_one_native_click_and_detects_page_rejection() {
    let dir = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(dir.path()).with_mock(false)
        .with_launch_options(BrowserLaunchOptions {
            executable_path: std::env::var_os("PI_BROWSER_EXECUTABLE").map(Into::into),
            ..Default::default()
        }).with_domain_allowlist(Some(Vec::new()));
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    runtime.block_on(async {
        call(&tool, json!({"action":"open","tab":"checks","url":"about:blank"})).await;
        call(&tool, json!({"action":"evaluate","tab":"checks","script":r#"
            (()=>{
                document.body.innerHTML='<input id="check" type="checkbox"><input id="r1" type="radio" name="g" checked><input id="r2" type="radio" name="g">';
                window.clicks=[];check.addEventListener('click',e=>clicks.push(e.isTrusted));return true;
            })()
        "#})).await;
        for (wanted, changed) in [(true,true), (true,false), (false,true)] {
            let result = call(&tool, json!({"action":"set_checked","tab":"checks","selector":"#check","checked":wanted})).await;
            assert_eq!(result["checked"], wanted);
            assert_eq!(result["changed"], changed);
            assert_eq!(result["input_method"], "native-pointer");
        }
        let events = call(&tool, json!({"action":"evaluate","tab":"checks","script":"clicks"})).await;
        assert_eq!(events["result"], json!([true,true]));
        call(&tool, json!({"action":"set_checked","tab":"checks","selector":"#r2","checked":true})).await;
        let group = call(&tool, json!({"action":"evaluate","tab":"checks","script":"[r1.checked,r2.checked]"})).await;
        assert_eq!(group["result"], json!([false,true]));
        assert!(tool.execute("radio-uncheck", json!({"action":"set_checked","tab":"checks","selector":"#r2","checked":false}), None).await.is_err());
        call(&tool, json!({"action":"evaluate","tab":"checks","script":"check.addEventListener('click',e=>e.preventDefault());true"})).await;
        assert!(tool.execute("cancelled-check", json!({"action":"set_checked","tab":"checks","selector":"#check","checked":true}), None).await.is_err());
        let events = call(&tool, json!({"action":"evaluate","tab":"checks","script":"clicks"})).await;
        assert_eq!(events["result"], json!([true,true,true]), "rejected click must not be replayed");
        call(&tool, json!({"action":"stop"})).await;
    });
}

// Real loopback transport, not a Chromium simulation claim. These peers make
// public dispatcher/identity/verification/replay errors observable without a
// browser install; the ignored tests above separately exercise the real DOM.
mod wire {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;
    use std::time::{Duration, Instant};

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Fault { None, RevertSelection, Navigate, RejectClick, PressError, AlreadyAfterScroll }

    #[derive(Default)]
    struct Seen { queries: usize, mutations: usize, verifications: usize, inputs: Vec<String> }

    fn accept(listener: &TcpListener) -> TcpStream {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match listener.accept() {
                Ok((socket, _)) => {
                    socket.set_nonblocking(false).unwrap();
                    socket.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
                    socket.set_write_timeout(Some(Duration::from_secs(10))).unwrap();
                    return socket;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "form peer accept deadline");
                    thread::sleep(Duration::from_millis(5));
                }
                Err(e) => panic!("form peer accept: {e}"),
            }
        }
    }

    fn headers(socket: &mut TcpStream) -> String {
        let mut data = Vec::new();
        while !data.ends_with(b"\r\n\r\n") {
            assert!(data.len() < 16384);
            let mut byte = [0];socket.read_exact(&mut byte).unwrap();data.push(byte[0]);
        }
        String::from_utf8(data).unwrap()
    }

    fn receive(socket: &mut TcpStream) -> Option<Value> {
        let mut header = [0;2];
        match socket.read_exact(&mut header) {
            Ok(()) => {}
            Err(e) if matches!(e.kind(), std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset) => return None,
            Err(e) => panic!("form peer frame: {e}"),
        }
        if header[0] & 15 == 8 { return None; }
        assert_eq!(header[0], 0x81);assert_ne!(header[1] & 128, 0);
        let size = match header[1] & 127 {
            126 => {let mut b=[0;2];socket.read_exact(&mut b).unwrap();usize::from(u16::from_be_bytes(b))}
            127 => panic!("unexpected large form request"),
            n => usize::from(n),
        };
        let mut mask=[0;4];socket.read_exact(&mut mask).unwrap();
        let mut data=vec![0;size];socket.read_exact(&mut data).unwrap();
        for (i, b) in data.iter_mut().enumerate() {*b ^= mask[i%4];}
        Some(serde_json::from_slice(&data).unwrap())
    }

    fn send(socket: &mut TcpStream, value: Value) {
        let data=serde_json::to_vec(&value).unwrap();
        let mut frame=vec![0x81];
        if data.len()<126 {frame.push(u8::try_from(data.len()).unwrap());}
        else {frame.push(126);frame.extend(u16::try_from(data.len()).unwrap().to_be_bytes());}
        frame.extend(data);socket.write_all(&frame).unwrap();
    }

    fn serve(socket: &mut TcpStream, fault: Fault, mut checked: bool) -> Seen {
        let mut seen=Seen::default();
        for _ in 0..512 {
            let Some(request)=receive(socket) else {return seen};
            let method=request["method"].as_str().unwrap();
            let params=&request["params"];
            let mut failed=false;
            let result=match method {
                "Browser.setDownloadBehavior" => {assert_eq!(params["behavior"],"deny");json!({})}
                "Target.getTargets" => json!({"targetInfos":[{"targetId":"target","type":"page","url":"about:blank"}]}),
                "Target.attachToTarget" => json!({"sessionId":"page"}),
                "Target.setAutoAttach" => json!({}),
                "Page.getFrameTree" => json!({"frameTree":{"frame":{"id":"main","url":"about:blank",
                    "loaderId":if fault==Fault::Navigate && seen.mutations>0 {"new"} else {"loader"}}}}),
                "Page.createIsolatedWorld" => {assert_eq!(params["frameId"],"main");json!({"executionContextId":7})}
                "DOM.getDocument" => json!({"root":{"nodeId":1}}),
                "DOM.querySelector" => {seen.queries+=1;json!({"nodeId":9})}
                "DOM.describeNode" => json!({"node":{"backendNodeId":42}}),
                "DOM.resolveNode" => {
                    assert_eq!(params["backendNodeId"],42,"verification cannot substitute a new node");
                    assert_eq!(params["executionContextId"],7);
                    json!({"object":{"objectId":"control"}})
                }
                "Runtime.releaseObject" => {assert_eq!(params["objectId"],"control");json!({})}
                "Runtime.callFunctionOn" => {
                    if params["functionDeclaration"].as_str().unwrap().contains("Document.prototype.querySelector") {
                        seen.queries+=1;json!({"result":{"objectId":"control","type":"object"}})
                    } else {
                        assert_eq!(params["objectId"],"control");
                        let action=params["arguments"][0]["value"].as_str().unwrap();
                        let value=match action {
                            "form_select" => {
                                assert_eq!(params["arguments"][1]["value"],json!([{"value":"b"}]));
                                seen.mutations+=1;
                                json!({"changed":true,"multiple":false,"selected":[{"index":1,"value":"b","label":"B"}]})
                            }
                            "form_selection" => {
                                seen.verifications+=1;
                                json!({"multiple":false,"selected":[{"index":1,"value":if fault==Fault::RevertSelection {"a"} else {"b"},"label":"B"}]})
                            }
                            "form_checked" => {seen.verifications+=1;json!({"kind":"checkbox","checked":checked,"indeterminate":false})}
                            "point" => json!({"x":40,"y":30}),
                            _ => panic!("unexpected form helper action {action}"),
                        };
                        json!({"result":{"type":"object","value":value}})
                    }
                }
                "Page.bringToFront" => json!({}),
                "DOM.scrollIntoViewIfNeeded" => {
                    assert_eq!(params["backendNodeId"],42);
                    if fault==Fault::AlreadyAfterScroll {checked=true;}
                    json!({})
                }
                "Input.dispatchMouseEvent" => {
                    assert_eq!(request["sessionId"],"page");
                    assert_eq!(params["x"].as_f64(),Some(40.0));
                    assert_eq!(params["y"].as_f64(),Some(30.0));
                    let kind=params["type"].as_str().unwrap();seen.inputs.push(kind.into());
                    if kind=="mousePressed" && fault==Fault::PressError {failed=true;}
                    if kind=="mouseReleased" && fault!=Fault::RejectClick {checked=!checked;}
                    json!({})
                }
                _ => panic!("unexpected form protocol command {method}"),
            };
            let mut reply=if failed {json!({"id":request["id"],"error":{"code":-32000,"message":"press failed"}})}
                else {json!({"id":request["id"],"result":result})};
            if let Some(session)=request.get("sessionId") {reply["sessionId"]=session.clone();}
            send(socket,reply);
        }
        panic!("form peer command budget exceeded")
    }

    fn run(mut args: Value, fault: Fault, checked: bool) -> (pi::error::Result<pi::tools::ToolOutput>, Seen) {
        let listener=TcpListener::bind("127.0.0.1:0").unwrap();listener.set_nonblocking(true).unwrap();
        let address=listener.local_addr().unwrap();
        let peer=thread::spawn(move || {
            let mut http=accept(&listener);assert!(headers(&mut http).starts_with("GET /json/version "));
            let body=json!({"webSocketDebuggerUrl":format!("ws://{address}/devtools/browser/forms")}).to_string();
            write!(http,"HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();drop(http);
            let mut socket=accept(&listener);let request=headers(&mut socket);
            let key=request.lines().find_map(|line| {
                let (name,value)=line.split_once(':')?;
                name.eq_ignore_ascii_case("sec-websocket-key").then(||value.trim())
            }).unwrap();
            let key=asupersync::net::websocket::compute_accept_key(key);
            write!(socket,"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {key}\r\n\r\n").unwrap();
            serve(&mut socket,fault,checked)
        });
        let dir=tempfile::tempdir().unwrap();
        let tool=BrowserTool::new(dir.path()).with_mock(false).with_cdp_endpoint(format!("http://{address}"));
        args["tab"]=json!("target");args["timeout_ms"]=json!(5000);
        let runtime=asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
        let result=runtime.block_on(tool.execute("forms-wire",args,None));drop(tool);
        (result,peer.join().unwrap())
    }

    #[test]
    fn public_selection_verifies_once_without_rebinding_or_replaying() {
        for frame in [None,Some("main")] {
            for fault in [Fault::None,Fault::RevertSelection,Fault::Navigate] {
                let mut args=json!({"action":"select_option","selector":"#s","options":[{"value":"b"}]});
                if let Some(frame)=frame {args["frame"]=json!(frame);}
                let (result,seen)=run(args,fault,false);
                assert_eq!(result.is_ok(),fault==Fault::None);
                assert_eq!(seen.queries,1);assert_eq!(seen.mutations,1);assert!(seen.inputs.is_empty());
                assert_eq!(seen.verifications,usize::from(fault!=Fault::Navigate));
            }
        }
    }

    #[test]
    fn public_checked_noops_and_native_transitions_have_exact_input_counts() {
        for (initial,wanted) in [(false,false),(true,true),(false,true),(true,false)] {
            let (result,seen)=run(json!({"action":"set_checked","selector":"#c","checked":wanted}),Fault::None,initial);
            let result=result.unwrap().details.unwrap();
            assert_eq!(result["changed"],initial!=wanted);assert_eq!(result["checked"],wanted);
            assert_eq!(seen.queries,1);assert_eq!(seen.mutations,0);
            let expected=if initial==wanted {vec![]} else {vec!["mousePressed","mouseReleased"]};
            assert_eq!(seen.inputs,expected);
        }
    }

    #[test]
    fn checked_input_failures_release_once_and_never_retry_a_toggle() {
        for fault in [Fault::RejectClick,Fault::PressError,Fault::AlreadyAfterScroll] {
            let (result,seen)=run(json!({"action":"set_checked","selector":"#c","checked":true}),fault,false);
            assert_eq!(result.is_ok(),fault==Fault::AlreadyAfterScroll);
            assert_eq!(seen.queries,1);
            if fault==Fault::AlreadyAfterScroll {assert!(seen.inputs.is_empty());}
            else {assert_eq!(seen.inputs,["mousePressed","mouseReleased"]);}
        }
    }
}
