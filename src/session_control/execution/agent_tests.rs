//! Exercise the ownership wrapper through the real SDK, agent and wire adapter.

use super::*;
use crate::agent::{Agent, AgentConfig, AgentEvent, AgentSession};
use crate::compaction::ResolvedCompactionSettings;
use crate::model::StopReason;
use crate::provider::StreamOptions;
use crate::providers::openai::OpenAIProvider;
use crate::sdk::{AgentSessionHandle, EventListeners};
use crate::session::Session;
use crate::session_control::ControllableSession;
use crate::tools::ToolRegistry;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

fn session(cwd: &std::path::Path, base_url: &str) -> ControllableSession {
    let provider = Arc::new(OpenAIProvider::new("ownership-fixture").with_base_url(base_url));
    let agent = Agent::new(
        provider,
        ToolRegistry::new(&[], cwd, None),
        AgentConfig {
            model_accepts_images: true,
            stream_options: StreamOptions {
                api_key: Some("ownership-fixture-key".to_string()),
                ..StreamOptions::default()
            },
            ..AgentConfig::default()
        },
    );
    let session = AgentSession::new(
        agent,
        Arc::new(asupersync::sync::Mutex::new(Session::in_memory())),
        false,
        ResolvedCompactionSettings::default(),
    );
    AgentSessionHandle::from_session_with_listeners(session, EventListeners::default())
        .into_controllable()
}

#[test]
fn cancelled_sdk_prompt_and_continuation_never_mutate_history_or_emit_events() {
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    let owner = runtime.request_cx_with_budget(Budget::new());
    let temp = tempfile::tempdir().unwrap();
    let mut session = session(temp.path(), "http://127.0.0.1:9/v1");
    let events = Arc::new(AtomicUsize::new(0));
    runtime.block_on(Box::pin(async {
        let parent = Cx::current().unwrap();
        let store = session.session_mut().session_store();
        let before = serde_json::to_value(
            store.lock(&parent).await.unwrap().to_messages_for_current_path(),
        ).unwrap();
        let captured = Arc::clone(&events);
        let turn = {
            let _current = owner.clone().set_current_restricted();
            session.prompt("must not enter history".to_string(), move |_| {
                captured.fetch_add(1, Ordering::SeqCst);
            }).unwrap()
        };
        let control = turn.control();
        control.steer("recoverable correction").unwrap();
        cancel(&owner);
        assert!(turn.await.unwrap_err().to_string().contains("SESSION_CONTROL_CANCELLED"));
        assert!(control.snapshot().finished);
        assert_eq!(control.take_pending()[0].text, "recoverable correction");

        let captured = Arc::clone(&events);
        let continuation = {
            let _current = owner.clone().set_current_restricted();
            session.continue_turn(move |_| {
                captured.fetch_add(1, Ordering::SeqCst);
            }).unwrap()
        };
        assert!(continuation.await.unwrap_err().to_string().contains("SESSION_CONTROL_CANCELLED"));
        let after = serde_json::to_value(
            store.lock(&parent).await.unwrap().to_messages_for_current_path(),
        ).unwrap();
        assert_eq!(before, after);
        assert_eq!(events.load(Ordering::SeqCst), 0);
        assert!(!parent.is_cancel_requested());
    }));
}

#[test]
fn explicit_sdk_abort_before_start_prevents_prompt_installation() {
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let mut session = session(temp.path(), "http://127.0.0.1:9/v1");
    runtime.block_on(Box::pin(async {
        let turn = session.prompt("never installed".to_string(), |_| {
            panic!("unused aborted prompt must not emit a native event");
        }).unwrap();
        assert!(turn.control().abort());
        assert!(turn.await.unwrap_err().to_string().contains("SESSION_CONTROL_CANCELLED"));
        let store = session.session_mut().session_store();
        let cx = AgentCx::for_current_or_request();
        assert!(store.lock(cx.cx()).await.unwrap().to_messages_for_current_path().is_empty());
    }));
}

#[test]
fn cancelling_initiator_stops_an_idle_wire_stream_without_cancelling_poller() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let observed_close = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&observed_close);
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = Arc::clone(&stop);
    let peer = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(4);
        let mut socket = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if stopping.load(Ordering::SeqCst) || Instant::now() >= deadline {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("fixture accept: {error}"),
            }
        };
        socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        socket.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut reader = BufReader::new(socket.try_clone().unwrap());
        let mut length = None;
        let mut total = 0;
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).unwrap() > 0);
            total += line.len();
            assert!(total <= 64 * 1024);
            if line == "\r\n" { break; }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length") {
                length = Some(value.trim().parse::<usize>().unwrap());
            }
        }
        let length = length.unwrap();
        assert!(length <= 1024 * 1024);
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(!body.to_string().contains("unclaimed follow-up"));
        let chunk = format!("data: {}\n\n", serde_json::json!({
            "id":"owned-stream", "object":"chat.completion.chunk",
            "created":0, "model":"ownership-fixture",
            "choices":[{"index":0,"delta":{"role":"assistant","content":"partial"},"finish_reason":null}]
        }));
        write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{chunk}\r\n", chunk.len()).unwrap();
        socket.flush().unwrap();
        socket.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut byte = [0_u8; 1];
        while Instant::now() < deadline && !stopping.load(Ordering::SeqCst) {
            match socket.read(&mut byte) {
                Ok(0) => { observed.store(true, Ordering::SeqCst); return; }
                Err(error) if matches!(error.kind(), std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted) => {
                    observed.store(true, Ordering::SeqCst);
                    return;
                }
                Err(error) if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                result => panic!("unexpected fixture read: {result:?}"),
            }
        }
        // A missing cancellation wake becomes a finite, observable stream EOF,
        // not a hung test. It must not satisfy the early-peer-close assertion.
    });
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    let owner = runtime.request_cx_with_budget(Budget::new());
    let cancelling = owner.clone();
    let (started, received) = std::sync::mpsc::channel();
    let canceller = std::thread::spawn(move || {
        if received.recv_timeout(Duration::from_secs(3)).is_ok() {
            cancel(&cancelling);
        }
    });
    let temp = tempfile::tempdir().unwrap();
    let mut session = session(temp.path(), &endpoint);
    runtime.block_on(Box::pin(async {
        let parent = Cx::current().unwrap();
        let turn = {
            let _current = owner.clone().set_current_restricted();
            session.prompt("begin idle stream".to_string(), move |event| {
                if matches!(event, AgentEvent::MessageUpdate { .. }) {
                    let _ = started.send(());
                }
            }).unwrap()
        };
        let control = turn.control();
        control.follow_up("unclaimed follow-up").unwrap();
        let completion = turn.await;
        assert!(completion.is_err() || completion.as_ref().is_ok_and(|message| matches!(message.stop_reason, StopReason::Aborted | StopReason::Error)));
        assert!(control.snapshot().finished);
        assert_eq!(control.snapshot().handed_to_agent, 0);
        assert_eq!(control.take_pending()[0].text, "unclaimed follow-up");
        assert!(!parent.is_cancel_requested());
    }));
    canceller.join().unwrap();
    // Join before setting stop so the fixture actually observes socket closure.
    peer.join().unwrap();
    stop.store(true, Ordering::SeqCst);
    assert!(observed_close.load(Ordering::SeqCst), "owner cancellation must close the stream before fixture EOF");
}
