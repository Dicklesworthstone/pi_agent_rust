use super::*;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize};

fn run<F: std::future::Future>(future: F) -> F::Output {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .unwrap()
        .block_on(future)
}

fn pair() -> (Arc<DapTransport>, TcpStream) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (server, _) = listener.accept().unwrap();
    server
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    server
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let transport = DapTransport::from_lanes(
        None,
        Box::new(client.try_clone().unwrap()),
        Box::new(client.try_clone().unwrap()),
        Some(client),
        Arc::new(Mutex::new(crate::lsp::jsonrpc::PublicTailBuffer::new())),
    );
    (Arc::new(transport), server)
}

// Independent bounded peer decoder; BufReader retains coalesced next frames.
fn read(peer: &mut BufReader<TcpStream>) -> Value {
    let mut length = None;
    let mut bytes = 0;
    loop {
        let mut line = String::new();
        assert!(
            Read::by_ref(peer).take(4097).read_line(&mut line).unwrap() > 0,
            "unexpected EOF"
        );
        bytes += line.len();
        assert!(bytes <= 4096);
        if line == "\r\n" {
            break;
        }
        let (name, value) = line.split_once(':').unwrap();
        if name.eq_ignore_ascii_case("Content-Length") {
            assert!(length.is_none());
            length = Some(value.trim().parse::<usize>().unwrap());
        }
    }
    let length = length.unwrap();
    assert!((1..=2 * 1024 * 1024).contains(&length));
    let mut body = vec![0; length];
    peer.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
}

fn send(peer: &mut BufReader<TcpStream>, message: Value) {
    let body = serde_json::to_vec(&message).unwrap();
    write!(peer.get_mut(), "Content-Length: {}\r\n\r\n", body.len()).unwrap();
    peer.get_mut().write_all(&body).unwrap();
    peer.get_mut().flush().unwrap();
}

fn answer(peer: &mut BufReader<TcpStream>, request: &Value) {
    send(
        peer,
        json!({"seq":200,"type":"response","request_seq":request["seq"],
        "command":request["command"],"success":true,"body":{"ok":true}}),
    );
}

fn reverse(sequence: u64) -> Value {
    json!({"seq":sequence,"type":"request","command":"hostProbe","arguments":{"marker":"ready"}})
}

#[test]
fn asynchronous_reverse_callback_can_reenter_the_same_native_transport() {
    let (transport, peer) = pair();
    let server = std::thread::spawn(move || {
        let mut peer = BufReader::new(peer);
        let launch = read(&mut peer);
        send(&mut peer, reverse(101));
        // Launch cannot finish until a host callback makes another DAP call.
        let nested = read(&mut peer);
        assert_eq!(nested["command"], "nestedProbe");
        answer(&mut peer, &nested);
        let response = read(&mut peer);
        assert_eq!(response["request_seq"], 101);
        assert_eq!(response["success"], true);
        assert_eq!(response["body"]["nested"], true);
        send(
            &mut peer,
            json!({"seq":201,"type":"event","event":"output","body":{"output":"host-completed\n"}}),
        );
        answer(&mut peer, &launch);
    });
    let thread = std::thread::current().id();
    let called = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&called);
    let nested = Arc::clone(&transport);
    let handler: ReverseRequestHandler = Arc::new(move |command, args| {
        let nested = Arc::clone(&nested);
        let count = Arc::clone(&count);
        Box::pin(async move {
            assert_eq!(
                std::thread::current().id(),
                thread,
                "host ran on the reader thread"
            );
            assert_eq!(command, "hostProbe");
            assert_eq!(args["marker"], "ready");
            count.fetch_add(1, Ordering::SeqCst);
            let reply = nested
                .request("nestedProbe", json!({}), Duration::from_secs(2))
                .await
                .map_err(|error| error.to_string())?;
            Ok(json!({"nested":reply["ok"]}))
        })
    });
    let result =
        run(transport.request_with_reverse("launch", json!({}), Duration::from_secs(3), handler))
            .unwrap();
    assert_eq!(result["ok"], true);
    assert_eq!(called.load(Ordering::SeqCst), 1);
    assert!(lock(&transport.life.reverse.active).is_none());
    server.join().unwrap();
    assert!(transport.stderr_tail().contains("host-completed"));
}

struct Dropped(Arc<AtomicBool>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[test]
fn cancellation_drops_active_host_work_and_next_request_has_no_inherited_handler() {
    let (transport, peer) = pair();
    let server = std::thread::spawn(move || {
        let mut peer = BufReader::new(peer);
        let _launch = read(&mut peer);
        send(&mut peer, reverse(101));
        let refused = read(&mut peer);
        assert_eq!(refused["request_seq"], 101);
        assert_eq!(refused["success"], false);
        let next = read(&mut peer);
        assert_eq!(next["command"], "next");
        send(&mut peer, reverse(102));
        let default_refusal = read(&mut peer);
        assert_eq!(default_refusal["request_seq"], 102);
        assert_eq!(default_refusal["success"], false);
        answer(&mut peer, &next);
    });
    let started = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));
    let handler: ReverseRequestHandler = {
        let started = Arc::clone(&started);
        let dropped = Arc::clone(&dropped);
        Arc::new(move |_, _| {
            let started = Arc::clone(&started);
            let dropped = Arc::clone(&dropped);
            Box::pin(async move {
                let _guard = Dropped(dropped);
                started.store(true, Ordering::SeqCst);
                std::future::pending().await
            })
        })
    };
    let owner = AgentCx::for_request();
    run(async {
        let mut request = Box::pin(owner.with_current(transport.request_with_reverse(
            "launch",
            json!({}),
            Duration::from_secs(3),
            handler,
        )));
        let deadline = Instant::now() + Duration::from_secs(2);
        while !started.load(Ordering::SeqCst) {
            assert!(futures::poll!(request.as_mut()).is_pending());
            assert!(Instant::now() < deadline);
            AgentCx::for_current_or_request()
                .time()
                .sleep(Duration::from_millis(2))
                .await;
        }
        owner.set_cancel_requested(true);
        assert!(matches!(request.await, Err(DapError::Cancelled)));
        assert!(dropped.load(Ordering::SeqCst));
        assert!(lock(&transport.pending).is_empty());
        transport
            .request("next", json!({}), Duration::from_secs(2))
            .await
            .unwrap();
    });
    server.join().unwrap();
}

#[test]
fn timeout_retires_pending_host_callback_and_sends_no_success() {
    let (transport, peer) = pair();
    let server = std::thread::spawn(move || {
        let mut peer = BufReader::new(peer);
        read(&mut peer);
        send(&mut peer, reverse(101));
        let reply = read(&mut peer);
        assert_eq!(reply["request_seq"], 101);
        assert_eq!(reply["success"], false);
    });
    let dropped = Arc::new(AtomicBool::new(false));
    let capture = Arc::clone(&dropped);
    let handler: ReverseRequestHandler = Arc::new(move |_, _| {
        let guard = Dropped(Arc::clone(&capture));
        Box::pin(async move {
            let _guard = guard;
            std::future::pending().await
        })
    });
    let result = run(transport.request_with_reverse(
        "launch",
        json!({}),
        Duration::from_millis(200),
        handler,
    ));
    assert!(
        matches!(result, Err(DapError::Timeout { .. })),
        "{result:?}"
    );
    assert!(dropped.load(Ordering::SeqCst));
    assert!(lock(&transport.life.reverse.active).is_none());
    server.join().unwrap();
}

#[test]
fn admission_bounds_and_duplicate_ids_fail_without_invoking_a_host() {
    let state = State::default();
    assert_eq!(state.enqueue(&reverse(1)), None);
    let window = Arc::new(Window {
        queue: Mutex::new(Queue::default()),
    });
    *lock(&state.active) = Some(Arc::clone(&window));
    assert_eq!(state.enqueue(&reverse(1)), Some(true));
    assert_eq!(state.enqueue(&reverse(1)), Some(false));
    for sequence in 2..=8 {
        assert_eq!(state.enqueue(&reverse(sequence)), Some(true));
    }
    assert_eq!(state.enqueue(&reverse(9)), Some(false));
    lock(&window.queue).requests.clear();
    let mut oversized = reverse(10);
    oversized["arguments"] = json!({"data":"x".repeat(MAX_ARGUMENT_BYTES)});
    assert_eq!(state.enqueue(&oversized), Some(false));
    for sequence in 9..=32 {
        assert_eq!(state.enqueue(&reverse(sequence)), Some(true));
        lock(&window.queue).requests.clear();
    }
    assert_eq!(state.enqueue(&reverse(33)), Some(false));
    state.close();
    assert_eq!(state.enqueue(&reverse(34)), None);
}

#[test]
fn a_second_window_is_refused_and_dropped_queue_does_not_move_to_the_next_one() {
    let (transport, peer) = pair();
    let handler: ReverseRequestHandler = Arc::new(|_, _| Box::pin(async { Ok(json!({})) }));
    let permit = Permit::open(
        &transport,
        AgentCx::for_request(),
        Duration::from_secs(2),
        Arc::clone(&handler),
    )
    .unwrap();
    assert!(
        Permit::open(
            &transport,
            AgentCx::for_request(),
            Duration::from_secs(2),
            Arc::clone(&handler)
        )
        .is_err()
    );
    assert_eq!(transport.life.reverse.enqueue(&reverse(101)), Some(true));
    drop(permit);
    let mut peer = BufReader::new(peer);
    let refused = read(&mut peer);
    assert_eq!(refused["request_seq"], 101);
    assert_eq!(refused["success"], false);
    let next = Permit::open(
        &transport,
        AgentCx::for_request(),
        Duration::from_secs(2),
        handler,
    )
    .unwrap();
    assert!(lock(&next.window.queue).requests.is_empty());
}
