//! The real browser tool against a bounded HTTP/WebSocket protocol peer.
//! These fixtures do not substitute for a live Chromium or DSR run.

use super::*;
use crate::browser::BrowserTool;
use crate::tools::Tool;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

fn headers(socket: &mut TcpStream) -> String {
    socket.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    socket.set_write_timeout(Some(Duration::from_secs(30))).unwrap();
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
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match listener.accept() {
            Ok((socket, _)) => return socket,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "dialog peer received no connection");
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("dialog peer accept: {error}"),
        }
    }
}

fn read_frame(socket: &mut TcpStream, method: &str) -> Value {
    let mut head = [0; 2];
    socket.read_exact(&mut head).unwrap();
    assert_eq!(head[0], 0x81);
    assert_ne!(head[1] & 0x80, 0);
    let size = match head[1] & 0x7f {
        126 => {
            let mut bytes = [0; 2];
            socket.read_exact(&mut bytes).unwrap();
            usize::from(u16::from_be_bytes(bytes))
        }
        127 => {
            let mut bytes = [0; 8];
            socket.read_exact(&mut bytes).unwrap();
            usize::try_from(u64::from_be_bytes(bytes)).unwrap()
        }
        size => usize::from(size),
    };
    assert!(size < 1024 * 1024);
    let mut mask = [0; 4];
    socket.read_exact(&mut mask).unwrap();
    let mut bytes = vec![0; size];
    socket.read_exact(&mut bytes).unwrap();
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte ^= mask[index % 4];
    }
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["method"], method, "no unrelated action may be replayed");
    if method.starts_with("Page.") || method.starts_with("Runtime.") {
        assert_eq!(value["sessionId"], "session-1");
    }
    value
}

fn write_frame(socket: &mut TcpStream, value: Value) {
    let bytes = serde_json::to_vec(&value).unwrap();
    socket.write_all(&[0x81]).unwrap();
    if bytes.len() < 126 {
        socket.write_all(&[u8::try_from(bytes.len()).unwrap()]).unwrap();
    } else {
        socket.write_all(&[126]).unwrap();
        socket.write_all(&u16::try_from(bytes.len()).unwrap().to_be_bytes()).unwrap();
    }
    socket.write_all(&bytes).unwrap();
}

fn reply(socket: &mut TcpStream, request: &Value, result: Value) {
    let mut response = json!({"id":request["id"],"result":result});
    if let Some(session) = request.get("sessionId") {
        response["sessionId"] = session.clone();
    }
    write_frame(socket, response);
}

fn peer(
    script: impl FnOnce(&mut TcpStream) + Send + 'static,
) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let mut discovery = accept(&listener);
        assert!(headers(&mut discovery).starts_with("GET /json/version "));
        let body = json!({"webSocketDebuggerUrl":format!("ws://{address}/devtools/browser/dialog")})
            .to_string();
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
        let request = read_frame(&mut socket, "Browser.setDownloadBehavior");
        assert_eq!(request["params"]["behavior"], "deny");
        reply(&mut socket, &request, json!({}));
        let request = read_frame(&mut socket, "Target.getTargets");
        reply(&mut socket, &request, json!({"targetInfos":[{
            "targetId":"page-1","type":"page","url":"https://example.com/form","title":"Dialog fixture"
        }]}));
        script(&mut socket);
    });
    (format!("http://{address}"), handle)
}

fn attach(socket: &mut TcpStream) {
    let request = read_frame(socket, "Target.attachToTarget");
    assert_eq!(request["params"], json!({"targetId":"page-1","flatten":true}));
    reply(socket, &request, json!({"sessionId":"session-1"}));
}

fn run(endpoint: String, args: Value, allowlist: Option<Vec<String>>) -> Result<ToolOutput> {
    let dir = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(dir.path())
        .with_mock(false)
        .with_cdp_endpoint(endpoint)
        .with_domain_allowlist(allowlist);
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    runtime.block_on(tool.execute("dialog-test", args, None))
}

#[test]
fn explicit_dialog_responses_preserve_literal_prompt_answers() {
    for args in [
        json!({"accept":false}),
        json!({"accept":true}),
        json!({"accept":true,"prompt_text":""}),
        json!({"accept":true,"prompt_text":"  secret\n雪\"\\\u{0000}  "}),
    ] {
        let expected = response_parameters(&args).unwrap();
        assert_eq!(expected["accept"], args["accept"]);
        assert_eq!(expected.get("promptText"), args.get("prompt_text"));
    }
}

#[test]
fn malformed_or_ambiguous_dialog_responses_fail_before_connection() {
    for args in [
        json!({"action":"handle_dialog","tab":"page-1"}),
        json!({"action":"handle_dialog","tab":"page-1","accept":"true"}),
        json!({"action":"handle_dialog","tab":"page-1","accept":null}),
        json!({"action":"handle_dialog","accept":true}),
        json!({"action":"handle_dialog","tab":"","accept":true}),
        json!({"action":"handle_dialog","tab":"page-1","accept":false,"prompt_text":"answer"}),
        json!({"action":"handle_dialog","tab":"page-1","accept":true,"prompt_text":1}),
        json!({"action":"handle_dialog","tab":"page-1","accept":true,"script":"must not run"}),
        json!({"action":"evaluate","script":"42","accept":true}),
    ] {
        let error = run("not a CDP endpoint".into(), args, None).unwrap_err();
        assert!(error.to_string().contains("BROWSER_DIALOG_INVALID"), "{error}");
    }
}

#[test]
fn prompt_size_limit_counts_utf8_bytes_and_does_not_echo_answers() {
    assert!(response_parameters(&json!({"accept":true,"prompt_text":"a".repeat(MAX_PROMPT_BYTES)})).is_ok());
    let secret = "雪".repeat(MAX_PROMPT_BYTES / 3 + 1);
    let error = response_parameters(&json!({"accept":true,"prompt_text":secret})).unwrap_err();
    assert!(!error.to_string().contains('雪'));
}

#[test]
fn native_dialog_responses_never_evaluate_a_suspended_page_or_replay_input() {
    for (accepted, answer) in [(false, None), (true, None), (true, Some("")), (true, Some("  雪\n  "))] {
        let expected = answer.map(str::to_owned);
        let (endpoint, handle) = peer(move |socket| {
            attach(socket);
            let request = read_frame(socket, "Page.handleJavaScriptDialog");
            let mut params = json!({"accept":accepted});
            if let Some(answer) = expected {
                params["promptText"] = json!(answer);
            }
            assert_eq!(request["params"], params);
            write_frame(socket, json!({"sessionId":"session-1","method":"Page.javascriptDialogClosed","params":{"result":accepted,"userInput":"not-for-output"}}));
            reply(socket, &request, json!({}));
        });
        let mut args = json!({"action":"handle_dialog","tab":"page-1","accept":accepted});
        if let Some(answer) = answer {
            args["prompt_text"] = json!(answer);
        }
        let result = run(endpoint, args, None).unwrap();
        let details = result.details.unwrap();
        assert_eq!(details["accepted"], accepted);
        assert_eq!(details["trigger_replayed"], false);
        assert_eq!(details["prompt_text_supplied"], answer.is_some());
        assert!(!details.to_string().contains("not-for-output"));
        handle.join().unwrap();
    }
}

#[test]
fn native_dialog_failure_is_not_a_success_and_does_not_echo_prompt_data() {
    let (endpoint, handle) = peer(|socket| {
        attach(socket);
        let request = read_frame(socket, "Page.handleJavaScriptDialog");
        write_frame(socket, json!({"id":request["id"],"sessionId":"session-1","error":{"code":-32000,"message":"private-answer"}}));
    });
    let error = run(endpoint, json!({"action":"handle_dialog","tab":"page-1","accept":true,"prompt_text":"private-answer"}), None).unwrap_err();
    assert!(error.to_string().contains("BROWSER_DIALOG_RESPONSE_FAILED"));
    assert!(!error.to_string().contains("private-answer"));
    handle.join().unwrap();
}

#[test]
fn dialog_responses_obey_the_current_target_domain_guard() {
    let (endpoint, handle) = peer(|_| {});
    let result = run(endpoint, json!({"action":"handle_dialog","tab":"page-1","accept":true}), Some(vec!["allowed.invalid".into()]));
    assert!(result.is_err());
    handle.join().unwrap();
}

#[test]
fn dialog_actions_reject_mock_successes_and_are_discoverable() {
    let dir = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(dir.path()).with_mock(true);
    assert!(tool.parameters()["properties"]["action"]["enum"].as_array().unwrap().contains(&json!("handle_dialog")));
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    assert!(runtime.block_on(tool.execute("no-fake-dialog", json!({"action":"handle_dialog","tab":"page-1","accept":false}), None)).is_err());
}

#[test]
fn handling_a_dialog_in_an_idle_managed_session_never_launches_a_browser() {
    let dir = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(dir.path())
        .with_mock(false)
        .with_launch_options(crate::browser::BrowserLaunchOptions {
            executable_path: Some(std::path::PathBuf::from("/nonexistent/do-not-launch")),
            ..Default::default()
        });
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    let error = runtime
        .block_on(tool.execute(
            "idle-dialog",
            json!({"action":"handle_dialog","tab":"page-1","accept":false}),
            None,
        ))
        .unwrap_err();
    assert!(error.to_string().contains("no owned browser is running"));
}

fn expectation(kind: &str, accepted: bool) -> Value {
    json!({"type":kind,"message":"Continue?","url":"https://example.com/form","accept":accepted})
}

fn opening(kind: &str) -> Value {
    json!({"sessionId":"session-1","method":"Page.javascriptDialogOpening","params":{
        "type":kind,"message":"Continue?","url":"https://example.com/form",
        "hasBrowserHandler":false,"defaultPrompt":"do-not-copy"
    }})
}

fn triggered_args(expected: Value) -> Value {
    let script = match expected["type"].as_str() {
        Some("prompt") => "void prompt('Continue?')",
        Some("alert") => "alert('Continue?'); 42",
        _ => "confirm('Continue?')",
    };
    json!({"action":"evaluate","tab":"page-1","script":script,
           "dialog_response":expected})
}

fn enable(socket: &mut TcpStream) {
    attach(socket);
    let request = read_frame(socket, "Page.enable");
    reply(socket, &request, json!({}));
}

#[test]
fn expected_dialog_validation_rejects_ambiguous_consent_before_connecting() {
    let good = expectation("confirm", true);
    for field in ["type", "message", "url", "accept"] {
        let mut missing = good.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert!(Expected::from_args(&triggered_args(missing), None).is_err());
    }
    for value in [Value::Null, json!([]), json!(true), json!("accept")] {
        assert!(Expected::from_args(&triggered_args(value), None).is_err());
    }
    for (field, value) in [
        ("type", json!("file-chooser")), ("accept", json!("yes")),
        ("message", json!(null)), ("url", json!("javascript:alert(1)")),
        ("prompt_text", json!("not a prompt")), ("match_any", json!(true)),
    ] {
        let mut bad = good.clone();
        bad[field] = value;
        let error = run("not a CDP endpoint".into(), triggered_args(bad), None).unwrap_err();
        assert!(error.to_string().contains("BROWSER_DIALOG_INVALID"), "{error}");
    }
    for action in ["start", "goto", "open", "download", "handle_dialog", "snapshot"] {
        let mut args = triggered_args(good.clone());
        args["action"] = json!(action);
        assert!(Expected::from_args(&args, None).is_err());
    }
    let mut implicit = triggered_args(good);
    implicit.as_object_mut().unwrap().remove("tab");
    assert!(Expected::from_args(&implicit, None).is_err());
}

#[test]
fn one_operation_consent_requires_exact_type_message_url_and_session() {
    let args = triggered_args(expectation("confirm", false));
    let mut state = State::default();
    state.arm(Expected::from_args(&args, None).unwrap().unwrap());
    let mut foreign = opening("confirm");
    foreign["sessionId"] = json!("another-tab");
    assert!(state.response_for(&foreign, Some("session-1"), false).unwrap().is_none());
    assert_eq!(state.response_for(&opening("confirm"), Some("session-1"), false).unwrap(),
        Some(json!({"accept":false})));
    assert!(state.response_for(&opening("confirm"), Some("session-1"), false).is_err());
    for field in ["type", "message", "url"] {
        let mut state = State::default();
        state.arm(Expected::from_args(&args, None).unwrap().unwrap());
        let mut changed = opening("confirm");
        changed["params"][field] = json!("mismatch-private-data");
        let error = state.response_for(&changed, Some("session-1"), false).unwrap_err();
        assert!(error.to_string().contains("BROWSER_DIALOG_MISMATCH"));
        assert!(!error.to_string().contains("private-data"));
    }
}

#[test]
fn dialog_acknowledgement_requires_its_id_session_and_success_object() {
    let mut state = State::default();
    state.record_sent(9, false);
    assert!(!state.acknowledge(&json!({"id":8,"result":{}}), Some("session-1")).unwrap());
    for response in [
        json!({"id":9,"result":{}}),
        json!({"id":9,"sessionId":"other","result":{}}),
        json!({"id":9,"sessionId":"session-1","result":null}),
        json!({"id":9,"sessionId":"session-1","result":{},"error":{}}),
    ] {
        assert!(state.acknowledge(&response, Some("session-1")).is_err());
        assert!(!state.completed());
    }
    assert!(state.acknowledge(&json!({"id":9,"sessionId":"session-1","result":{}}), Some("session-1")).unwrap());
    assert!(state.completed());
    assert!(!state.pending());
}

#[test]
fn native_trigger_waits_for_both_replies_in_either_order() {
    for primary_first in [false, true] {
        let (endpoint, handle) = peer(move |socket| {
            enable(socket);
            let primary = read_frame(socket, "Runtime.evaluate");
            let mut foreign = opening("confirm");
            foreign["sessionId"] = json!("other-tab");
            write_frame(socket, foreign);
            write_frame(socket, opening("confirm"));
            let handler = read_frame(socket, "Page.handleJavaScriptDialog");
            assert_eq!(handler["params"], json!({"accept":false}));
            assert_ne!(handler["id"], primary["id"]);
            let result = json!({"result":{"type":"boolean","value":false}});
            if primary_first {
                reply(socket, &primary, result);
                reply(socket, &handler, json!({}));
            } else {
                reply(socket, &handler, json!({}));
                reply(socket, &primary, result);
            }
        });
        let out = run(endpoint, triggered_args(expectation("confirm", false)), None).unwrap();
        let details = out.details.unwrap();
        assert_eq!(details["result"], false);
        assert_eq!(details["dialog_response"], json!({"acknowledged":true,"accepted":false,"trigger_replayed":false}));
        handle.join().unwrap();
    }
}

#[test]
fn native_trigger_waits_for_a_dialog_after_the_primary_command_acknowledgement() {
    let (endpoint, handle) = peer(|socket| {
        enable(socket);
        let primary = read_frame(socket, "Runtime.evaluate");
        reply(socket, &primary, json!({"result":{"type":"number","value":42}}));
        write_frame(socket, opening("alert"));
        let handler = read_frame(socket, "Page.handleJavaScriptDialog");
        assert_eq!(handler["params"], json!({"accept":true}));
        reply(socket, &handler, json!({}));
    });
    let mut args = triggered_args(expectation("alert", true));
    args["script"] = json!("setTimeout(() => alert('Continue?'), 0); 42");
    let out = run(endpoint, args, None).unwrap();
    assert_eq!(out.details.unwrap()["result"], 42);
    handle.join().unwrap();
}

#[test]
fn native_expected_prompt_preserves_the_exact_answer_without_receipt_echo() {
    let (endpoint, handle) = peer(|socket| {
        enable(socket);
        let primary = read_frame(socket, "Runtime.evaluate");
        write_frame(socket, opening("prompt"));
        let handler = read_frame(socket, "Page.handleJavaScriptDialog");
        assert_eq!(handler["params"], json!({"accept":true,"promptText":"  private\n雪  "}));
        reply(socket, &handler, json!({}));
        reply(socket, &primary, json!({"result":{"type":"undefined"}}));
    });
    let mut expected = expectation("prompt", true);
    expected["prompt_text"] = json!("  private\n雪  ");
    let out = run(endpoint, triggered_args(expected), None).unwrap();
    assert!(!out.details.unwrap().to_string().contains("private"));
    handle.join().unwrap();
}

#[test]
fn native_preexisting_dialog_does_not_spend_consent_for_a_future_trigger() {
    let (endpoint, handle) = peer(|socket| {
        attach(socket);
        let _enable = read_frame(socket, "Page.enable");
        write_frame(socket, opening("confirm"));
        // No Runtime.evaluate or Page.handleJavaScriptDialog should be sent.
    });
    let error = run(endpoint, triggered_args(expectation("confirm", true)), None).unwrap_err();
    assert!(error.to_string().contains("BROWSER_DIALOG_UNEXPECTED"), "{error}");
    handle.join().unwrap();
}

#[test]
fn native_mismatched_dialog_never_receives_automatic_consent() {
    for field in ["type", "message", "url"] {
        let (endpoint, handle) = peer(move |socket| {
            enable(socket);
            let _primary = read_frame(socket, "Runtime.evaluate");
            let mut event = opening("confirm");
            event["params"][field] = json!("unrelated-private-dialog");
            write_frame(socket, event);
        });
        let error = run(endpoint, triggered_args(expectation("confirm", true)), None).unwrap_err();
        assert!(error.to_string().contains("BROWSER_DIALOG_MISMATCH"), "{error}");
        assert!(!error.to_string().contains("private-dialog"));
        handle.join().unwrap();
    }
}

#[test]
fn native_second_dialog_cannot_reuse_the_first_response() {
    let (endpoint, handle) = peer(|socket| {
        enable(socket);
        let _primary = read_frame(socket, "Runtime.evaluate");
        write_frame(socket, opening("confirm"));
        let _handler = read_frame(socket, "Page.handleJavaScriptDialog");
        write_frame(socket, opening("confirm"));
    });
    let error = run(endpoint, triggered_args(expectation("confirm", true)), None).unwrap_err();
    assert!(error.to_string().contains("BROWSER_DIALOG_UNEXPECTED"), "{error}");
    handle.join().unwrap();
}

#[test]
fn native_handler_error_invalidates_a_buffered_success_without_echoing_it() {
    let (endpoint, handle) = peer(|socket| {
        enable(socket);
        let primary = read_frame(socket, "Runtime.evaluate");
        write_frame(socket, opening("confirm"));
        let handler = read_frame(socket, "Page.handleJavaScriptDialog");
        reply(socket, &primary, json!({"result":{"type":"string","value":"private-result"}}));
        write_frame(socket, json!({"id":handler["id"],"sessionId":"session-1",
            "error":{"code":-32000,"message":"private-response"}}));
    });
    let error = run(endpoint, triggered_args(expectation("confirm", true)), None).unwrap_err();
    assert!(error.to_string().contains("BROWSER_DIALOG_RESPONSE_FAILED"), "{error}");
    assert!(!error.to_string().contains("private-"));
    handle.join().unwrap();
}

#[test]
fn native_trigger_error_is_not_hidden_by_a_successful_dialog_response() {
    let (endpoint, handle) = peer(|socket| {
        enable(socket);
        let primary = read_frame(socket, "Runtime.evaluate");
        write_frame(socket, opening("confirm"));
        let handler = read_frame(socket, "Page.handleJavaScriptDialog");
        reply(socket, &handler, json!({}));
        write_frame(socket, json!({"id":primary["id"],"sessionId":"session-1",
            "error":{"code":-32000,"message":"trigger failed"}}));
    });
    let error = run(endpoint, triggered_args(expectation("confirm", true)), None).unwrap_err();
    assert!(error.to_string().contains("trigger failed"));
    handle.join().unwrap();
}

#[test]
fn expected_dialog_url_is_authorized_before_any_browser_connection() {
    let error = run("not a CDP endpoint".into(), triggered_args(expectation("confirm", true)),
        Some(vec!["elsewhere.invalid".into()])).unwrap_err();
    assert!(error.to_string().contains("expected dialog URL is not allowed"));
}

#[test]
fn mock_mode_cannot_silently_ignore_an_expected_dialog_response() {
    let dir = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(dir.path()).with_mock(true);
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    let error = runtime.block_on(tool.execute("mock-expectation",
        triggered_args(expectation("confirm", true)), None)).unwrap_err();
    assert!(error.to_string().contains("native backend"));
}

#[test]
fn expected_dialog_missing_after_command_success_is_not_reported_as_success() {
    let (endpoint, handle) = peer(|socket| {
        enable(socket);
        let primary = read_frame(socket, "Runtime.evaluate");
        reply(socket, &primary, json!({"result":{"type":"number","value":42}}));
        // The peer closes with no dialog. A future missing dialog is not a
        // successful "no-op" response and must not replay the evaluate.
    });
    assert!(run(endpoint, triggered_args(expectation("confirm", true)), None).is_err());
    handle.join().unwrap();
}

#[test]
fn expected_dialog_wait_uses_the_existing_whole_operation_deadline() {
    let (endpoint, handle) = peer(|socket| {
        enable(socket);
        let primary = read_frame(socket, "Runtime.evaluate");
        reply(socket, &primary, json!({"result":{"type":"number","value":42}}));
        // Wait for client teardown rather than racing a fixed server sleep.
        let mut byte = [0];
        assert_eq!(socket.read(&mut byte).unwrap(), 0);
    });
    let mut args = triggered_args(expectation("confirm", true));
    args["timeout_ms"] = json!(5000);
    let error = run(endpoint, args, None).unwrap_err();
    assert!(error.to_string().contains("operation timed out"), "{error}");
    handle.join().unwrap();
}
