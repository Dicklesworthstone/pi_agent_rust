//! Session picker listing and navigation tests (no TUI runtime).

mod common;

use asupersync::runtime::RuntimeBuilder;
use bubbletea::{KeyMsg, KeyType, Message};
use common::TestHarness;
use pi::config::Config;
use pi::model::UserContent;
use pi::session::{Session, SessionHeader, SessionMessage, encode_cwd};
use pi::session_index::SessionMeta;
use pi::session_picker::{SessionPicker, format_time, list_sessions_for_project, pick_session};
use std::env;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::thread::sleep;
use std::time::Duration;

fn key_message(key_type: KeyType, runes: Vec<char>) -> Message {
    Message::new(KeyMsg {
        key_type,
        runes,
        alt: false,
        paste: false,
    })
}

fn write_session_file(base_dir: &Path, cwd: &Path, name: &str, id: &str) -> String {
    let project_dir = base_dir.join(encode_cwd(cwd));
    std::fs::create_dir_all(&project_dir).expect("create project session dir");
    let path = project_dir.join(name);

    let mut header = SessionHeader::new();
    header.cwd = cwd.display().to_string();
    header.id = id.to_string();
    header.timestamp = "2026-02-03T12:00:00.000Z".to_string();

    let json = serde_json::to_string(&header).expect("serialize header");
    std::fs::write(&path, format!("{json}\n")).expect("write session file");
    path.display().to_string()
}

fn selected_line(view: &str) -> Option<&str> {
    view.lines().find(|line| line.starts_with('>'))
}

fn run_async<T>(future: impl Future<Output = T>) -> T {
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("runtime build");
    runtime.block_on(future)
}

fn session_picker_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().expect("lock")
}

struct CurrentDirGuard {
    previous: PathBuf,
}

impl CurrentDirGuard {
    fn new(path: &Path) -> Self {
        let previous = env::current_dir().expect("current dir");
        env::set_current_dir(path).expect("set current dir");
        Self { previous }
    }
}

impl Drop for CurrentDirGuard {
    fn drop(&mut self) {
        let _ = env::set_current_dir(&self.previous);
    }
}

fn create_session(harness: &TestHarness, base_dir: &Path, cwd: &Path, label: &str) -> PathBuf {
    let mut session = Session::create_with_dir(Some(base_dir.to_path_buf()));
    session.header.cwd = cwd.display().to_string();
    session.append_message(SessionMessage::User {
        content: UserContent::Text(format!("hello {label}")),
        timestamp: Some(0),
    });
    run_async(async {
        session.save().await.expect("save session");
    });

    let path = session.path.clone().expect("session path");
    harness.log().info_ctx("setup", "Created session", |ctx| {
        ctx.push(("label".to_string(), label.to_string()));
        ctx.push(("path".to_string(), path.display().to_string()));
    });
    path
}

#[test]
fn format_time_parses_rfc3339() {
    let harness = TestHarness::new("format_time_parses_rfc3339");
    let input = "2026-02-03T10:30:00.000Z";
    let formatted = format_time(input);
    harness
        .log()
        .info_ctx("format", "formatted timestamp", |ctx| {
            ctx.push(("input".to_string(), input.to_string()));
            ctx.push(("output".to_string(), formatted.clone()));
        });
    assert!(formatted.contains("2026-02-03"));
    assert!(formatted.contains("10:30"));
}

#[test]
fn format_time_falls_back_for_invalid() {
    let harness = TestHarness::new("format_time_falls_back_for_invalid");
    let input = "not-a-timestamp";
    let formatted = format_time(input);
    harness
        .log()
        .info_ctx("format", "fallback timestamp", |ctx| {
            ctx.push(("input".to_string(), input.to_string()));
            ctx.push(("output".to_string(), formatted.clone()));
        });
    assert_eq!(formatted, input);
}

#[test]
fn list_sessions_for_project_returns_empty_if_missing() {
    let harness = TestHarness::new("list_sessions_for_project_returns_empty_if_missing");
    let base_dir = harness.temp_path("sessions");
    let cwd = harness.temp_path("project");
    std::fs::create_dir_all(&cwd).expect("create cwd");

    let sessions = list_sessions_for_project(&cwd, Some(&base_dir));
    harness.log().info_ctx("list", "sessions list", |ctx| {
        ctx.push(("count".to_string(), sessions.len().to_string()));
    });
    assert!(sessions.is_empty());
}

#[test]
fn list_sessions_for_project_orders_by_mtime() {
    let harness = TestHarness::new("list_sessions_for_project_orders_by_mtime");
    let base_dir = harness.temp_path("sessions");
    let cwd = harness.temp_path("project");
    std::fs::create_dir_all(&cwd).expect("create cwd");

    let first_path = write_session_file(&base_dir, &cwd, "a.jsonl", "aaaaaaa1");
    sleep(Duration::from_millis(15));
    let second_path = write_session_file(&base_dir, &cwd, "b.jsonl", "bbbbbbb2");

    let sessions = list_sessions_for_project(&cwd, Some(&base_dir));
    harness.log().info_ctx("list", "ordered sessions", |ctx| {
        ctx.push(("count".to_string(), sessions.len().to_string()));
        if let Some(first) = sessions.first() {
            ctx.push(("first".to_string(), first.path.clone()));
        }
    });
    assert!(sessions.len() >= 2);
    assert_eq!(sessions[0].path, second_path);
    assert_eq!(sessions[1].path, first_path);
}

#[test]
fn session_picker_navigation_down_up() {
    let harness = TestHarness::new("session_picker_navigation_down_up");
    let sessions = vec![
        SessionMeta {
            path: "/tmp/a.jsonl".to_string(),
            id: "aaaaaaaa".to_string(),
            cwd: "/tmp".to_string(),
            timestamp: "2026-02-03T10:00:00.000Z".to_string(),
            message_count: 1,
            last_modified_ms: 1000,
            size_bytes: 100,
            name: None,
        },
        SessionMeta {
            path: "/tmp/b.jsonl".to_string(),
            id: "bbbbbbbb".to_string(),
            cwd: "/tmp".to_string(),
            timestamp: "2026-02-03T11:00:00.000Z".to_string(),
            message_count: 2,
            last_modified_ms: 2000,
            size_bytes: 200,
            name: None,
        },
    ];

    let mut picker = SessionPicker::new(sessions);
    let initial_view = picker.view();
    assert!(
        selected_line(&initial_view)
            .unwrap_or_default()
            .contains("aaaaaaaa")
    );

    picker.update(key_message(KeyType::Down, Vec::new()));
    let after_down = picker.view();
    harness.log().info("nav", "after Down");
    assert!(
        selected_line(&after_down)
            .unwrap_or_default()
            .contains("bbbbbbbb")
    );

    picker.update(key_message(KeyType::Up, Vec::new()));
    let after_up = picker.view();
    assert!(
        selected_line(&after_up)
            .unwrap_or_default()
            .contains("aaaaaaaa")
    );
}

#[test]
fn session_picker_navigation_with_jk() {
    let harness = TestHarness::new("session_picker_navigation_with_jk");
    let sessions = vec![
        SessionMeta {
            path: "/tmp/a.jsonl".to_string(),
            id: "aaaaaaaa".to_string(),
            cwd: "/tmp".to_string(),
            timestamp: "2026-02-03T10:00:00.000Z".to_string(),
            message_count: 1,
            last_modified_ms: 1000,
            size_bytes: 100,
            name: None,
        },
        SessionMeta {
            path: "/tmp/b.jsonl".to_string(),
            id: "bbbbbbbb".to_string(),
            cwd: "/tmp".to_string(),
            timestamp: "2026-02-03T11:00:00.000Z".to_string(),
            message_count: 2,
            last_modified_ms: 2000,
            size_bytes: 200,
            name: None,
        },
    ];

    let mut picker = SessionPicker::new(sessions);
    picker.update(key_message(KeyType::Runes, vec!['j']));
    let after_j = picker.view();
    harness.log().info("nav", "after j");
    assert!(
        selected_line(&after_j)
            .unwrap_or_default()
            .contains("bbbbbbbb")
    );

    picker.update(key_message(KeyType::Runes, vec!['k']));
    let after_k = picker.view();
    assert!(
        selected_line(&after_k)
            .unwrap_or_default()
            .contains("aaaaaaaa")
    );
}

#[test]
fn session_picker_enter_sets_chosen_path() {
    let harness = TestHarness::new("session_picker_enter_sets_chosen_path");
    let sessions = vec![SessionMeta {
        path: "/tmp/a.jsonl".to_string(),
        id: "aaaaaaaa".to_string(),
        cwd: "/tmp".to_string(),
        timestamp: "2026-02-03T10:00:00.000Z".to_string(),
        message_count: 1,
        last_modified_ms: 1000,
        size_bytes: 100,
        name: None,
    }];

    let mut picker = SessionPicker::new(sessions);
    picker.update(key_message(KeyType::Enter, Vec::new()));
    harness.log().info("select", "pressed Enter");
    assert_eq!(picker.selected_path(), Some("/tmp/a.jsonl"));
}

#[test]
fn session_picker_cancel_sets_flag() {
    let harness = TestHarness::new("session_picker_cancel_sets_flag");
    let sessions = vec![SessionMeta {
        path: "/tmp/a.jsonl".to_string(),
        id: "aaaaaaaa".to_string(),
        cwd: "/tmp".to_string(),
        timestamp: "2026-02-03T10:00:00.000Z".to_string(),
        message_count: 1,
        last_modified_ms: 1000,
        size_bytes: 100,
        name: None,
    }];

    let mut picker = SessionPicker::new(sessions);
    picker.update(key_message(KeyType::Esc, Vec::new()));
    harness.log().info("cancel", "pressed Esc");
    assert!(picker.was_cancelled());
}

#[test]
fn pick_session_returns_none_when_no_sessions() {
    let _lock = session_picker_lock();
    let harness = TestHarness::new("pick_session_returns_none_when_no_sessions");
    let base_dir = harness.temp_path("sessions");
    let cwd = harness.temp_path("project");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let _guard = CurrentDirGuard::new(&cwd);

    let result = run_async(pick_session(Some(&base_dir)));
    harness
        .log()
        .info_ctx("result", "pick_session returned", |ctx| {
            ctx.push(("is_some".to_string(), result.is_some().to_string()));
        });
    assert!(result.is_none());
}

#[test]
fn pick_session_returns_session_when_single_entry() {
    let _lock = session_picker_lock();
    let harness = TestHarness::new("pick_session_returns_session_when_single_entry");
    let base_dir = harness.temp_path("sessions");
    let cwd = harness.temp_path("project");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let _guard = CurrentDirGuard::new(&cwd);

    let session_path = create_session(&harness, &base_dir, &cwd, "single");
    let session = run_async(pick_session(Some(&base_dir))).expect("pick session");
    harness
        .log()
        .info_ctx("verify", "picked session path", |ctx| {
            ctx.push(("expected".to_string(), session_path.display().to_string()));
            ctx.push((
                "actual".to_string(),
                session
                    .path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
            ));
        });
    assert_eq!(session.path.as_ref(), Some(&session_path));
    assert_eq!(session.session_dir.as_ref(), Some(&base_dir));
}

#[test]
fn resume_with_picker_creates_new_session_when_project_dir_missing() {
    let _lock = session_picker_lock();
    let harness =
        TestHarness::new("resume_with_picker_creates_new_session_when_project_dir_missing");
    let base_dir = harness.temp_path("sessions");
    let cwd = harness.temp_path("project");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let _guard = CurrentDirGuard::new(&cwd);

    let session = run_async(async {
        Session::resume_with_picker(Some(&base_dir), &Config::default(), None)
            .await
            .expect("resume")
    });
    harness.log().info_ctx("verify", "resume result", |ctx| {
        ctx.push((
            "path".to_string(),
            session
                .path
                .as_ref()
                .map_or_else(|| "<none>".to_string(), |p| p.display().to_string()),
        ));
    });
    assert!(session.path.is_none());
    assert_eq!(session.session_dir.as_ref(), Some(&base_dir));
}

#[test]
fn resume_with_picker_creates_new_session_when_sessions_empty() {
    let _lock = session_picker_lock();
    let harness = TestHarness::new("resume_with_picker_creates_new_session_when_sessions_empty");
    let base_dir = harness.temp_path("sessions");
    let cwd = harness.temp_path("project");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let _guard = CurrentDirGuard::new(&cwd);
    let project_dir = base_dir.join(encode_cwd(&cwd));
    std::fs::create_dir_all(&project_dir).expect("create project dir");

    let session = run_async(async {
        Session::resume_with_picker(Some(&base_dir), &Config::default(), None)
            .await
            .expect("resume")
    });
    harness.log().info_ctx("verify", "resume result", |ctx| {
        ctx.push((
            "path".to_string(),
            session
                .path
                .as_ref()
                .map_or_else(|| "<none>".to_string(), |p| p.display().to_string()),
        ));
        ctx.push((
            "session_dir".to_string(),
            session
                .session_dir
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        ));
    });
    assert!(session.path.is_none());
    assert_eq!(session.session_dir.as_ref(), Some(&base_dir));
}

#[test]
fn resume_with_picker_selects_session_with_override_input() {
    let _lock = session_picker_lock();
    let harness = TestHarness::new("resume_with_picker_selects_session_with_override_input");
    let base_dir = harness.temp_path("sessions");
    let cwd = harness.temp_path("project");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let _guard = CurrentDirGuard::new(&cwd);

    let first_path = create_session(&harness, &base_dir, &cwd, "first");
    sleep(Duration::from_millis(20));
    let second_path = create_session(&harness, &base_dir, &cwd, "second");

    let session = run_async(async {
        Session::resume_with_picker(Some(&base_dir), &Config::default(), Some("2".to_string()))
            .await
            .expect("resume")
    });

    harness.log().info_ctx("verify", "selected session", |ctx| {
        ctx.push(("expected".to_string(), first_path.display().to_string()));
        ctx.push(("newest".to_string(), second_path.display().to_string()));
        ctx.push((
            "actual".to_string(),
            session
                .path
                .as_ref()
                .map_or_else(|| "<none>".to_string(), |p| p.display().to_string()),
        ));
    });

    assert_eq!(session.path.as_ref(), Some(&first_path));
    assert_eq!(session.session_dir.as_ref(), Some(&base_dir));
}
