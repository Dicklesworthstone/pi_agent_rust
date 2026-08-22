//! Conformance tests for session JSONL format (v3).

mod common;

use asupersync::runtime::RuntimeBuilder;
use common::{TestHarness, validate_jsonl};
use pi::Error;
use pi::model::{AssistantMessage, ContentBlock, StopReason, TextContent, Usage, UserContent};
use pi::session::{
    CustomEntry, EntryBase, Session, SessionEntry, SessionHeader, SessionMessage, encode_cwd,
};
use pi::session_index::SessionIndex;
use proptest::prelude::*;
use serde_json::json;
use std::future::Future;
use std::path::Path;

fn write_session_file(harness: &TestHarness, contents: &str) -> std::path::PathBuf {
    harness.create_file("session.jsonl", contents)
}

fn run_async_test<F: Future<Output = ()>>(future: F) {
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("runtime build");
    runtime.block_on(future);
}

fn make_user_message(text: &str) -> SessionMessage {
    SessionMessage::User {
        content: UserContent::Text(text.to_string()),
        timestamp: Some(0),
    }
}

fn make_assistant_message(text: &str) -> SessionMessage {
    SessionMessage::Assistant {
        message: AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new(text))],
            api: "test".to_string(),
            provider: "test".to_string(),
            model: "test".to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            stop_details: None,
            error_message: None,
            timestamp: 0,
        },
    }
}

async fn open_session(path: &Path) -> pi::PiResult<Session> {
    let path_string = path.to_string_lossy().to_string();
    Session::open(&path_string).await
}

async fn save_and_reopen(harness: &TestHarness, session: &mut Session) -> Session {
    session.save().await.expect("save session");
    let path = session.path.clone().expect("session path set");
    harness.record_artifact("session.jsonl", &path);
    open_session(&path).await.expect("reopen session")
}

fn assert_contains_entry<'a>(
    entries: &'a [SessionEntry],
    predicate: impl Fn(&'a SessionEntry) -> bool,
    description: &str,
) {
    assert!(
        entries.iter().any(predicate),
        "Expected session entries to contain {description}"
    );
}

#[test]
fn dropin174_session_surface_logs_include_requirement_id() {
    let harness = TestHarness::new("dropin174_session_surface_logs_include_requirement_id");
    harness
        .log()
        .info_ctx("dropin174.session", "Session parity assertion", |ctx| {
            ctx.push((
                "requirement_id".to_string(),
                "DROPIN-174-SESSION".to_string(),
            ));
            ctx.push(("surface".to_string(), "session".to_string()));
            ctx.push((
                "parity_requirement".to_string(),
                "Session/branch/compaction parity".to_string(),
            ));
        });

    let jsonl = harness.log().dump_jsonl();
    let validation_errors = validate_jsonl(&jsonl);
    assert!(
        validation_errors.is_empty(),
        "expected valid structured logs, got {validation_errors:?}"
    );

    let has_requirement_log = jsonl
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .any(|record| {
            record.get("category").and_then(serde_json::Value::as_str) == Some("dropin174.session")
                && record
                    .get("context")
                    .and_then(|ctx| ctx.get("requirement_id"))
                    .and_then(serde_json::Value::as_str)
                    == Some("DROPIN-174-SESSION")
                && record
                    .get("context")
                    .and_then(|ctx| ctx.get("surface"))
                    .and_then(serde_json::Value::as_str)
                    == Some("session")
        });

    assert!(
        has_requirement_log,
        "expected structured log entry to include requirement_id + surface context"
    );
}

fn proptest_session_header() -> impl Strategy<Value = SessionHeader> {
    let provider = prop_oneof![
        Just(None),
        Just(Some("anthropic".to_string())),
        Just(Some("openai".to_string())),
        Just(Some("gemini".to_string())),
        Just(Some("azure".to_string())),
    ];
    let model_id = prop_oneof![
        Just(None),
        Just(Some("claude-test".to_string())),
        Just(Some("gpt-test".to_string())),
        Just(Some("gemini-test".to_string())),
    ];
    let thinking_level = prop_oneof![
        Just(None),
        Just(Some("off".to_string())),
        Just(Some("low".to_string())),
        Just(Some("medium".to_string())),
        Just(Some("high".to_string())),
    ];
    let parent_session = prop_oneof![
        Just(None),
        Just(Some("/tmp/parent.jsonl".to_string())),
        Just(Some("parent-session-id".to_string())),
    ];

    (
        0u32..1_000_000,
        provider,
        model_id,
        thinking_level,
        parent_session,
    )
        .prop_map(
            |(seed, provider, model_id, thinking_level, parent_session)| SessionHeader {
                r#type: "session".to_string(),
                version: Some(pi::session::SESSION_VERSION),
                id: format!("sess-{seed}"),
                timestamp: "2026-02-03T00:00:00.000Z".to_string(),
                cwd: "/tmp/project".to_string(),
                provider,
                model_id,
                thinking_level,
                fallback_provider: None,
                fallback_model_id: None,
                fallback_thinking_level: None,
                current_leaf: None,
                parent_session,
                additional_roots: None,
            },
        )
}

fn proptest_message_entries() -> impl Strategy<Value = Vec<SessionEntry>> {
    proptest::collection::btree_set(0u32..1_000_000, 0..16)
        .prop_flat_map(|id_set| {
            let ids = id_set
                .into_iter()
                .map(|value| format!("{value:08x}"))
                .collect::<Vec<_>>();
            let len = ids.len();
            (
                Just(ids),
                proptest::collection::vec(any::<u8>(), len),
                proptest::collection::vec(any::<u8>(), len),
            )
        })
        .prop_map(|(ids, parent_bytes, msg_bytes)| {
            let mut entries = Vec::with_capacity(ids.len());
            for i in 0..ids.len() {
                let parent_choice = parent_bytes[i] % u8::try_from(i + 1).unwrap_or(u8::MAX);
                let parent_id = if parent_choice == 0 {
                    Some("root".to_string())
                } else {
                    let parent_idx = usize::from(parent_choice - 1);
                    Some(ids[parent_idx].clone())
                };

                let base = EntryBase {
                    id: Some(ids[i].clone()),
                    parent_id,
                    timestamp: "2026-02-03T00:00:01.000Z".to_string(),
                };
                let message = SessionMessage::User {
                    content: UserContent::Text(format!("msg-{i}-{}", msg_bytes[i])),
                    timestamp: Some(0),
                };
                entries.push(SessionEntry::Message(pi::session::MessageEntry {
                    base,
                    message,
                }));
            }
            entries
        })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, .. ProptestConfig::default() })]

    #[test]
    fn proptest_session_header_json_roundtrip(header in proptest_session_header()) {
        let encoded = serde_json::to_string(&header).expect("serialize header");
        let decoded: SessionHeader = serde_json::from_str(&encoded).expect("parse header");
        prop_assert_eq!(
            serde_json::to_value(&decoded).expect("to value"),
            serde_json::to_value(&header).expect("to value"),
        );
    }

    #[test]
    fn proptest_session_entry_json_roundtrip_and_tree_invariants(entries in proptest_message_entries()) {
        let mut decoded_entries = Vec::with_capacity(entries.len());
        for entry in &entries {
            let encoded = serde_json::to_string(entry).expect("serialize entry");
            let decoded: SessionEntry = serde_json::from_str(&encoded).expect("parse entry");
            prop_assert_eq!(
                serde_json::to_value(&decoded).expect("to value"),
                serde_json::to_value(entry).expect("to value"),
            );
            decoded_entries.push(decoded);
        }

        let ids = decoded_entries
            .iter()
            .filter_map(|entry| entry.base().id.as_deref())
            .collect::<std::collections::HashSet<_>>();
        for entry in &decoded_entries {
            let base = entry.base();
            prop_assert!(base.id.as_deref().is_some_and(|id| ids.contains(id)));
            if let Some(parent_id) = base.parent_id.as_deref() {
                prop_assert!(parent_id == "root" || ids.contains(parent_id));
            }
        }

        let leaf_id = decoded_entries.last().and_then(SessionEntry::base_id).cloned();
        if let Some(ref leaf_id) = leaf_id {
            prop_assert!(ids.contains(leaf_id.as_str()));
        }

        let mut session = Session::create();
        session.header = SessionHeader {
            r#type: "session".to_string(),
            version: Some(pi::session::SESSION_VERSION),
            id: "sess-proptest".to_string(),
            timestamp: "2026-02-03T00:00:00.000Z".to_string(),
            cwd: "/tmp/project".to_string(),
            provider: None,
            model_id: None,
            thinking_level: None,
            fallback_provider: None,
            fallback_model_id: None,
            fallback_thinking_level: None,
            current_leaf: None,
            parent_session: None,
            additional_roots: None,
        };
        session.entries = decoded_entries;
        session._test_set_leaf_id(leaf_id);

        if let Some(leaf_id) = session.leaf_id().map(str::to_string) {
            // Path should always end at the requested entry.
            let path = session.get_path_to_entry(&leaf_id);
            prop_assert_eq!(path.last().map(String::as_str), Some(leaf_id.as_str()));
        }

        let _ = session.entries_for_current_path();
        let _ = session.list_leaves();
    }
}

#[test]
fn load_session_accepts_parent_session_alias_and_fills_ids() {
    run_async_test(async {
        let harness = TestHarness::new("load_session_accepts_parent_session_alias_and_fills_ids");
        let jsonl = r#"
{"type":"session","version":3,"id":"sess-123","timestamp":"2026-02-03T00:00:00.000Z","cwd":"/tmp/project","provider":"anthropic","modelId":"claude-sonnet-4-20250514","thinkingLevel":"medium","parentSession":"/tmp/parent.jsonl"}
{"type":"message","parentId":"root","timestamp":"2026-02-03T00:00:01.000Z","message":{"role":"user","content":"Hello","timestamp":1706918401000}}
{"type":"model_change","timestamp":"2026-02-03T00:00:02.000Z","provider":"anthropic","modelId":"claude-sonnet-4-20250514"}
{"type":"thinking_level_change","timestamp":"2026-02-03T00:00:03.000Z","thinkingLevel":"medium"}
{"type":"compaction","timestamp":"2026-02-03T00:00:04.000Z","summary":"compacted","firstKeptEntryId":"a1b2c3d4","tokensBefore":128}
{"type":"branch_summary","timestamp":"2026-02-03T00:00:05.000Z","fromId":"root","summary":"branch summary"}
{"type":"label","timestamp":"2026-02-03T00:00:06.000Z","targetId":"a1b2c3d4","label":"checkpoint"}
{"type":"session_info","timestamp":"2026-02-03T00:00:07.000Z","name":"demo session"}
{"type":"custom","timestamp":"2026-02-03T00:00:08.000Z","customType":"note","data":{"tag":"demo"}}
"#;

        let path = write_session_file(&harness, jsonl.trim_start());
        let session = Session::open(path.to_string_lossy().as_ref())
            .await
            .expect("open session");

        assert_eq!(
            session.header.parent_session.as_deref(),
            Some("/tmp/parent.jsonl")
        );
        assert_eq!(session.entries.len(), 8);
        assert!(
            session
                .entries
                .iter()
                .all(|entry| entry.base().id.as_ref().is_some())
        );

        let leaf = session.leaf_id();
        let last_id = session
            .entries
            .last()
            .and_then(SessionEntry::base_id)
            .map(String::as_str);
        assert_eq!(leaf, last_id);
    });
}

#[test]
fn session_header_serializes_branched_from_field() {
    run_async_test(async {
        let _harness = TestHarness::new("session_header_serializes_branched_from_field");
        let mut session = Session::create();
        session.header.parent_session = Some("/tmp/parent.jsonl".to_string());

        let header_json = serde_json::to_string(&session.header).expect("serialize header");
        assert!(header_json.contains("\"branchedFrom\""));
        assert!(!header_json.contains("parentSession"));
    });
}

#[test]
fn open_missing_session_returns_session_not_found_error() {
    run_async_test(async {
        let harness = TestHarness::new("open_missing_session_returns_session_not_found_error");
        let missing = harness.temp_path("missing.jsonl");
        harness
            .log()
            .info("setup", "Attempting to open missing file");

        let err = open_session(&missing).await.expect_err("expected error");
        assert!(
            matches!(err, Error::SessionNotFound { .. }),
            "Expected SessionNotFound, got: {err}"
        );
    });
}

#[test]
fn open_empty_session_file_errors() {
    run_async_test(async {
        let harness = TestHarness::new("open_empty_session_file_errors");
        let path = harness.create_file("empty.jsonl", "");
        harness.record_artifact("empty.jsonl", &path);

        let err = open_session(&path).await.expect_err("expected error");
        assert!(
            matches!(err, Error::Session(_)),
            "Expected Session error, got: {err}"
        );
    });
}

#[test]
fn open_header_only_session_succeeds_with_no_entries() {
    run_async_test(async {
        let harness = TestHarness::new("open_header_only_session_succeeds_with_no_entries");
        let header = SessionHeader::new();
        let jsonl = format!(
            "{}\n",
            serde_json::to_string(&header).expect("serialize header")
        );
        let path = harness.create_file("header_only.jsonl", jsonl);
        harness.record_artifact("header_only.jsonl", &path);

        let loaded = open_session(&path).await.expect("open header-only session");
        assert!(loaded.entries.is_empty());
        assert!(loaded.leaf_id().is_none());
    });
}

#[test]
fn save_creates_path_under_override_dir() {
    run_async_test(async {
        let harness = TestHarness::new("save_creates_path_under_override_dir");
        let base_dir = harness.temp_path("sessions");
        let mut session = Session::create_with_dir(Some(base_dir.clone()));
        session.append_message(make_user_message("Hello"));

        session.save().await.expect("save session");
        let path = session.path.clone().expect("session path set");
        harness.record_artifact("session.jsonl", &path);

        assert!(path.exists(), "Expected session file to exist");

        let cwd = std::env::current_dir().expect("current_dir");
        let expected_prefix = base_dir.join(encode_cwd(&cwd));
        assert!(
            path.starts_with(&expected_prefix),
            "Expected session path {path:?} to start with {expected_prefix:?}"
        );
    });
}

#[test]
fn plan_fork_from_user_message_branches_from_parent_and_returns_selected_text() {
    let mut session = Session::create();
    let root_user = session.append_message(make_user_message("Hello"));
    let root_assistant = session.append_message(make_assistant_message("Hi"));
    let selected_user = session.append_message(make_user_message("Question"));
    let _selected_assistant = session.append_message(make_assistant_message("Answer"));

    let plan = session
        .plan_fork_from_user_message(&selected_user)
        .expect("plan fork");

    assert_eq!(plan.selected_text, "Question");
    assert_eq!(plan.leaf_id.as_deref(), Some(root_assistant.as_str()));

    assert_eq!(plan.entries.len(), 2);
    assert_eq!(
        plan.entries[0].base_id().map(String::as_str),
        Some(root_user.as_str())
    );
    assert_eq!(
        plan.entries[1].base_id().map(String::as_str),
        Some(root_assistant.as_str())
    );

    let pi::session::ForkPlan {
        entries, leaf_id, ..
    } = plan;
    let mut forked = Session::create();
    forked.entries = entries;
    forked._test_set_leaf_id(leaf_id);

    let appended = forked.append_message(make_user_message("Followup"));
    let appended_entry = forked.get_entry(&appended).expect("appended entry");
    assert_eq!(
        appended_entry.base().parent_id.as_deref(),
        Some(root_assistant.as_str())
    );
}

#[test]
fn plan_fork_from_root_user_message_creates_empty_session() {
    let mut session = Session::create();
    let root_user = session.append_message(make_user_message("Root"));
    let _assistant = session.append_message(make_assistant_message("Hi"));

    let plan = session
        .plan_fork_from_user_message(&root_user)
        .expect("plan fork");

    assert_eq!(plan.selected_text, "Root");
    assert!(plan.entries.is_empty());
    assert!(plan.leaf_id.is_none());
}

#[test]
fn plan_fork_from_non_user_message_errors() {
    let mut session = Session::create();
    let _root_user = session.append_message(make_user_message("Hello"));
    let assistant_id = session.append_message(make_assistant_message("Hi"));

    let err = session
        .plan_fork_from_user_message(&assistant_id)
        .expect_err("expected error");

    assert!(matches!(err, Error::Session(_)), "unexpected error: {err}");
}

#[test]
fn save_updates_session_index_for_override_dir() {
    run_async_test(async {
        let harness = TestHarness::new("save_updates_session_index_for_override_dir");
        let base_dir = harness.temp_path("sessions");
        let mut session = Session::create_with_dir(Some(base_dir.clone()));
        session.append_message(make_user_message("Hello"));

        session.save().await.expect("save session");

        let index = SessionIndex::for_sessions_root(&base_dir);
        let indexed = index
            .list_sessions(Some(&session.header.cwd))
            .expect("list sessions")
            .into_iter()
            .any(|meta| meta.id == session.header.id);

        harness.assert_log("Session saved and indexed");
        assert!(indexed, "Expected session to be indexed after save()");
    });
}

#[test]
fn save_and_open_round_trip_linear_messages_preserves_leaf_id() {
    run_async_test(async {
        let harness =
            TestHarness::new("save_and_open_round_trip_linear_messages_preserves_leaf_id");
        let base_dir = harness.temp_path("sessions");
        let mut session = Session::create_with_dir(Some(base_dir));

        let id1 = session.append_message(make_user_message("Hello"));
        let id2 = session.append_message(make_assistant_message("Hi there!"));

        let loaded = save_and_reopen(&harness, &mut session).await;

        assert_eq!(loaded.entries.len(), 2);
        assert_eq!(loaded.leaf_id(), Some(id2.as_str()));

        let path = loaded.get_path_to_entry(&id2);
        assert_eq!(path, vec![id1, id2]);
    });
}

#[test]
fn save_and_open_round_trip_branching_preserves_leaves_and_branch_point() {
    run_async_test(async {
        let harness = TestHarness::new(
            "save_and_open_round_trip_branching_preserves_leaves_and_branch_point",
        );
        let base_dir = harness.temp_path("sessions");
        let mut session = Session::create_with_dir(Some(base_dir));

        let root = session.append_message(make_user_message("Root"));
        let leaf_a = session.append_message(make_assistant_message("Branch A"));
        assert!(session.create_branch_from(&root));
        let leaf_b = session.append_message(make_assistant_message("Branch B"));

        let loaded = save_and_reopen(&harness, &mut session).await;
        let summary = loaded.branch_summary();

        let leaves = summary
            .leaves
            .into_iter()
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(
            leaves,
            [leaf_a.clone(), leaf_b.clone()]
                .into_iter()
                .collect::<std::collections::HashSet<_>>()
        );
        assert!(
            summary.branch_points.contains(&root),
            "Expected root entry to be a branch point"
        );
    });
}

#[test]
fn open_skips_corrupted_entries_and_keeps_valid_ones() {
    run_async_test(async {
        let harness = TestHarness::new("open_skips_corrupted_entries_and_keeps_valid_ones");
        let base_dir = harness.temp_path("sessions");
        let mut session = Session::create_with_dir(Some(base_dir));
        session.append_message(make_user_message("Hello"));
        session.append_message(make_assistant_message("World"));

        session.save().await.expect("save session");
        let original_path = session.path.clone().expect("session path set");
        harness.record_artifact("original.jsonl", &original_path);

        let original = std::fs::read_to_string(&original_path).expect("read session");
        let mut lines: Vec<&str> = original.lines().collect();
        assert!(lines.len() >= 2);
        lines.insert(2.min(lines.len()), "{ this is not json }");

        let corrupted_path = harness.temp_path("corrupted.jsonl");
        std::fs::write(&corrupted_path, format!("{}\n", lines.join("\n")))
            .expect("write corrupted session");
        harness.record_artifact("corrupted.jsonl", &corrupted_path);

        let loaded = open_session(&corrupted_path)
            .await
            .expect("open corrupted session");
        assert_eq!(loaded.entries.len(), session.entries.len());
    });
}

#[test]
fn save_round_trips_model_change_entry() {
    run_async_test(async {
        let harness = TestHarness::new("save_round_trips_model_change_entry");
        let base_dir = harness.temp_path("sessions");
        let mut session = Session::create_with_dir(Some(base_dir));
        session.append_message(make_user_message("Hello"));
        session.append_model_change("anthropic".to_string(), "claude-test".to_string());

        let loaded = save_and_reopen(&harness, &mut session).await;
        assert_contains_entry(
            &loaded.entries,
            |entry| matches!(entry, SessionEntry::ModelChange(change) if change.provider == "anthropic" && change.model_id == "claude-test"),
            "ModelChange(provider=anthropic, model_id=claude-test)",
        );
    });
}

#[test]
fn save_round_trips_thinking_level_change_entry() {
    run_async_test(async {
        let harness = TestHarness::new("save_round_trips_thinking_level_change_entry");
        let base_dir = harness.temp_path("sessions");
        let mut session = Session::create_with_dir(Some(base_dir));
        session.append_message(make_user_message("Hello"));
        session.append_thinking_level_change("high".to_string());

        let loaded = save_and_reopen(&harness, &mut session).await;
        assert_contains_entry(
            &loaded.entries,
            |entry| {
                matches!(
                    entry,
                    SessionEntry::ThinkingLevelChange(change) if change.thinking_level == "high"
                )
            },
            "ThinkingLevelChange(thinking_level=high)",
        );
    });
}

#[test]
fn save_round_trips_session_info_entry() {
    run_async_test(async {
        let harness = TestHarness::new("save_round_trips_session_info_entry");
        let base_dir = harness.temp_path("sessions");
        let mut session = Session::create_with_dir(Some(base_dir));
        session.append_message(make_user_message("Hello"));
        session.append_session_info(Some("demo session".to_string()));

        let loaded = save_and_reopen(&harness, &mut session).await;
        assert_contains_entry(
            &loaded.entries,
            |entry| matches!(entry, SessionEntry::SessionInfo(info) if info.name.as_deref() == Some("demo session")),
            "SessionInfo(name=demo session)",
        );
    });
}

#[test]
fn save_round_trips_label_entry() {
    run_async_test(async {
        let harness = TestHarness::new("save_round_trips_label_entry");
        let base_dir = harness.temp_path("sessions");
        let mut session = Session::create_with_dir(Some(base_dir));
        let target_id = session.append_message(make_user_message("Hello"));
        let label_id = session
            .add_label(&target_id, Some("checkpoint".to_string()))
            .expect("label created");

        let loaded = save_and_reopen(&harness, &mut session).await;
        assert_contains_entry(
            &loaded.entries,
            |entry| {
                matches!(
                    entry,
                    SessionEntry::Label(label) if label.base.id.as_deref() == Some(label_id.as_str())
                        && label.target_id == target_id
                        && label.label.as_deref() == Some("checkpoint")
                )
            },
            "Label(target_id=..., label=checkpoint)",
        );
    });
}

#[test]
fn save_round_trips_compaction_entry() {
    run_async_test(async {
        let harness = TestHarness::new("save_round_trips_compaction_entry");
        let base_dir = harness.temp_path("sessions");
        let mut session = Session::create_with_dir(Some(base_dir));
        let first = session.append_message(make_user_message("Hello"));
        let keep = session.append_message(make_user_message("Keep me"));
        session.append_message(make_assistant_message("Ignore me"));
        session.append_compaction(
            "compacted".to_string(),
            keep.clone(),
            128,
            Some(json!({"from":"test"})),
            Some(false),
        );

        let loaded = save_and_reopen(&harness, &mut session).await;
        assert_contains_entry(
            &loaded.entries,
            |entry| {
                matches!(
                    entry,
                    SessionEntry::Compaction(compaction)
                        if compaction.summary == "compacted"
                            && compaction.first_kept_entry_id == keep
                            && compaction.tokens_before == 128
                            && compaction.details.as_ref().is_some_and(|v| v.get("from").and_then(|v| v.as_str()) == Some("test"))
                            && compaction.from_hook == Some(false)
                )
            },
            "Compaction(summary=compacted, firstKeptEntryId=..., tokensBefore=128)",
        );

        // Sanity: first entry remains in history, even if omitted from current path context.
        assert_contains_entry(
            &loaded.entries,
            |entry| entry.base().id.as_deref() == Some(first.as_str()),
            "first message entry id",
        );
    });
}

#[test]
fn save_round_trips_branch_summary_entry() {
    run_async_test(async {
        let harness = TestHarness::new("save_round_trips_branch_summary_entry");
        let base_dir = harness.temp_path("sessions");
        let mut session = Session::create_with_dir(Some(base_dir));
        let root = session.append_message(make_user_message("Hello"));
        session.append_branch_summary(root.clone(), "branch summary".to_string(), None, None);

        let loaded = save_and_reopen(&harness, &mut session).await;
        assert_contains_entry(
            &loaded.entries,
            |entry| {
                matches!(
                    entry,
                    SessionEntry::BranchSummary(summary)
                        if summary.from_id == root && summary.summary == "branch summary"
                )
            },
            "BranchSummary(from_id=..., summary=branch summary)",
        );
    });
}

#[test]
fn save_round_trips_custom_entry() {
    run_async_test(async {
        let harness = TestHarness::new("save_round_trips_custom_entry");
        let base_dir = harness.temp_path("sessions");
        let mut session = Session::create_with_dir(Some(base_dir));
        session.append_message(make_user_message("Hello"));

        let custom_id = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
        let base = EntryBase::new(session.leaf_id().map(str::to_string), custom_id.clone());
        session.entries.push(SessionEntry::Custom(CustomEntry {
            base,
            custom_type: "note".to_string(),
            data: Some(json!({"tag":"demo"})),
        }));
        session._test_set_leaf_id(Some(custom_id));

        let loaded = save_and_reopen(&harness, &mut session).await;
        assert_contains_entry(
            &loaded.entries,
            |entry| {
                matches!(
                    entry,
                    SessionEntry::Custom(custom)
                        if custom.custom_type == "note"
                            && custom.data.as_ref().is_some_and(|v| v.get("tag").and_then(|v| v.as_str()) == Some("demo"))
                )
            },
            "Custom(customType=note, data.tag=demo)",
        );
    });
}

#[test]
fn concurrent_saves_do_not_corrupt_session_file() {
    let harness = TestHarness::new("concurrent_saves_do_not_corrupt_session_file");
    let base_dir = harness.temp_path("sessions");

    let mut session = Session::create_with_dir(Some(base_dir));
    session.append_message(make_user_message("Hello"));

    run_async_test(async {
        session.save().await.expect("initial save");
    });
    let path = session.path.clone().expect("session path set");
    harness.record_artifact("session.jsonl", &path);

    let path1 = path.clone();
    let path2 = path.clone();

    let t1 = std::thread::spawn(move || {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async move {
            let mut s = Session::open(path1.to_string_lossy().as_ref())
                .await
                .expect("open session");
            s.append_message(make_user_message("From thread 1"));
            s.save().await
        })
    });

    let t2 = std::thread::spawn(move || {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async move {
            let mut s = Session::open(path2.to_string_lossy().as_ref())
                .await
                .expect("open session");
            s.append_message(make_user_message("From thread 2"));
            s.save().await
        })
    });

    let r1 = t1.join().expect("thread 1 join");
    let r2 = t2.join().expect("thread 2 join");

    assert!(
        r1.is_ok() || r2.is_ok(),
        "Expected at least one save to succeed: r1={r1:?} r2={r2:?}"
    );

    run_async_test(async {
        let loaded = Session::open(path.to_string_lossy().as_ref())
            .await
            .expect("open after concurrent saves");
        assert!(!loaded.entries.is_empty());
    });
}

// ============================================================================
// Header Metadata Persistence Tests (bd-ah1)
// ============================================================================

#[test]
fn save_preserves_header_provider_and_model() {
    run_async_test(async {
        let harness = TestHarness::new("save_preserves_header_provider_and_model");
        let base_dir = harness.temp_path("sessions");
        let mut session = Session::create_with_dir(Some(base_dir));

        session.header.provider = Some("anthropic".to_string());
        session.header.model_id = Some("claude-opus-4".to_string());
        session.header.thinking_level = Some("high".to_string());
        session.append_message(make_user_message("Hello"));

        let loaded = save_and_reopen(&harness, &mut session).await;

        harness.log().info(
            "header",
            format!(
                "Provider: {:?}, Model: {:?}, Thinking: {:?}",
                loaded.header.provider, loaded.header.model_id, loaded.header.thinking_level
            ),
        );

        assert_eq!(
            loaded.header.provider.as_deref(),
            Some("anthropic"),
            "Provider should be preserved"
        );
        assert_eq!(
            loaded.header.model_id.as_deref(),
            Some("claude-opus-4"),
            "Model ID should be preserved"
        );
        assert_eq!(
            loaded.header.thinking_level.as_deref(),
            Some("high"),
            "Thinking level should be preserved"
        );
    });
}

#[test]
fn save_preserves_header_cwd() {
    run_async_test(async {
        let harness = TestHarness::new("save_preserves_header_cwd");
        let base_dir = harness.temp_path("sessions");
        let mut session = Session::create_with_dir(Some(base_dir));

        let original_cwd = session.header.cwd.clone();
        session.append_message(make_user_message("Hello"));

        let loaded = save_and_reopen(&harness, &mut session).await;

        harness
            .log()
            .info("header", format!("CWD: {}", loaded.header.cwd));

        assert_eq!(
            loaded.header.cwd, original_cwd,
            "CWD should be preserved across save/reload"
        );
    });
}

// ============================================================================
// Tool Message Persistence Tests (bd-ah1)
// ============================================================================

fn make_tool_result_message(
    tool_call_id: &str,
    tool_name: &str,
    result_text: &str,
    is_error: bool,
) -> pi::session::SessionMessage {
    pi::session::SessionMessage::ToolResult {
        tool_call_id: tool_call_id.to_string(),
        tool_name: tool_name.to_string(),
        content: vec![ContentBlock::Text(TextContent::new(result_text))],
        details: None,
        is_error,
        timestamp: Some(0),
    }
}

fn make_assistant_with_tool_use(
    tool_call_id: &str,
    tool_name: &str,
) -> pi::session::SessionMessage {
    use pi::model::ToolCall;

    pi::session::SessionMessage::Assistant {
        message: AssistantMessage {
            content: vec![ContentBlock::ToolCall(ToolCall {
                id: tool_call_id.to_string(),
                name: tool_name.to_string(),
                arguments: serde_json::json!({"path": "/test/file"}),
                thought_signature: None,
            })],
            api: "test".to_string(),
            provider: "test".to_string(),
            model: "test".to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            stop_details: None,
            error_message: None,
            timestamp: 0,
        },
    }
}

#[test]
fn save_round_trips_tool_result_message() {
    run_async_test(async {
        let harness = TestHarness::new("save_round_trips_tool_result_message");
        let base_dir = harness.temp_path("sessions");
        let mut session = Session::create_with_dir(Some(base_dir));

        session.append_message(make_user_message("Read /test/file"));
        let tool_call_id = "tool_abc123";
        session.append_message(make_assistant_with_tool_use(tool_call_id, "read_file"));
        session.append_message(make_tool_result_message(
            tool_call_id,
            "read_file",
            "file contents here",
            false,
        ));

        let loaded = save_and_reopen(&harness, &mut session).await;

        harness
            .log()
            .info("entries", format!("Entry count: {}", loaded.entries.len()));

        assert_eq!(loaded.entries.len(), 3, "Should have 3 entries");

        // Verify tool result entry
        let tool_result_entry = &loaded.entries[2];
        if let SessionEntry::Message(msg_entry) = tool_result_entry {
            if let pi::session::SessionMessage::ToolResult {
                tool_call_id: id,
                tool_name: name,
                is_error,
                ..
            } = &msg_entry.message
            {
                assert_eq!(id, "tool_abc123");
                assert_eq!(name, "read_file");
                assert!(!is_error);
            } else {
                let got = &msg_entry.message;
                panic!("Expected ToolResult message, got {got:?}");
            }
        } else {
            panic!("Expected Message entry, got {tool_result_entry:?}");
        }
    });
}

#[test]
fn save_round_trips_tool_error_result() {
    run_async_test(async {
        let harness = TestHarness::new("save_round_trips_tool_error_result");
        let base_dir = harness.temp_path("sessions");
        let mut session = Session::create_with_dir(Some(base_dir));

        session.append_message(make_user_message("Read /nonexistent"));
        session.append_message(make_assistant_with_tool_use("tool_err1", "read_file"));
        session.append_message(make_tool_result_message(
            "tool_err1",
            "read_file",
            "File not found: /nonexistent",
            true,
        ));

        let loaded = save_and_reopen(&harness, &mut session).await;

        let tool_result_entry = &loaded.entries[2];
        if let SessionEntry::Message(msg_entry) = tool_result_entry {
            if let pi::session::SessionMessage::ToolResult { is_error, .. } = &msg_entry.message {
                assert!(*is_error, "is_error flag should be preserved as true");
            } else {
                panic!("Expected ToolResult message");
            }
        } else {
            panic!("Expected Message entry");
        }
    });
}

// ============================================================================
// Unicode Content Persistence Tests (bd-ah1)
// ============================================================================

#[test]
fn save_preserves_unicode_message_content() {
    run_async_test(async {
        let harness = TestHarness::new("save_preserves_unicode_message_content");
        let base_dir = harness.temp_path("sessions");
        let mut session = Session::create_with_dir(Some(base_dir));

        let unicode_content = "Hello 你好 שלום مرحبا 🎉🚀💻";
        session.append_message(make_user_message(unicode_content));

        let loaded = save_and_reopen(&harness, &mut session).await;

        let first_entry = &loaded.entries[0];
        if let SessionEntry::Message(msg_entry) = first_entry {
            if let pi::session::SessionMessage::User { content, .. } = &msg_entry.message {
                let text = match content {
                    UserContent::Text(t) => t.as_str(),
                    UserContent::Blocks(_) => panic!("Expected Text content"),
                };
                assert_eq!(
                    text, unicode_content,
                    "Unicode content should be preserved exactly"
                );
            } else {
                panic!("Expected User message");
            }
        } else {
            panic!("Expected Message entry");
        }
    });
}

#[test]
fn save_preserves_emoji_in_assistant_response() {
    run_async_test(async {
        let harness = TestHarness::new("save_preserves_emoji_in_assistant_response");
        let base_dir = harness.temp_path("sessions");
        let mut session = Session::create_with_dir(Some(base_dir));

        let emoji_text = "Here's a rocket 🚀 and some stars ⭐✨!";
        session.append_message(make_user_message("Send emojis"));
        session.append_message(make_assistant_message(emoji_text));

        let loaded = save_and_reopen(&harness, &mut session).await;

        let assistant_entry = &loaded.entries[1];
        if let SessionEntry::Message(msg_entry) = assistant_entry {
            if let pi::session::SessionMessage::Assistant { message } = &msg_entry.message {
                if let Some(ContentBlock::Text(text_block)) = message.content.first() {
                    assert_eq!(
                        text_block.text, emoji_text,
                        "Emoji content should be preserved"
                    );
                } else {
                    panic!("Expected Text content block");
                }
            } else {
                panic!("Expected Assistant message");
            }
        } else {
            panic!("Expected Message entry");
        }
    });
}

// ============================================================================
// Timestamp Preservation Tests (bd-ah1)
// ============================================================================

#[test]
fn save_preserves_message_timestamps() {
    run_async_test(async {
        let harness = TestHarness::new("save_preserves_message_timestamps");
        let base_dir = harness.temp_path("sessions");
        let mut session = Session::create_with_dir(Some(base_dir));

        let timestamp = 1_706_918_401_000_i64;
        session.append_message(pi::session::SessionMessage::User {
            content: UserContent::Text("Hello".to_string()),
            timestamp: Some(timestamp),
        });

        let loaded = save_and_reopen(&harness, &mut session).await;

        let first_entry = &loaded.entries[0];
        if let SessionEntry::Message(msg_entry) = first_entry {
            if let pi::session::SessionMessage::User {
                timestamp: ts_opt, ..
            } = &msg_entry.message
            {
                assert_eq!(
                    *ts_opt,
                    Some(timestamp),
                    "Message timestamp should be preserved"
                );
            } else {
                panic!("Expected User message");
            }
        } else {
            panic!("Expected Message entry");
        }
    });
}
