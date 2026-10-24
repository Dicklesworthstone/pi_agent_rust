//! Existing dispatch and live-adapter coverage, using the current transport.
use super::*;

fn runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::new()
        .enable_parking(false)
        .worker_threads(1)
        .blocking_threads(1, 8)
        .build()
        .expect("runtime")
}

#[test]
fn dispatch_routes_responses_by_request_seq() {
    let pending: PendingMap = Mutex::new(HashMap::new());
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    lock(&pending).insert(7, tx);
    let (events, _rx) = std::sync::mpsc::sync_channel(4);
    let tail = Mutex::new(crate::lsp::jsonrpc::PublicTailBuffer::new());
    let writer = Mutex::new(Vec::<u8>::new());
    assert!(dispatch(
        &serde_json::json!({
            "seq":9,"type":"response","request_seq":7,"success":true,"command":"stackTrace","body":{"stackFrames":[]}
        }),
        &pending,
        &events,
        &tail,
        &writer,
        &AtomicU64::new(100)
    ));
    assert!(
        rx.try_recv()
            .expect("completed")
            .expect("ok")
            .get("stackFrames")
            .is_some()
    );
}

#[test]
fn dispatch_surfaces_adapter_failures() {
    let pending: PendingMap = Mutex::new(HashMap::new());
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    lock(&pending).insert(3, tx);
    let (events, _rx) = std::sync::mpsc::sync_channel(4);
    let tail = Mutex::new(crate::lsp::jsonrpc::PublicTailBuffer::new());
    assert!(dispatch(
        &serde_json::json!({
            "seq":4,"type":"response","request_seq":3,"success":false,"command":"evaluate","message":"cannot evaluate while running"
        }),
        &pending,
        &events,
        &tail,
        &Mutex::new(Vec::<u8>::new()),
        &AtomicU64::new(100)
    ));
    let error = rx.try_recv().expect("completed").expect_err("error");
    assert_eq!(error.code(), "DAP_ADAPTER_ERROR");
    assert!(error.message().contains("cannot evaluate while running"));
}

#[test]
fn dispatch_queues_events_and_captures_output() {
    let pending: PendingMap = Mutex::new(HashMap::new());
    let (events, rx) = std::sync::mpsc::sync_channel(4);
    let tail = Mutex::new(crate::lsp::jsonrpc::PublicTailBuffer::new());
    let writer = Mutex::new(Vec::<u8>::new());
    let sequence = AtomicU64::new(100);
    assert!(dispatch(
        &serde_json::json!({"seq":5,"type":"event","event":"stopped","body":{"threadId":42,"reason":"breakpoint"}}),
        &pending,
        &events,
        &tail,
        &writer,
        &sequence
    ));
    assert_eq!(rx.try_recv().expect("event").body["threadId"], 42);
    assert!(dispatch(
        &serde_json::json!({"seq":6,"type":"event","event":"output","body":{"output":"hello\n"}}),
        &pending,
        &events,
        &tail,
        &writer,
        &sequence
    ));
    assert!(lock(&tail).tail().contains("hello"));
    assert!(
        rx.try_recv().is_err(),
        "output belongs in the bounded tail, not the control lane"
    );
}

#[test]
fn dispatch_declines_reverse_requests() {
    let pending: PendingMap = Mutex::new(HashMap::new());
    let (events, _rx) = std::sync::mpsc::sync_channel(4);
    let tail = Mutex::new(crate::lsp::jsonrpc::PublicTailBuffer::new());
    let writer = Mutex::new(Vec::<u8>::new());
    assert!(dispatch(
        &serde_json::json!({"seq":55,"type":"request","command":"runInTerminal","arguments":{"args":["/bin/true"]}}),
        &pending,
        &events,
        &tail,
        &writer,
        &AtomicU64::new(100)
    ));
    let bytes = lock(&writer).clone();
    let mut reader = std::io::BufReader::new(bytes.as_slice());
    let frame = crate::lsp::jsonrpc::read_frame(&mut reader)
        .unwrap()
        .unwrap();
    assert_eq!(frame["request_seq"], 55);
    assert_eq!(frame["type"], "response");
    assert_eq!(frame["command"], "runInTerminal");
    assert_eq!(frame["success"], false);
}

#[cfg(windows)]
#[test]
fn windows_stdio_adapter_accepts_input_after_spawn_returns() {
    let temp = tempfile::tempdir().unwrap();
    let transport = DapTransport::spawn(
        "cmd.exe",
        &[
            "/D".into(),
            "/Q".into(),
            "/C".into(),
            "set /P first=&echo pi-dap-stdio-ack 1>&2&set /P second=".into(),
        ],
        &[],
        temp.path(),
    )
    .expect("spawn contained stdio adapter");

    // The adapter cannot acknowledge before this post-spawn input arrives.
    // Replacing the Job inside spawn_inner kills it before the handshake,
    // even if it was scheduled before its parent returned from CreateProcess.
    assert!(
        transport
            .writer
            .try_send(Frame {
                bytes: b"probe\r\n".to_vec(),
                phase: Arc::new(AtomicU8::new(QUEUED)),
            })
            .is_ok(),
        "adapter writer must accept the startup probe"
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if transport.stderr_tail().contains("pi-dap-stdio-ack") {
            break;
        }
        assert!(
            transport.is_alive(),
            "adapter died before acknowledging stdin"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "adapter did not acknowledge its piped stdin"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(transport.is_alive());
    transport.kill();
    assert!(!transport.is_alive());
}

fn lldb() -> Option<String> {
    super::super::adapters::default_adapters()
        .into_iter()
        .find(|adapter| adapter.id == "lldb-dap")
        .and_then(|adapter| adapter.resolve_command())
}

#[test]
fn live_lldb_dap_initialize_round_trip() {
    let Some(command) = lldb() else {
        eprintln!("skip: no lldb-dap on this host");
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let transport = DapTransport::spawn(&command, &[], &[], temp.path()).expect("spawn lldb-dap");
    let runtime = runtime();
    let body = runtime
        .block_on(transport.request(
            "initialize",
            serde_json::json!({
                "clientID":"t","adapterID":"pi-dap","pathFormat":"path"
            }),
            Duration::from_secs(10),
        ))
        .expect("initialize");
    assert!(
        body.get("supportsConfigurationDoneRequest").is_some(),
        "{body}"
    );
    // Unsupported configuration at this stage may be an adapter error, but
    // the response must be correlated rather than silently stalling.
    let second = runtime.block_on(transport.request(
        "configurationDone",
        serde_json::json!({}),
        Duration::from_secs(5),
    ));
    assert!(
        matches!(second, Ok(_) | Err(DapError::Adapter { .. })),
        "{second:?}"
    );
    transport.kill();
}

#[test]
#[ignore = "existing lldb-dap launch pacing stall; the debugpy and framed protocol lanes remain active"]
fn live_lldb_dap_launch_fixture() {
    let Some(command) = lldb() else {
        eprintln!("skip: no lldb-dap");
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("fx.c");
    let binary = temp.path().join("fx");
    std::fs::write(
        &source,
        "#include <unistd.h>\nint main(void) { usleep(400000); return 0; }\n",
    )
    .unwrap();
    let status = std::process::Command::new("cc")
        .args(["-g", "-O0"])
        .arg(&source)
        .arg("-o")
        .arg(&binary)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("cc");
    if !status.success() {
        eprintln!("skip: cc failed");
        return;
    }
    let env: Vec<_> = std::env::vars()
        .filter(|(key, _)| {
            !matches!(
                key.as_str(),
                "LD_LIBRARY_PATH" | "DYLD_LIBRARY_PATH" | "DYLD_FALLBACK_LIBRARY_PATH"
            )
        })
        .collect();
    let transport = DapTransport::spawn_with_env(&command, &[], &env, temp.path()).expect("spawn");
    let runtime = runtime();
    runtime
        .block_on(transport.request(
            "initialize",
            serde_json::json!({
                "clientID":"t","adapterID":"pi-dap","pathFormat":"path"
            }),
            Duration::from_secs(10),
        ))
        .expect("initialize");
    std::thread::sleep(Duration::from_millis(400));
    let result = runtime.block_on(transport.request("launch", serde_json::json!({
        "program":binary.display().to_string(),"args":[],"cwd":temp.path().display().to_string(),
        "console":"internalConsole","stopOnEntry":false
    }), Duration::from_secs(10)));
    let tail = transport.stderr_tail();
    let events = transport.drain_events();
    transport.kill();
    assert!(
        result.is_ok(),
        "launch: {result:?}; tail: {tail}; events: {events:?}"
    );
}

const FAKE_ADAPTER_PY: &str = r#"
import json, sys

def read_frame():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line: return None
        line = line.strip()
        if not line: break
        k, v = line.split(b":", 1)
        headers[k.strip().lower()] = v.strip()
    return json.loads(sys.stdin.buffer.read(int(headers[b"content-length"])))

def send(msg):
    body = json.dumps(msg).encode()
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
    sys.stdout.buffer.flush()

while True:
    frame = read_frame()
    if frame is None: break
    if frame.get("type") != "request": continue
    cmd, seq = frame["command"], frame["seq"]
    sys.stderr.write("fake-dap: got %s seq=%s\n" % (cmd, seq))
    sys.stderr.flush()
    if cmd == "initialize":
        send({"seq":1,"type":"response","request_seq":seq,"command":cmd,"success":True,"body":{"supportsConfigurationDoneRequest":True}})
    elif cmd == "launch":
        send({"seq":2,"type":"response","request_seq":seq,"command":cmd,"success":True})
        send({"seq":3,"type":"event","event":"process","body":{"name":"fx","isLocalProcess":True,"startMethod":"launch","systemProcessId":4242}})
        send({"seq":4,"type":"event","event":"initialized","body":{}})
    else:
        send({"seq":6,"type":"response","request_seq":seq,"command":cmd,"success":True,"body":{}})
"#;

#[test]
fn fake_adapter_full_handshake() {
    if !std::process::Command::new("python3")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
    {
        eprintln!("skip: no python3");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("fake_dap.py");
    std::fs::write(&script, FAKE_ADAPTER_PY).unwrap();
    let transport =
        DapTransport::spawn("python3", &[script.display().to_string()], &[], temp.path()).unwrap();
    let runtime = runtime();
    runtime
        .block_on(transport.request(
            "initialize",
            serde_json::json!({"clientID":"t","adapterID":"pi-dap"}),
            Duration::from_secs(5),
        ))
        .unwrap();
    runtime
        .block_on(transport.request(
            "launch",
            serde_json::json!({"program":"/tmp/fx"}),
            Duration::from_secs(5),
        ))
        .unwrap();
    // A response and the subsequent initialized event are separate frames.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut events = Vec::new();
    while !events
        .iter()
        .any(|event: &DapEvent| event.event == "initialized")
        && std::time::Instant::now() < deadline
    {
        events.extend(transport.drain_events());
        std::thread::sleep(Duration::from_millis(5));
    }
    transport.kill();
    assert!(
        events.iter().any(|event| event.event == "initialized"),
        "{events:?}"
    );
}

#[cfg(unix)]
#[test]
fn wire_bytes_capture() {
    let temp = tempfile::tempdir().unwrap();
    let capture = temp.path().join("captured.bin");
    let transport = DapTransport::spawn(
        "sh",
        &[
            "-c".into(),
            "cat > \"$1\"".into(),
            "pi-dap-fixture".into(),
            capture.display().to_string(),
        ],
        &[],
        temp.path(),
    )
    .expect("spawn capture");
    let runtime = runtime();
    let one = runtime.block_on(transport.request(
        "initialize",
        serde_json::json!({"clientID":"t","adapterID":"pi-dap","pathFormat":"path"}),
        Duration::from_millis(300),
    ));
    let two = runtime.block_on(transport.request("launch", serde_json::json!({"program":"/tmp/fx","args":[],"cwd":"/tmp","console":"internalConsole","stopOnEntry":false}), Duration::from_millis(300)));
    assert!(one.is_err() && two.is_err());
    transport.kill();
    std::thread::sleep(Duration::from_millis(200));
    let bytes = std::fs::read(capture).unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("Content-Length:"));
    assert!(text.contains("\"command\":\"initialize\""));
    assert!(text.contains("\"command\":\"launch\""));
}
