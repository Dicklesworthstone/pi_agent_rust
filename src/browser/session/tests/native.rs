//! Public BrowserTool workflows over real HTTP/WebSocket transport.
//! The deterministic peer below is a protocol fixture, not a browser simulator
//! or evidence that the Rust integration has run against Chromium.

use super::*;
use futures::FutureExt as _;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

struct Step {
    method: &'static str,
    params: Value,
    reply: Option<Value>,
    hook: Option<Box<dyn FnOnce() + Send>>,
    after_reply: Option<Box<dyn FnOnce() + Send>>,
}

fn step(method: &'static str, params: Value, result: Value) -> Step {
    Step { method, params, reply: Some(json!({"result":result})), hook: None, after_reply: None }
}

fn headers(stream: &mut TcpStream) -> String {
    stream.set_read_timeout(Some(Duration::from_secs(8))).unwrap();
    stream.set_write_timeout(Some(Duration::from_secs(8))).unwrap();
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        assert!(bytes.len() < 16 * 1024, "bounded HTTP header");
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
    }
    String::from_utf8(bytes).unwrap()
}

fn accept(listener: &TcpListener) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        match listener.accept() {
            Ok((stream, _)) => return stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "native browser never connected");
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("fixture accept: {error}"),
        }
    }
}

fn request(stream: &mut TcpStream) -> Option<Value> {
    let mut first = [0];
    match stream.read(&mut first) {
        Ok(0) => return None,
        Err(error) if matches!(error.kind(), std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe) => return None,
        Ok(_) => {}
        Err(error) => panic!("fixture frame read: {error}"),
    }
    let closing = first[0] == 0x88;
    assert!(closing || first[0] == 0x81, "one final text or close frame");
    let mut second = [0];
    stream.read_exact(&mut second).unwrap();
    assert_ne!(second[0] & 0x80, 0, "client frames must be masked");
    let size = match second[0] & 0x7f {
        126 => {
            let mut bytes = [0; 2];
            stream.read_exact(&mut bytes).unwrap();
            usize::from(u16::from_be_bytes(bytes))
        }
        127 => {
            let mut bytes = [0; 8];
            stream.read_exact(&mut bytes).unwrap();
            usize::try_from(u64::from_be_bytes(bytes)).unwrap()
        }
        size => usize::from(size),
    };
    assert!(size <= 128 * 1024, "bounded client frame");
    let mut mask = [0; 4];
    stream.read_exact(&mut mask).unwrap();
    let mut bytes = vec![0; size];
    stream.read_exact(&mut bytes).unwrap();
    for (index, byte) in bytes.iter_mut().enumerate() { *byte ^= mask[index % 4]; }
    if closing { return None; }
    Some(serde_json::from_slice(&bytes).unwrap())
}

fn reply(stream: &mut TcpStream, frame: &Value) {
    let bytes = serde_json::to_vec(frame).unwrap();
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
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let worker = thread::spawn(move || {
        for script in scripts {
            let mut discovery = accept(&listener);
            assert!(headers(&mut discovery).starts_with("GET /json/version "));
            let body = json!({"webSocketDebuggerUrl":format!("ws://{address}/devtools/browser/ownership")}).to_string();
            write!(discovery, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            drop(discovery);
            let mut socket = accept(&listener);
            let upgrade = headers(&mut socket);
            assert!(upgrade.starts_with("GET /devtools/browser/ownership "));
            let key = upgrade.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("sec-websocket-key").then(|| value.trim())
            }).unwrap();
            let accept_key = asupersync::net::websocket::compute_accept_key(key);
            write!(socket, "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept_key}\r\n\r\n").unwrap();
            let mut closed = false;
            for mut expected in script {
                let message = request(&mut socket).expect("missing expected native command");
                assert_eq!(message["method"], expected.method);
                for (name, value) in expected.params.as_object().unwrap() {
                    assert_eq!(&message["params"][name], value, "{} parameter {name}", expected.method);
                }
                if let Some(hook) = expected.hook.take() { hook(); }
                let Some(mut response) = expected.reply else {
                    assert!(request(&mut socket).is_none(), "interrupted command was replayed or followed by more work");
                    closed = true;
                    break;
                };
                response["id"] = message["id"].clone();
                if let Some(session) = message.get("sessionId") {
                    response["sessionId"] = session.clone();
                }
                reply(&mut socket, &response);
                if let Some(hook) = expected.after_reply.take() { hook(); }
            }
            if !closed {
                assert!(request(&mut socket).is_none(), "unexpected extra command on a completed connection");
            }
        }
    });
    (format!("http://{address}"), worker)
}

fn connection(first: bool, targets: &[&str]) -> Vec<Step> {
    let mut script = Vec::new();
    if first {
        script.push(step("Browser.setDownloadBehavior", json!({"behavior":"deny","eventsEnabled":true}), json!({})));
    }
    let pages: Vec<_> = targets.iter().map(|target| json!({
        "targetId":target, "type":"page", "url":"about:blank", "title":target,
    })).collect();
    script.push(step("Target.getTargets", json!({}), json!({"targetInfos":pages})));
    script
}

fn attach(target: &str) -> Step {
    step("Target.attachToTarget", json!({"targetId":target,"flatten":true}), json!({"sessionId":format!("session-{target}")}))
}

fn open(first: bool, targets: &[&str], target: &str) -> Vec<Step> {
    let mut script = connection(first, targets);
    script.push(step("Target.createTarget", json!({"url":"about:blank"}), json!({"targetId":target})));
    script.push(attach(target));
    script.push(step("Page.enable", json!({}), json!({})));
    script.push(step("Page.setLifecycleEventsEnabled", json!({"enabled":true}), json!({})));
    script.push(step("Page.navigate", json!({"url":"about:blank"}), json!({})));
    script.push(step("Runtime.evaluate", json!({"expression":"({url: location.href, title: document.title})"}), json!({
        "result":{"type":"object","value":{"url":"about:blank","title":target}},
    })));
    script
}

fn evaluate(first: bool, targets: &[&str], target: &str, expression: &str, value: Value) -> Vec<Step> {
    let mut script = connection(first, targets);
    script.push(attach(target));
    script.push(step("Runtime.evaluate", json!({"expression":expression}), json!({
        "result":{"type":"object","value":value},
    })));
    script
}

fn registry(path: &std::path::Path, endpoint: &str) -> ToolRegistry {
    ToolRegistry::from_tools(vec![Box::new(BrowserTool::new(path).with_mock(false).with_cdp_endpoint(endpoint))])
}

#[test]
fn native_aliases_restore_across_sessions_and_release_does_not_close_external_pages() {
    let both = &["page-a", "page-b"];
    let (endpoint, worker) = peer(vec![
        open(true, &[], "page-a"),
        connection(true, &["page-a"]), // New B must not inherit A's active tab.
        open(false, &["page-a"], "page-b"),
        evaluate(false, both, "page-a", "41 + 1", json!(42)),
        evaluate(false, both, "page-a", "41 + 1", json!(42)),
        connection(true, both), // Released B forgot its alias, not its external page.
    ]);
    let dir = tempfile::tempdir().unwrap();
    let registry = registry(dir.path(), &endpoint);
    let scope = registry.job_session_scope();
    let tool = registry.get("browser").unwrap();
    run(async {
        select(&scope, "owner-a");
        call(tool, json!({"action":"open","tab":"work","url":"about:blank"})).await;
        select(&scope, "owner-b");
        let error = tool.execute("no-active-b", json!({"action":"evaluate","script":"41 + 1"}), None).await.unwrap_err();
        assert!(error.to_string().contains("nonexistent tab"));
        call(tool, json!({"action":"open","tab":"work","url":"about:blank"})).await;
        select(&scope, "owner-a");
        let restored = call(tool, json!({"action":"evaluate","script":"41 + 1"})).await;
        assert_eq!(restored.details.as_ref().unwrap()["result"], 42);
        assert_eq!(restored.details.unwrap()["browserSession"]["reused"], true);
        select(&scope, "owner-b");
        assert_eq!(call(tool, json!({"action":"release"})).await.details.unwrap()["released"], true);
        select(&scope, "owner-a");
        assert_eq!(call(tool, json!({"action":"evaluate","script":"41 + 1"})).await.details.unwrap()["result"], 42);
        select(&scope, "owner-b");
        assert!(tool.execute("released-b", json!({"action":"evaluate","script":"41 + 1"}), None).await.unwrap_err().to_string().contains("nonexistent tab"));
    });
    worker.join().unwrap();
}

#[test]
fn quiet_native_operation_is_interrupted_on_switch_and_original_namespace_is_reusable() {
    for unavailable in [false, true] {
        let scope = JobSessionScope::fixed("owner-a");
        let changed = scope.clone();
        let mut stalled = evaluate(false, &["page-a"], "page-a", "new Promise(() => {})", Value::Null);
        let last = stalled.last_mut().unwrap();
        last.reply = None;
        last.hook = Some(Box::new(move || {
            if unavailable {
                changed.bind(Arc::new(|| Box::pin(async { None })));
            } else {
                select(&changed, "owner-b");
            }
        }));
        let (endpoint, worker) = peer(vec![
            open(true, &[], "page-a"), stalled,
            evaluate(false, &["page-a"], "page-a", "6 * 7", json!(42)),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let mut tool = BrowserTool::new(dir.path()).with_mock(false).with_cdp_endpoint(endpoint);
        tool.bind_job_session_scope(scope.clone());
        run(async {
            call(&tool, json!({"action":"open","tab":"work","url":"about:blank"})).await;
            let error = tool.execute("stalled", json!({"action":"evaluate","script":"new Promise(() => {})","timeout_ms":3000}), None).await.unwrap_err();
            assert!(error.to_string().contains(if unavailable { "BROWSER_SESSION_UNAVAILABLE" } else { "BROWSER_SESSION_CHANGED" }));
            assert!(tool.sessions.execution.try_lock().is_some());
            assert_eq!(tool.sessions.namespaces.lock().unwrap().len(), 1);
            select(&scope, "owner-a");
            assert_eq!(call(&tool, json!({"action":"evaluate","script":"6 * 7"})).await.details.unwrap()["result"], 42);
        });
        worker.join().unwrap();
    }
}

#[test]
fn result_time_switch_never_publishes_the_previous_conversations_payload() {
    let scope = JobSessionScope::fixed("owner-a");
    let changed = scope.clone();
    let (sent, received) = futures::channel::oneshot::channel::<String>();
    let new_identity = received.map(|result| result.ok()).shared();
    let mut completed = evaluate(false, &["page-a"], "page-a", "privateData", json!("PRIVATE-CANARY"));
    // Block any ownership lookup racing this reply until the whole frame was
    // written. Then expose B before publication. This avoids racing intentional
    // connection retirement against the fixture's own response write.
    completed.last_mut().unwrap().hook = Some(Box::new(move || {
        changed.bind(Arc::new(move || Box::pin(new_identity.clone())));
    }));
    completed.last_mut().unwrap().after_reply = Some(Box::new(move || {
        sent.send("owner-b".into()).unwrap();
    }));
    let (endpoint, worker) = peer(vec![open(true, &[], "page-a"), completed]);
    let dir = tempfile::tempdir().unwrap();
    let mut tool = BrowserTool::new(dir.path()).with_mock(false).with_cdp_endpoint(endpoint);
    tool.bind_job_session_scope(scope.clone());
    run(async {
        call(&tool, json!({"action":"open","tab":"work","url":"about:blank"})).await;
        let error = tool.execute("changed-result", json!({"action":"evaluate","script":"privateData"}), None).await.unwrap_err().to_string();
        assert!(error.contains("BROWSER_SESSION_CHANGED"));
        assert!(!error.contains("PRIVATE-CANARY"));
        assert_eq!(call(&tool, json!({"action":"release"})).await.details.unwrap()["released"], false);
        select(&scope, "owner-a");
        assert_eq!(call(&tool, json!({"action":"release"})).await.details.unwrap()["released"], true);
    });
    worker.join().unwrap();
}

#[test]
fn cancelling_native_work_retires_its_connection_without_poisoning_the_owner_queue() {
    let scope = JobSessionScope::fixed("owner-a");
    let owner = AgentCx::for_request();
    let cancel = owner.clone();
    let mut stalled = evaluate(false, &["page-a"], "page-a", "pending", Value::Null);
    stalled.last_mut().unwrap().reply = None;
    stalled.last_mut().unwrap().hook = Some(Box::new(move || cancel.set_cancel_requested(true)));
    let (endpoint, worker) = peer(vec![
        open(true, &[], "page-a"), stalled,
        evaluate(false, &["page-a"], "page-a", "recovered", json!(true)),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let mut tool = BrowserTool::new(dir.path()).with_mock(false).with_cdp_endpoint(endpoint);
    tool.bind_job_session_scope(scope);
    run(async {
        call(&tool, json!({"action":"open","tab":"work","url":"about:blank"})).await;
        let error = owner.with_current(tool.execute("cancel", json!({"action":"evaluate","script":"pending"}), None)).await.unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        let fresh = AgentCx::for_request();
        let recovered = fresh.with_current(tool.execute("recover", json!({"action":"evaluate","script":"recovered"}), None)).await.unwrap();
        assert_eq!(recovered.details.unwrap()["result"], true);
    });
    worker.join().unwrap();
}

#[test]
fn dropping_native_future_releases_both_leases_and_preserves_its_original_mapping() {
    let (entered, arrived) = futures::channel::oneshot::channel::<()>();
    let mut stalled = evaluate(false, &["page-a"], "page-a", "pending", Value::Null);
    stalled.last_mut().unwrap().reply = None;
    stalled.last_mut().unwrap().hook = Some(Box::new(move || { entered.send(()).unwrap(); }));
    let (endpoint, worker) = peer(vec![
        open(true, &[], "page-a"), stalled,
        evaluate(false, &["page-a"], "page-a", "afterDrop", json!(84)),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(dir.path()).with_mock(false).with_cdp_endpoint(endpoint);
    run(async {
        call(&tool, json!({"action":"open","tab":"work","url":"about:blank"})).await;
        let pending = tool.execute("drop", json!({"action":"evaluate","script":"pending"}), None);
        match futures::future::select(pending, arrived).await {
            futures::future::Either::Right((Ok(()), pending)) => drop(pending),
            _ => panic!("native request must be awaiting its withheld reply"),
        }
        assert!(tool.sessions.execution.try_lock().is_some());
        assert_eq!(call(&tool, json!({"action":"evaluate","script":"afterDrop"})).await.details.unwrap()["result"], 84);
    });
    worker.join().unwrap();
}

#[test]
#[ignore = "requires an installed Chromium and explicit DSR live-browser lane"]
fn live_managed_browsers_keep_two_profiles_and_stopping_one_preserves_the_other() {
    let dir = tempfile::tempdir().unwrap();
    let scope = JobSessionScope::fixed("owner-a");
    let mut tool = BrowserTool::new(dir.path()).with_mock(false)
        .with_launch_options(crate::browser::BrowserLaunchOptions::default());
    tool.bind_job_session_scope(scope.clone());
    run(async {
        call(&tool, json!({"action":"open","tab":"work","url":"about:blank"})).await;
        call(&tool, json!({"action":"evaluate","script":"globalThis.ownerMarker = 41"})).await;
        let a = call(&tool, json!({"action":"status"})).await.details.unwrap()["process_id"].as_u64().unwrap();
        select(&scope, "owner-b");
        call(&tool, json!({"action":"open","tab":"work","url":"about:blank"})).await;
        call(&tool, json!({"action":"evaluate","script":"globalThis.ownerMarker = 99"})).await;
        let b = call(&tool, json!({"action":"status"})).await.details.unwrap()["process_id"].as_u64().unwrap();
        assert_ne!(a, b);
        select(&scope, "owner-a");
        assert_eq!(call(&tool, json!({"action":"evaluate","script":"ownerMarker"})).await.details.unwrap()["result"], 41);
        select(&scope, "owner-b");
        assert_eq!(call(&tool, json!({"action":"evaluate","script":"ownerMarker"})).await.details.unwrap()["result"], 99);
        assert_eq!(call(&tool, json!({"action":"stop"})).await.details.unwrap()["running"], false);
        select(&scope, "owner-a");
        let status = call(&tool, json!({"action":"status"})).await;
        assert_eq!(status.details.as_ref().unwrap()["running"], true);
        assert_eq!(status.details.unwrap()["process_id"], a);
        assert_eq!(call(&tool, json!({"action":"evaluate","script":"ownerMarker"})).await.details.unwrap()["result"], 41);
        call(&tool, json!({"action":"release"})).await;
    });
}
