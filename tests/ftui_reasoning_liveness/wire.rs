//! Real HTTP/SSE -> SDK -> subscription -> Program -> ANSI presentation.
//! The peer advances only after a presented tail and a real editor round trip.
//! Deadlines diagnose nontermination; they are not performance thresholds.

use super::{DELTAS, phase};
use ftui::render::terminal_model::TerminalModel;
use ftui::runtime::{BackendEventSource, BackendFeatures, TerminalPresenter};
use ftui::{
    Event, KeyCode, KeyEvent, Program, ProgramConfig, TerminalCapabilities, TerminalWriter,
};
use pi::agent::{Agent, AgentConfig, AgentSession};
use pi::compaction::ResolvedCompactionSettings;
use pi::interactive::{PiMsg, conversation_from_session};
use pi::interactive_ftui::{PiFtuiModel, UiCommand, agent_event_to_pi_msgs};
use pi::model::{AssistantMessage, ContentBlock, Message, StopReason};
use pi::provider::StreamOptions;
use pi::providers::openai::OpenAIProvider;
use pi::sdk::{AgentSessionHandle, EventListeners};
use pi::session::Session;
use pi::session_control::SessionControlHandle;
use pi::tools::ToolRegistry;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io::{self, BufRead, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

const COLS: u16 = 47;
const ROWS: u16 = 16;
const BATCH: usize = 64;
const WAIT: Duration = Duration::from_secs(20);
type ControlSlot = Arc<Mutex<Option<SessionControlHandle>>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Complete,
    Resume,
    Abort,
}

fn delta(index: usize) -> String {
    format!("{}\nTAIL-{index:04}\n", "界".repeat(48))
}

fn expected_thinking() -> String {
    (0..DELTAS).map(delta).collect()
}

fn thinking(message: &AssistantMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Thinking(value) => Some(value.thinking.as_str()),
            _ => None,
        })
        .collect()
}

fn read_request(stream: &TcpStream) -> Value {
    stream
        .set_read_timeout(Some(WAIT))
        .expect("request timeout");
    let mut reader = io::BufReader::new(stream.try_clone().expect("request reader"));
    let mut header = Vec::new();
    let mut length = None;
    loop {
        let before = header.len();
        // Limit the reader itself, not just a post-allocation length check.
        let available = u64::try_from(64 * 1024 - header.len()).expect("header allowance");
        let n = reader
            .by_ref()
            .take(available)
            .read_until(b'\n', &mut header)
            .expect("HTTP request header");
        assert!(n > 0 && header.len() < 64 * 1024, "bounded HTTP header");
        let line = std::str::from_utf8(&header[before..]).expect("header UTF-8");
        if before == 0 {
            assert!(line.starts_with("POST "), "{line}");
        }
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            assert!(length.is_none(), "duplicate content length");
            length = Some(value.trim().parse::<usize>().expect("content length"));
        }
    }
    let length = length.expect("content-length request");
    assert!(length <= 4 * 1024 * 1024, "bounded request body");
    let mut body = vec![0; length];
    reader.read_exact(&mut body).expect("request body");
    serde_json::from_slice(&body).expect("request JSON")
}

fn request_has(request: &Value, role: &str, text: &str) -> bool {
    request["messages"]
        .as_array()
        .expect("request messages")
        .iter()
        .any(|message| {
            if message["role"] != role {
                return false;
            }
            match &message["content"] {
                Value::String(content) => content == text,
                Value::Array(parts) => {
                    let content: String = parts
                        .iter()
                        .filter(|part| part["type"] == "text")
                        .filter_map(|part| part["text"].as_str())
                        .collect();
                    content == text
                }
                _ => false,
            }
        })
}

fn accept(listener: &TcpListener) -> TcpStream {
    let deadline = Instant::now() + WAIT;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nodelay(true).expect("fixture TCP_NODELAY");
                stream.set_write_timeout(Some(WAIT)).expect("write timeout");
                return stream;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "waiting for provider request");
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("accept: {error}"),
        }
    }
}

fn begin_response(stream: &mut TcpStream) {
    stream
        .write_all(
            concat!(
                "HTTP/1.1 200 OK\r\n",
                "Content-Type: text/event-stream\r\n",
                "Transfer-Encoding: chunked\r\n",
                "Connection: close\r\n\r\n",
            )
            .as_bytes(),
        )
        .expect("SSE headers");
}

fn chunk(stream: &mut TcpStream, bytes: &[u8]) {
    write!(stream, "{:x}\r\n", bytes.len()).expect("chunk length");
    stream.write_all(bytes).expect("chunk payload");
    stream.write_all(b"\r\n").expect("chunk end");
}

fn event(stream: &mut TcpStream, value: Value, finish: Option<&str>) {
    let data = format!(
        "data: {}\n\n",
        json!({
            "id": "gh255-wire",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "gh255-wire",
            "choices": [{"index": 0, "delta": value, "finish_reason": finish}]
        })
    );
    // Split the HTTP body inside a UTF-8 character, independently of SSE
    // framing. No assumption that TCP writes become individual reads.
    let split = data
        .as_bytes()
        .windows(3)
        .position(|bytes| bytes == "界".as_bytes())
        .map_or(data.len() / 2, |index| index + 1);
    chunk(stream, &data.as_bytes()[..split]);
    chunk(stream, &data.as_bytes()[split..]);
    stream.flush().expect("flush SSE event");
}

fn finish(stream: &mut TcpStream, answer: &str) {
    event(stream, json!({"content": answer}), None);
    event(stream, json!({}), Some("stop"));
    chunk(stream, b"data: [DONE]\n\n");
    stream
        .write_all(b"0\r\n\r\n")
        .expect("end chunked response");
    stream.flush().expect("flush response end");
}

fn peer(listener: &TcpListener, ack: &mpsc::Receiver<usize>, abort: bool) {
    let mut first = accept(listener);
    assert!(request_has(
        &read_request(&first),
        "user",
        "reason carefully"
    ));
    begin_response(&mut first);
    event(&mut first, json!({"role": "assistant"}), None);
    for index in 0..DELTAS {
        let value = if abort {
            json!({"reasoning": delta(index)})
        } else {
            json!({"reasoning_content": delta(index)})
        };
        event(&mut first, value, None);
        if (index + 1) % BATCH == 0 {
            phase(&format!(
                "wire delta={index} sent; awaiting presented tail and editor"
            ));
            assert_eq!(ack.recv_timeout(WAIT).expect("UI checkpoint"), index);
        }
    }
    if abort {
        // No finish_reason, [DONE], EOF, or timeout supplied by the peer.
        // Only terminal Escape cancelling the live owner can release it.
        let mut byte = [0];
        match first.read(&mut byte) {
            Ok(0) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe
                ) => {}
            result => {
                panic!("cancelled stream must close before a recovery request: {result:?}");
            }
        }
    } else {
        finish(&mut first, "ANSWER_DONE");
    }
    drop(first);
    let mut next = accept(listener);
    let request = read_request(&next);
    assert!(request_has(
        &request,
        "user",
        if abort { "recover" } else { "next prompt" }
    ));
    if !abort {
        assert!(request_has(&request, "user", "reason carefully"));
        assert!(
            request_has(&request, "assistant", "ANSWER_DONE"),
            "resumed provider lost history"
        );
    }
    begin_response(&mut next);
    finish(&mut next, if abort { "RECOVERED_OK" } else { "RESUMED_OK" });
}

struct Screen {
    terminal: TerminalModel,
    flushes: usize,
}

struct ScreenWriter(Arc<Mutex<Screen>>);

impl Write for ScreenWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("screen").terminal.process(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().expect("screen").flushes += 1;
        Ok(())
    }
}

#[derive(Debug)]
enum Step {
    Tail(usize),
    Typed(usize),
    Erased(usize),
    Stored,
    ColdResume,
    Draft(&'static str),
    Aborting,
    Recovered,
    Finished,
}

struct Script {
    mode: Mode,
    step: Step,
    probes: usize,
    cold_resume: bool,
    last_flush: usize,
    deadline: Instant,
}

struct Input {
    screen: Arc<Mutex<Screen>>,
    script: Arc<Mutex<Script>>,
    ack: mpsc::SyncSender<usize>,
    agent: mpsc::Sender<PiMsg>,
    pending: VecDeque<Event>,
}

fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code))
}

impl Input {
    // Keep the ordered wire/UI handshake visible as one state machine.
    #[allow(clippy::too_many_lines)]
    fn advance(&mut self) {
        let (rows, flushes) = {
            let screen = self.screen.lock().expect("screen snapshot");
            let rows: Vec<_> = (0..usize::from(ROWS))
                .map(|row| screen.terminal.row_text(row).expect("row"))
                .collect();
            (rows, screen.flushes)
        };
        let mut script = self.script.lock().expect("script");
        let body = rows[1..usize::from(ROWS - 3)].join("\n");
        let compact: String = body.chars().filter(|ch| !ch.is_whitespace()).collect();
        let input = rows[usize::from(ROWS - 2)].trim();
        assert!(
            Instant::now() < script.deadline,
            "wire UI stalled at {:?}:\n{}",
            script.step,
            rows.join("\n")
        );
        if script.probes > 0
            && flushes != script.last_flush
            && matches!(
                script.step,
                Step::Tail(_) | Step::Typed(_) | Step::Erased(_)
            )
        {
            assert!(
                rows[0].contains("pi · working"),
                "lost live header: {rows:?}"
            );
            assert!(compact.contains("TAIL-"), "blank thinking frame: {rows:?}");
        }
        script.last_flush = flushes;
        let next = match script.step {
            Step::Tail(index) if compact.contains(&format!("TAIL-{index:04}")) => {
                assert!(rows[0].contains("working"));
                self.pending.push_back(key(KeyCode::Char('x')));
                Some(Step::Typed(index))
            }
            Step::Typed(index) if input == "x" => {
                assert!(
                    compact.contains(&format!("TAIL-{index:04}")),
                    "typing lost thinking tail"
                );
                self.pending.push_back(key(KeyCode::Backspace));
                Some(Step::Erased(index))
            }
            Step::Erased(index) if input.is_empty() || input.contains("Type a message") => {
                assert!(
                    compact.contains(&format!("TAIL-{index:04}")),
                    "editing lost thinking tail"
                );
                script.probes += 1;
                self.ack
                    .send(index)
                    .expect("acknowledge presented editor round trip");
                if index + BATCH < DELTAS {
                    Some(Step::Tail(index + BATCH))
                } else if script.mode == Mode::Abort {
                    phase("all thinking presented; dispatch terminal Escape into live Program");
                    self.pending.push_back(key(KeyCode::Escape));
                    Some(Step::Aborting)
                } else {
                    Some(Step::Stored)
                }
            }
            Step::ColdResume
                if compact.contains(&format!("TAIL-{:04}", DELTAS - 1))
                    && body.contains("ANSWER_DONE")
                    && rows[0].contains("ready") =>
            {
                script.cold_resume = true;
                self.pending
                    .extend("next prompt".chars().map(|ch| key(KeyCode::Char(ch))));
                Some(Step::Draft("next prompt"))
            }
            Step::Aborting if body.contains("ABORT_SETTLED") && rows[0].contains("ready") => {
                phase("Escape settled without quitting; typing a real recovery prompt");
                self.pending
                    .extend("recover".chars().map(|ch| key(KeyCode::Char(ch))));
                Some(Step::Draft("recover"))
            }
            Step::Draft(text) if input == text => {
                self.pending.push_back(key(KeyCode::Enter));
                Some(Step::Recovered)
            }
            Step::Stored
                if body.contains("FIRST_SAVED")
                    && body.contains("ANSWER_DONE")
                    && rows[0].contains("ready") =>
            {
                self.agent
                    .send(PiMsg::UiShutdown)
                    .expect("completed UI shutdown");
                Some(Step::Finished)
            }
            Step::Recovered
                if rows[0].contains("ready")
                    && ((script.mode == Mode::Resume
                        && body.contains("RESUME_SAVED")
                        && body.contains("RESUMED_OK"))
                        || (script.mode == Mode::Abort
                            && body.contains("RECOVERY_SAVED")
                            && body.contains("RECOVERED_OK"))) =>
            {
                self.agent
                    .send(PiMsg::UiShutdown)
                    .expect("recovered UI shutdown");
                Some(Step::Finished)
            }
            _ => None,
        };
        if let Some(next) = next {
            script.step = next;
        }
    }
}

impl BackendEventSource for Input {
    type Error = io::Error;

    fn size(&self) -> io::Result<(u16, u16)> {
        Ok((COLS, ROWS))
    }

    fn set_features(&mut self, _: BackendFeatures) -> io::Result<()> {
        Ok(())
    }

    fn poll_event(&mut self, timeout: Duration) -> io::Result<bool> {
        if self.pending.is_empty() {
            self.advance();
        }
        if self.pending.is_empty() && !timeout.is_zero() {
            std::thread::sleep(timeout.min(Duration::from_millis(1)));
            self.advance();
        }
        Ok(!self.pending.is_empty())
    }

    fn read_event(&mut self) -> io::Result<Option<Event>> {
        Ok(self.pending.pop_front())
    }
}

fn controlled_prompt(
    runtime: &asupersync::runtime::Runtime,
    handle: &mut AgentSessionHandle,
    slot: &ControlSlot,
    tx: &mpsc::Sender<PiMsg>,
    prompt: String,
    deltas: &Arc<AtomicUsize>,
) -> pi::error::Result<AssistantMessage> {
    let events = tx.clone();
    let count = Arc::clone(deltas);
    let turn = handle.prompt_controlled(prompt, move |event| {
        for message in agent_event_to_pi_msgs(&event) {
            if matches!(message, PiMsg::ThinkingDelta(_)) {
                count.fetch_add(1, Ordering::SeqCst);
            }
            events.send(message).expect("bridge real SDK event");
        }
    });
    let control = turn.control();
    *slot.lock().expect("control slot") = Some(control.clone());
    let result = runtime.block_on(turn);
    *slot.lock().expect("control slot") = None;
    assert!(
        control.snapshot().finished,
        "owner must retire before next turn"
    );
    result
}

fn submitted(rx: &mpsc::Receiver<UiCommand>, expected: &str) -> String {
    let command = rx.recv_timeout(WAIT).expect("real terminal submission");
    assert_eq!(command, UiCommand::Prompt(expected.to_string()));
    let UiCommand::Prompt(text) = command else {
        unreachable!()
    };
    text
}

// The three scenarios deliberately use the same SDK construction and owner
// publication, rather than separate success and cancellation substitutes.
#[allow(clippy::too_many_lines)]
fn drive(
    mode: Mode,
    path: &Path,
    url: &str,
    tx: &mpsc::Sender<PiMsg>,
    rx: &mpsc::Receiver<UiCommand>,
    slot: &ControlSlot,
) {
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("driver runtime");
    let stored = if mode == Mode::Resume {
        phase("cold reopen through Session::open");
        runtime
            .block_on(Session::open(path.to_string_lossy().as_ref()))
            .expect("cold session open")
    } else {
        let mut stored = Session::create_with_dir(path.parent().map(Path::to_path_buf));
        stored.path = Some(path.to_path_buf());
        stored
    };
    let history = stored.to_messages_for_current_path();
    let (messages, usage) = conversation_from_session(&stored);
    tx.send(PiMsg::ConversationReset {
        session_id: stored.header.id.clone(),
        messages,
        usage,
        status: None,
    })
    .expect("native conversation replay");
    let provider = Arc::new(
        OpenAIProvider::new("gh255-wire")
            .with_base_url(url)
            .with_reasoning(true),
    );
    let mut agent = Agent::new(
        provider,
        ToolRegistry::new(&[], path.parent().expect("session directory"), None),
        AgentConfig {
            stream_options: StreamOptions {
                api_key: Some("local-gh255-fixture-key".to_string()),
                max_tokens: Some(8192),
                ..StreamOptions::default()
            },
            ..AgentConfig::default()
        },
    );
    agent.replace_messages(history);
    let session = AgentSession::new(
        agent,
        Arc::new(asupersync::sync::Mutex::new(stored)),
        true,
        ResolvedCompactionSettings {
            enabled: false,
            ..ResolvedCompactionSettings::default()
        },
    );
    let mut handle =
        AgentSessionHandle::from_session_with_listeners(session, EventListeners::default());
    let deltas = Arc::new(AtomicUsize::new(0));
    let prompt = if mode == Mode::Resume {
        submitted(rx, "next prompt")
    } else {
        "reason carefully".to_string()
    };
    let result = controlled_prompt(&runtime, &mut handle, slot, tx, prompt, &deltas);
    if mode == Mode::Abort {
        match result {
            Err(pi::error::Error::Aborted) => {
                tx.send(PiMsg::AgentError("Operation aborted".to_string()))
                    .expect("terminal abort outcome");
            }
            Ok(message) => assert_eq!(message.stop_reason, StopReason::Aborted),
            Err(error) => panic!("expected native cancellation, not another error: {error}"),
        }
        assert_eq!(deltas.load(Ordering::SeqCst), DELTAS);
        tx.send(PiMsg::System("ABORT_SETTLED".to_string()))
            .expect("settled abort");
        let prompt = submitted(rx, "recover");
        let recovered = controlled_prompt(&runtime, &mut handle, slot, tx, prompt, &deltas)
            .expect("same-handle recovery");
        assert_eq!(recovered.stop_reason, StopReason::Stop);
        tx.send(PiMsg::System("RECOVERY_SAVED".to_string()))
            .expect("saved recovery");
    } else {
        let completed = result.expect("completed controlled turn");
        assert_eq!(completed.stop_reason, StopReason::Stop);
        if mode == Mode::Complete {
            assert_eq!(deltas.load(Ordering::SeqCst), DELTAS);
            assert_eq!(thinking(&completed), expected_thinking());
        }
        // prompt_controlled must cross the real persistence boundary. No
        // hand-built assistant message and no compensating session.save().
        assert!(path.is_file(), "SDK turn did not persist the session");
        let marker = if mode == Mode::Complete {
            "FIRST_SAVED"
        } else {
            "RESUME_SAVED"
        };
        tx.send(PiMsg::System(marker.to_string()))
            .expect("saved turn");
    }
}

fn run_ui(mode: Mode, path: &Path, url: &str, ack: mpsc::SyncSender<usize>) {
    let (agent_tx, agent_rx) = mpsc::channel();
    let (submit_tx, submit_rx) = mpsc::channel();
    let slot: ControlSlot = Arc::new(Mutex::new(None));
    let screen = Arc::new(Mutex::new(Screen {
        terminal: TerminalModel::new(usize::from(COLS), usize::from(ROWS)),
        flushes: 0,
    }));
    let script = Arc::new(Mutex::new(Script {
        mode,
        step: if mode == Mode::Resume {
            Step::ColdResume
        } else {
            Step::Tail(BATCH - 1)
        },
        probes: 0,
        cold_resume: false,
        last_flush: 0,
        deadline: Instant::now() + Duration::from_secs(90),
    }));
    let events = Input {
        screen: Arc::clone(&screen),
        script: Arc::clone(&script),
        ack,
        agent: agent_tx.clone(),
        pending: VecDeque::new(),
    };
    let owner = Arc::clone(&slot);
    let path = path.to_path_buf();
    let url = url.to_string();
    let driver =
        std::thread::spawn(move || drive(mode, &path, &url, &agent_tx, &submit_rx, &owner));
    let model = PiFtuiModel::new(agent_rx)
        .with_thinking_visible(true)
        .with_turn_control(slot)
        .with_submit_channel(submit_tx);
    let config = ProgramConfig {
        intercept_signals: false,
        ..ProgramConfig::default().with_forced_size(COLS, ROWS)
    };
    let mut capabilities = TerminalCapabilities::basic();
    capabilities.color_depth = ftui::ColorDepth::Ansi256;
    let writer = TerminalWriter::with_diff_config(
        ScreenWriter(screen),
        config.screen_mode,
        config.ui_anchor,
        capabilities,
        config.diff_config.clone(),
    );
    // Real subscription manager, bounded sender, event queue, frame budgets,
    // diff engine and ANSI writer. No Model::update/view calls in the test,
    // no injected degradation, and no periodic full-redraw workaround.
    let mut program = Program::with_event_source(
        model,
        events,
        BackendFeatures::default(),
        TerminalPresenter::new(writer),
        config,
    )
    .expect("headless real Program");
    program.run().expect("run actual FTUI loop");
    assert!(!program.is_running());
    let proof = script.lock().expect("script proof");
    assert!(
        matches!(proof.step, Step::Finished),
        "premature UI exit: {:?}",
        proof.step
    );
    if mode == Mode::Resume {
        assert!(proof.cold_resume);
    } else {
        assert_eq!(proof.probes, DELTAS / BATCH);
    }
    drop(proof);
    drop(program);
    driver.join().expect("SDK driver completed");
}

pub(super) fn run(abort: bool) {
    let directory = tempfile::tempdir().expect("real session directory");
    let path = directory.path().join("reasoning.jsonl");
    let listener = TcpListener::bind("127.0.0.1:0").expect("local SSE peer");
    listener.set_nonblocking(true).expect("bounded accept");
    let url = format!("http://{}/v1", listener.local_addr().expect("peer address"));
    let (ack_tx, ack_rx) = mpsc::sync_channel(1);
    let server = std::thread::spawn(move || peer(&listener, &ack_rx, abort));
    run_ui(
        if abort { Mode::Abort } else { Mode::Complete },
        &path,
        &url,
        ack_tx.clone(),
    );
    if !abort {
        // Both Program and the SDK handle were dropped before the next open.
        run_ui(Mode::Resume, &path, &url, ack_tx);
    }
    server.join().expect("real HTTP peer completed");
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("inspection runtime");
    let saved = runtime
        .block_on(Session::open(path.to_string_lossy().as_ref()))
        .expect("reopen final durable session");
    let assistants: Vec<_> = saved
        .to_messages_for_current_path()
        .into_iter()
        .filter_map(|message| match message {
            Message::Assistant(message) => Some(message),
            _ => None,
        })
        .collect();
    if !abort {
        assert_eq!(
            thinking(&assistants[0]),
            expected_thinking(),
            "persisted reasoning is exact"
        );
    }
    let last = assistants.last().expect("persisted continuation");
    assert_eq!(last.stop_reason, StopReason::Stop);
    let answer: String = last
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(answer, if abort { "RECOVERED_OK" } else { "RESUMED_OK" });
}
