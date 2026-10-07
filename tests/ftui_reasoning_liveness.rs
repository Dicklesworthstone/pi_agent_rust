//! GH #255: mixed-width reasoning must not strand the FTUI event loop.
//!
//! Direct model cases exercise Full-quality frames. Wire cases additionally
//! drive the real provider, SDK persistence, subscription, Program and ANSI
//! presentation paths with keyboard checkpoints during the reasoning stream.
//! Supervised children bound nontermination; deadlines are not performance
//! thresholds. The wire peer gates batches, not an unbounded producer flood.

#![cfg(feature = "ftui")]
#![forbid(unsafe_code)]
#![recursion_limit = "256"]

use std::io::Write as _;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use ftui::render::grapheme_pool::GraphemePool;
use ftui::text::{Line, WrapMode};
use ftui::{Cmd, Event, Frame, KeyCode, KeyEvent, KeyEventKind, Model, Modifiers};
use pi::agent::AbortHandle;
use pi::interactive::{ConversationMessage, MessageRole, PiMsg};
use pi::interactive_ftui::{PiFtuiModel, PiFtuiMsg, UiCommand};
use pi::model::{AssistantMessage, ContentBlock, Message, StopReason, Usage, UserContent};
use pi::session::{Session, SessionMessage};

#[path = "ftui_reasoning_liveness/wire.rs"]
mod wire;

const CHILD_CASE: &str = "PI_GH255_CHILD_CASE";
const WIDTH: u16 = 47;
const HEIGHT: u16 = 12;
const DELTAS: usize = 4096;

fn phase(label: &str) {
    println!("GH255 {label}");
    std::io::stdout().flush().expect("flush diagnostic phase");
}

#[test]
fn gh255_reasoning_stream_and_persisted_resume_remain_live() {
    for case in ["word_char_progress", "stream_resume_abort"] {
        supervise(case);
    }
}

#[test]
fn gh255_wire_reasoning_stays_visible_and_cold_resume_completes() {
    supervise("wire_complete_resume");
}

#[test]
fn gh255_wire_escape_cancels_open_stream_and_same_session_recovers() {
    supervise("wire_abort_recovery");
}

fn supervise(case: &str) {
    let log = tempfile::NamedTempFile::new().expect("child diagnostic log");
    let output = log.reopen().expect("open diagnostic output");
    let mut child = Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "gh255_supervised_child",
            "--ignored",
            "--nocapture",
        ])
        .env(CHILD_CASE, case)
        .stdin(Stdio::null())
        .stdout(Stdio::from(output.try_clone().expect("clone output handle")))
        .stderr(Stdio::from(output))
        .spawn()
        .expect("spawn supervised FTUI test");
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll supervised child") {
            break status;
        }
        if started.elapsed() > Duration::from_secs(120) {
            let _ = child.kill();
            let _ = child.wait();
            let trace = std::fs::read_to_string(log.path()).unwrap_or_default();
            panic!("GH255 {case} failed to return; last phase identifies the stall:\n{trace}");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let trace = std::fs::read_to_string(log.path()).expect("read child trace");
    assert!(status.success(), "GH255 {case} failed: {status}\n{trace}");
    assert!(
        trace.contains("GH255 complete"),
        "child did not finish: {trace}"
    );
}

#[test]
#[ignore = "run by the parent with a nontermination deadline"]
fn gh255_supervised_child() {
    match std::env::var(CHILD_CASE)
        .expect("supervised child case")
        .as_str()
    {
        "word_char_progress" => word_char_progress(),
        "stream_resume_abort" => stream_resume_abort(),
        "wire_complete_resume" => wire::run(false),
        "wire_abort_recovery" => wire::run(true),
        other => panic!("unknown GH255 case: {other}"),
    }
    phase("complete");
}

fn word_char_progress() {
    // In 0.7 the first wide grapheme uses two of three cells. Splitting the
    // second into the remaining cell returns (empty, unchanged). Neither
    // the empty-row nor the full-row branch fires: the loop cannot progress.
    phase("WordChar: two wide graphemes, width=3, before wrap");
    let line = Line::styled("界界", ftui::Style::new().dim());
    let wrapped = line.wrap(3, WrapMode::WordChar);
    assert_eq!(wrapped.len(), 2);
    assert_eq!(wrapped[0].to_plain_text(), "界");
    assert_eq!(wrapped[1].to_plain_text(), "界");
    for row in &wrapped {
        assert!(row.width() <= 3);
        assert_eq!(row.spans()[0].style, line.spans()[0].style);
    }
    phase("WordChar: mixed ASCII/CJK/emoji, before wrap");
    let input = "a界🙂界bc界";
    let rows = Line::raw(input).wrap(3, WrapMode::WordChar);
    assert_eq!(
        rows.iter().map(Line::to_plain_text).collect::<String>(),
        input
    );
    assert!(rows.iter().all(|row| row.width() <= 3));
}

fn key(code: KeyCode, modifiers: Modifiers) -> PiFtuiMsg {
    PiFtuiMsg::Term(Event::Key(KeyEvent {
        code,
        modifiers,
        kind: KeyEventKind::Press,
    }))
}

fn send(model: &mut PiFtuiModel, msg: PiMsg) {
    let _ = model.update(PiFtuiMsg::Agent(msg));
}

fn capture(model: &PiFtuiModel) -> String {
    let mut pool = GraphemePool::new();
    let mut frame = Frame::new(WIDTH, HEIGHT, &mut pool);
    // Keep Frame::new's Full quality: the first offending delta and cold
    // resume must work without setting a degradation override.
    model.view(&mut frame);
    let mut text = String::new();
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            text.push(
                frame
                    .buffer
                    .get(x, y)
                    .and_then(|cell| cell.content.as_char())
                    .unwrap_or(' '),
            );
        }
        text.push('\n');
    }
    text
}

fn assert_tail(frame: &str, marker: &str) {
    // ASCII markers remain observable when wrapped. The actual wide text
    // is checked separately after its round trip through session storage.
    let compact: String = frame.chars().filter(|ch| !ch.is_whitespace()).collect();
    assert!(
        compact.contains(marker),
        "missing live tail {marker}:\n{frame}"
    );
    assert!(frame.contains("pi ·"), "missing header:\n{frame}");
}

fn run_async<T>(future: impl std::future::Future<Output = T>) -> T {
    asupersync::runtime::RuntimeBuilder::new()
        .build()
        .expect("session runtime")
        .block_on(future)
}

fn persist_and_reopen(thinking: &str) -> Session {
    let directory = tempfile::tempdir().expect("session directory");
    let path = directory.path().join("reasoning.jsonl");
    let mut session = Session::create_with_dir(Some(directory.path().to_path_buf()));
    session.path = Some(path.clone());
    session.append_message(SessionMessage::User {
        content: UserContent::Text("reason carefully".to_string()),
        timestamp: Some(0),
    });
    let assistant: AssistantMessage = serde_json::from_value(serde_json::json!({
        "content": [
            { "type": "thinking", "thinking": thinking },
            { "type": "text", "text": "ANSWER_DONE" }
        ],
        "api": "openai-completions",
        "provider": "gh255-fixture",
        "model": "reasoning-fixture",
        "usage": Usage::default(),
        "stopReason": "stop",
        "timestamp": 1
    }))
    .expect("assistant message fixture");
    session.append_message(SessionMessage::Assistant {
        message: assistant.into(),
    });
    phase("persist completed thinking: before save");
    run_async(session.save()).expect("save completed session");
    let expected = serde_json::to_value(&session.entries).expect("original entries");
    drop(session);
    phase("cold session resume: before open");
    let reopened = run_async(Session::open(path.to_string_lossy().as_ref()))
        .expect("open completed session from disk");
    assert_eq!(
        serde_json::to_value(&reopened.entries).expect("loaded entries"),
        expected
    );
    reopened
}

fn stream_resume_abort() {
    let (_agent_tx, agent_rx) = mpsc::channel();
    let mut model = PiFtuiModel::new(agent_rx).with_thinking_visible(true);
    let _ = model.update(PiFtuiMsg::Term(Event::Resize {
        width: WIDTH,
        height: HEIGHT,
    }));
    send(&mut model, PiMsg::AgentStart);
    send(&mut model, PiMsg::ThinkingDelta("warmup\n".to_string()));
    assert!(capture(&model).contains("warmup"));

    let mut thinking = String::from("warmup\n");
    // An odd usable width leaves one cell before a two-cell glyph. 0.7
    // stalls in the FIRST frame containing this token, not in update().
    let token = "界".repeat(48);
    for index in 0..DELTAS {
        let marker = format!("TAIL-{index:04}");
        let delta = format!("{token}\n{marker}\n");
        thinking.push_str(&delta);
        send(&mut model, PiMsg::ThinkingDelta(delta));
        if index == 0 || index % 64 == 63 {
            phase(&format!(
                "delta={index} update returned; before Full frame"
            ));
            assert_tail(&capture(&model), &marker);
            // Interleave real terminal-key dispatch without scrolling: the
            // default follow-stream-tail state must continue to follow.
            let hide = model.update(key(KeyCode::Char('t'), Modifiers::CTRL));
            let show = model.update(key(KeyCode::Char('t'), Modifiers::CTRL));
            assert!(matches!(hide, Cmd::None));
            assert!(matches!(show, Cmd::None));
            assert_tail(&capture(&model), &marker);
        }
    }
    send(&mut model, PiMsg::TextDelta("ANSWER_DONE".to_string()));
    send(
        &mut model,
        PiMsg::AgentDone {
            usage: None,
            stop_reason: StopReason::Stop,
            error_message: None,
        },
    );
    phase("completed turn: before Full frame");
    assert!(capture(&model).contains("ANSWER_DONE"));

    let reopened = persist_and_reopen(&thinking);
    let messages = reopened.to_messages();
    let assistant = messages
        .iter()
        .find_map(|message| match message {
            Message::Assistant(assistant) => Some(assistant),
            _ => None,
        })
        .expect("persisted assistant");
    let restored_thinking = assistant
        .content
        .iter()
        .find_map(|block| match block {
            ContentBlock::Thinking(thinking) => Some(thinking.thinking.clone()),
            _ => None,
        })
        .expect("persisted thinking block");
    assert_eq!(restored_thinking, thinking, "never truncate stored reasoning");

    let (_tx, rx) = mpsc::channel();
    let (submit_tx, submit_rx) = mpsc::channel();
    let (abort, signal) = AbortHandle::new();
    let slot = Arc::new(Mutex::new(Some(abort)));
    let mut resumed = PiFtuiModel::new(rx)
        .with_thinking_visible(true)
        .with_submit_channel(submit_tx)
        .with_turn_abort(slot);
    send(
        &mut resumed,
        PiMsg::ConversationReset {
            session_id: reopened.header.id.clone(),
            messages: vec![ConversationMessage {
                role: MessageRole::Assistant,
                content: "ANSWER_DONE".to_string(),
                thinking: Some(restored_thinking),
                collapsed: false,
            }],
            usage: Usage::default(),
            status: None,
        },
    );
    phase("cold UI replay: before Full frame");
    let frame = capture(&resumed);
    assert_tail(&frame, "TAIL-4095");
    assert!(frame.contains("ANSWER_DONE"));
    for ch in "next prompt".chars() {
        let _ = resumed.update(key(KeyCode::Char(ch), Modifiers::empty()));
    }
    assert!(
        capture(&resumed).contains("next prompt"),
        "resumed editor accepts input"
    );
    let _ = resumed.update(key(KeyCode::Enter, Modifiers::empty()));
    assert_eq!(
        submit_rx.try_recv().expect("next prompt submitted"),
        UiCommand::Prompt("next prompt".to_string())
    );

    send(&mut resumed, PiMsg::AgentStart);
    send(
        &mut resumed,
        PiMsg::ThinkingDelta(format!("{token}\nABORT_TAIL")),
    );
    phase("resumed turn: before abort-frame");
    assert_tail(&capture(&resumed), "ABORT_TAIL");
    let first = resumed.update(key(KeyCode::Char('c'), Modifiers::CTRL));
    assert!(matches!(first, Cmd::None));
    assert!(!signal.is_aborted());
    // No expensive rendering between keys: the shipped double-tap interval
    // is a UI policy, not the workload deadline.
    let second = resumed.update(key(KeyCode::Char('c'), Modifiers::CTRL));
    assert!(matches!(second, Cmd::Quit));
    assert!(signal.is_aborted(), "quit must deliver the real abort handle");
}
