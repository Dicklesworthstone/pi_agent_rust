//! Real-bank and native-tool regression coverage for session-shared memory.

use super::shared::{MAX_VALUE_BYTES, SharedMemoryStore, SharedMemoryTool};
use super::{MemoryStore, RecallTool, RetainTool};
use crate::jobs::JobSessionScope;
use crate::tools::{Tool, ToolRegistry};
use serde_json::json;
use std::path::Path;
use std::sync::{Arc, Barrier, Mutex};

fn bank(root: &Path) -> Arc<MemoryStore> {
    Arc::new(MemoryStore {
        db_path: root.join("bank.sqlite"),
        project_key: "shared-test".to_string(),
        project_root: root.to_path_buf(),
        secret_patterns: Vec::new(),
    })
}

fn run<F: std::future::Future>(future: F) -> F::Output {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap()
        .block_on(future)
}

#[test]
fn same_session_reopens_exact_values_without_polluting_project_memories() {
    let dir = tempfile::tempdir().unwrap();
    let project = bank(dir.path());
    let shared = SharedMemoryStore::new(Arc::clone(&project), "session-a").unwrap();
    let content = "  first line\n日本語\n\n";
    let written = shared.write("handoff", content, Some("absent")).unwrap();
    let reopened = SharedMemoryStore::new(bank(dir.path()), "session-a").unwrap();
    let value = reopened.read("handoff").unwrap().unwrap();
    assert_eq!(value.content, content);
    assert_eq!(value.version.bytes, content.len());
    assert_eq!(value.version.revision, written.revision);
    assert!(project.list(10).unwrap().is_empty());
    assert!(project.recall("first", None).unwrap().is_empty());
    assert!(project.mental_model().unwrap().is_empty());
}

#[test]
fn sessions_and_projects_are_isolated_even_with_the_same_key() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let first = SharedMemoryStore::new(bank(a.path()), "session-a").unwrap();
    let other_session = SharedMemoryStore::new(bank(a.path()), "session-b").unwrap();
    let other_project = SharedMemoryStore::new(bank(b.path()), "session-a").unwrap();
    first.write("handoff", "only A", None).unwrap();
    for other in [&other_session, &other_project] {
        assert!(other.read("handoff").unwrap().is_none());
        assert!(other.list("", None, 10).unwrap().entries.is_empty());
    }
    other_session.write("handoff", "only B", None).unwrap();
    assert_eq!(first.read("handoff").unwrap().unwrap().content, "only A");
    assert_eq!(
        other_session.read("handoff").unwrap().unwrap().content,
        "only B"
    );
}

#[test]
fn create_only_and_revision_checks_reject_stale_writes_including_aba() {
    let dir = tempfile::tempdir().unwrap();
    let shared = SharedMemoryStore::new(bank(dir.path()), "session").unwrap();
    let first = shared.write("plan", "original", Some("absent")).unwrap();
    assert!(
        shared
            .write("plan", "must not replace", Some("absent"))
            .unwrap_err()
            .to_string()
            .contains("PI_SHARED_MEMORY_CONFLICT")
    );
    let second = shared
        .write("plan", "changed", Some(&first.revision))
        .unwrap();
    let third = shared
        .write("plan", "original", Some(&second.revision))
        .unwrap();
    assert_ne!(first.revision, third.revision);
    assert!(
        shared
            .write("plan", "stale", Some(&first.revision))
            .unwrap_err()
            .to_string()
            .contains("PI_SHARED_MEMORY_CONFLICT")
    );
    let fourth = shared
        .write("plan", "original", Some(&third.revision))
        .unwrap();
    assert_ne!(
        third.revision, fourth.revision,
        "same-content writes still invalidate stale revisions"
    );
    assert_eq!(shared.read("plan").unwrap().unwrap().content, "original");
    let blank = shared.write("plan", "", Some(&fourth.revision)).unwrap();
    assert_eq!(blank.bytes, 0);
    assert_eq!(shared.read("plan").unwrap().unwrap().content, "");
}

#[test]
fn competing_agents_cannot_both_replace_the_same_revision() {
    let dir = tempfile::tempdir().unwrap();
    let shared = SharedMemoryStore::new(bank(dir.path()), "session").unwrap();
    let first = shared.write("plan", "original", None).unwrap();
    let barrier = Barrier::new(2);
    let results = std::thread::scope(|scope| {
        let handles: Vec<_> = ["worker-a", "worker-b"]
            .into_iter()
            .map(|content| {
                let shared = shared.clone();
                let revision = &first.revision;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    shared.write("plan", content, Some(revision))
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    let error = results
        .iter()
        .find_map(|result| result.as_ref().err())
        .unwrap();
    assert!(
        error.to_string().contains("PI_SHARED_MEMORY_CONFLICT"),
        "{error}"
    );
    let current = shared.read("plan").unwrap().unwrap();
    let winner = results
        .iter()
        .find_map(|result| result.as_ref().ok())
        .unwrap();
    assert_eq!(current.version.revision, winner.revision);
    assert!(matches!(current.content.as_str(), "worker-a" | "worker-b"));
}

#[test]
fn key_validation_precedes_storage_and_never_echoes_the_key() {
    let dir = tempfile::tempdir().unwrap();
    let shared = SharedMemoryStore::new(bank(dir.path()), "session").unwrap();
    for key in [
        "",
        "..",
        "/etc/passwd",
        "a/b",
        "a\\b",
        "a\0b",
        "a%",
        "é",
        "x';DROP TABLE memories;--",
    ] {
        let error = shared
            .write(key, "sensitive value", None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("PI_SHARED_MEMORY_INVALID_KEY"));
        assert!(!error.contains("sensitive value"));
        if key.len() > 2 {
            assert!(!error.contains(key));
        }
        assert!(shared.read(key).is_err());
    }
    assert!(shared.write(&"a".repeat(129), "v", None).is_err());
    assert!(!dir.path().join("bank.sqlite").exists());
    shared.write(&"a".repeat(128), "v", None).unwrap();
}

#[test]
fn literal_prefix_and_key_cursors_do_not_skip_or_duplicate_entries() {
    let dir = tempfile::tempdir().unwrap();
    let shared = SharedMemoryStore::new(bank(dir.path()), "session").unwrap();
    for key in ["a_two", "aa", "a_one", "z"] {
        shared.write(key, &"🦀".repeat(200), None).unwrap();
    }
    let first = shared.list("a_", None, 1).unwrap();
    assert_eq!(first.entries[0].version.key, "a_one");
    assert_eq!(first.entries[0].preview.chars().count(), 160);
    assert_eq!(first.next_cursor.as_deref(), Some("a_one"));
    let second = shared.list("a_", first.next_cursor.as_deref(), 1).unwrap();
    assert_eq!(second.entries[0].version.key, "a_two");
    assert!(second.next_cursor.is_none());
    assert_eq!(shared.list("", None, 50).unwrap().entries.len(), 4);
    assert!(shared.list("", None, 0).is_err());
    assert!(shared.list("", None, 51).is_err());
}

#[test]
fn oversize_values_and_invalid_revisions_do_not_replace_existing_data() {
    let dir = tempfile::tempdir().unwrap();
    let shared = SharedMemoryStore::new(bank(dir.path()), "session").unwrap();
    let original = shared.write("plan", "kept", None).unwrap();
    assert!(
        shared
            .write("plan", &"x".repeat(MAX_VALUE_BYTES + 1), None)
            .unwrap_err()
            .to_string()
            .contains("PI_SHARED_MEMORY_VALUE_LIMIT")
    );
    assert!(
        shared
            .write("plan", "lost", Some("invalid-revision"))
            .unwrap_err()
            .to_string()
            .contains("PI_SHARED_MEMORY_INVALID_REVISION")
    );
    assert_eq!(
        shared.read("plan").unwrap().unwrap().version.revision,
        original.revision
    );
    assert_eq!(shared.read("plan").unwrap().unwrap().content, "kept");
}

#[test]
fn byte_quota_is_per_session_and_replacement_reclaims_its_previous_size() {
    let dir = tempfile::tempdir().unwrap();
    let shared = SharedMemoryStore::new(bank(dir.path()), "session-a").unwrap();
    let value = "x".repeat(MAX_VALUE_BYTES);
    for index in 0..64 {
        shared
            .write(&format!("part-{index}"), &value, None)
            .unwrap();
    }
    assert!(
        shared
            .write("overflow", "x", None)
            .unwrap_err()
            .to_string()
            .contains("PI_SHARED_MEMORY_CAPACITY")
    );
    assert!(shared.read("overflow").unwrap().is_none());
    shared.write("part-0", "", None).unwrap();
    shared.write("overflow", &value, None).unwrap();
    let other = SharedMemoryStore::new(bank(dir.path()), "session-b").unwrap();
    other
        .write("plan", "another session has its own quota", None)
        .unwrap();
}

#[test]
fn bound_tools_follow_switches_and_reject_model_supplied_session_authority() {
    let dir = tempfile::tempdir().unwrap();
    let project = bank(dir.path());
    let active = Arc::new(Mutex::new("session-a".to_string()));
    let scope = JobSessionScope::fixed("unused");
    let current = Arc::clone(&active);
    scope.bind(Arc::new(move || {
        let id = current.lock().unwrap().clone();
        Box::pin(async move { Some(id) })
    }));
    let mut retain = RetainTool::new(Arc::clone(&project));
    let mut recall = RecallTool::new(Arc::clone(&project));
    retain.bind_job_session_scope(scope.clone());
    recall.bind_job_session_scope(scope.clone());
    run(async {
        let written = retain
            .execute(
                "a",
                json!({"scope":"session","key":"plan","content":"A","expectedRevision":"absent"}),
                None,
            )
            .await
            .unwrap();
        assert!(!written.is_error);
        let read = recall
            .execute("b", json!({"scope":"session","key":"plan"}), None)
            .await
            .unwrap();
        assert_eq!(read.details.unwrap()["value"]["content"], "A");
        *active.lock().unwrap() = "session-b".to_string();
        let missing = recall
            .execute("c", json!({"scope":"session","key":"plan"}), None)
            .await
            .unwrap_err();
        assert!(missing.to_string().contains("PI_SHARED_MEMORY_NOT_FOUND"));
        retain
            .execute(
                "d",
                json!({"scope":"session","key":"plan","content":"B"}),
                None,
            )
            .await
            .unwrap();
        let injected = retain
            .execute(
                "e",
                json!({"scope":"session","sessionId":"session-a","key":"plan","content":"attack"}),
                None,
            )
            .await
            .unwrap_err();
        assert!(
            injected
                .to_string()
                .contains("PI_SHARED_MEMORY_INVALID_INPUT")
        );
        *active.lock().unwrap() = "session-a".to_string();
        let read = recall
            .execute("f", json!({"scope":"session","key":"plan"}), None)
            .await
            .unwrap();
        assert_eq!(read.details.unwrap()["value"]["content"], "A");
        let listed = recall
            .execute("g", json!({"scope":"session"}), None)
            .await
            .unwrap();
        assert_eq!(
            listed.details.unwrap()["entries"].as_array().unwrap().len(),
            1
        );
        scope.bind(Arc::new(|| Box::pin(async { None })));
        let unavailable = recall
            .execute("h", json!({"scope":"session","key":"plan"}), None)
            .await
            .unwrap_err();
        assert!(
            unavailable
                .to_string()
                .contains("PI_SHARED_MEMORY_SESSION_UNAVAILABLE")
        );
    });
    assert!(project.list(10).unwrap().is_empty());
}

#[test]
fn dedicated_tools_interoperate_with_retain_recall_and_do_not_echo_write_content() {
    let dir = tempfile::tempdir().unwrap();
    let project = bank(dir.path());
    let scope = JobSessionScope::fixed("shared-session");
    let mut writer = SharedMemoryTool::write(Arc::clone(&project));
    let mut reader = SharedMemoryTool::read(Arc::clone(&project));
    let mut list = SharedMemoryTool::list(Arc::clone(&project));
    let mut recall = RecallTool::new(Arc::clone(&project));
    for tool in [
        &mut writer as &mut dyn Tool,
        &mut reader,
        &mut list,
        &mut recall,
    ] {
        tool.bind_job_session_scope(scope.clone());
    }
    assert_eq!(writer.name(), "write_memory");
    assert_eq!(reader.name(), "read_memory");
    assert_eq!(list.name(), "list_memory");
    assert!(writer.effects().writes());
    assert!(reader.effects().parallel_safe());
    run(async {
        let output = writer
            .execute(
                "w",
                json!({"key":"note","content":"sensitive shared body"}),
                None,
            )
            .await
            .unwrap();
        assert!(
            !serde_json::to_string(&output)
                .unwrap()
                .contains("sensitive shared body")
        );
        let output = reader
            .execute("r", json!({"key":"note"}), None)
            .await
            .unwrap();
        assert_eq!(
            output.details.unwrap()["value"]["content"],
            "sensitive shared body"
        );
        let output = recall
            .execute("r2", json!({"scope":"session","key":"note"}), None)
            .await
            .unwrap();
        assert_eq!(
            output.details.unwrap()["value"]["content"],
            "sensitive shared body"
        );
        assert_eq!(
            list.execute("l", json!({}), None)
                .await
                .unwrap()
                .details
                .unwrap()["entries"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(
            reader
                .execute("bad", json!({"key":null}), None)
                .await
                .is_err()
        );
    });
    let value = SharedMemoryStore::new(project, "shared-session")
        .unwrap()
        .read("note")
        .unwrap()
        .unwrap();
    assert!(!format!("{value:?}").contains("sensitive shared body"));
}

#[test]
fn unbound_shared_tools_fail_closed_but_project_memory_still_works() {
    let dir = tempfile::tempdir().unwrap();
    let project = bank(dir.path());
    let retain = RetainTool::new(Arc::clone(&project));
    run(async {
        let error = retain
            .execute(
                "x",
                json!({"scope":"session","key":"plan","content":"no scope"}),
                None,
            )
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("PI_SHARED_MEMORY_SESSION_UNAVAILABLE")
        );
        assert!(!dir.path().join("bank.sqlite").exists());
        let output = retain
            .execute("y", json!({"content":"a project fact"}), None)
            .await
            .unwrap();
        assert!(!output.is_error);
        assert!(
            retain
                .execute(
                    "z",
                    json!({"scope":"project","key":"plan","content":"ambiguous"}),
                    None
                )
                .await
                .is_err()
        );
    });
    assert_eq!(project.recall("project", None).unwrap().len(), 1);
}

#[test]
fn shared_values_survive_a_compaction_entry_and_bank_reopen() {
    use crate::model::UserContent;
    use crate::session::{Session, SessionMessage};

    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::in_memory();
    let kept = session.append_message(SessionMessage::User {
        content: UserContent::Text("continue this task".to_string()),
        timestamp: Some(1),
    });
    let project = bank(dir.path());
    let mut writer = RetainTool::new(project);
    writer.bind_job_session_scope(JobSessionScope::fixed(session.header.id.clone()));
    run(writer.execute(
        "w",
        json!({"scope":"session","key":"handoff","content":"inspect src/agent.rs next"}),
        None,
    ))
    .unwrap();
    session.append_compaction(
        "earlier work summarized".to_string(),
        kept,
        10000,
        None,
        None,
    );
    let mut reader = SharedMemoryTool::read(bank(dir.path()));
    reader.bind_job_session_scope(JobSessionScope::fixed(session.header.id.clone()));
    let output = run(reader.execute("r", json!({"key":"handoff"}), None)).unwrap();
    assert_eq!(
        output.details.unwrap()["value"]["content"],
        "inspect src/agent.rs next"
    );
    let fork = Session::in_memory();
    reader.bind_job_session_scope(JobSessionScope::fixed(fork.header.id));
    assert!(
        run(reader.execute("r2", json!({"key":"handoff"}), None))
            .unwrap_err()
            .to_string()
            .contains("PI_SHARED_MEMORY_NOT_FOUND")
    );
}

#[test]
fn registry_installs_all_aliases_and_rebinds_older_snapshots_without_manual_tool_binding() {
    let dir = tempfile::tempdir().unwrap();
    let project = bank(dir.path());
    let mut registry = ToolRegistry::from_tools(Vec::new());
    registry.bind_job_session_resolver(Arc::new(|| {
        Box::pin(async { Some("registry-a".to_string()) })
    }));
    registry.enable_shared_memory(Arc::clone(&project)).unwrap();
    registry.push(Box::new(RetainTool::new(Arc::clone(&project))));
    registry.push(Box::new(RecallTool::new(Arc::clone(&project))));
    let snapshot = registry.clone_shallow();
    let before = registry.tools().len();
    let error = registry
        .enable_shared_memory(Arc::clone(&project))
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("PI_SHARED_MEMORY_TOOL_COLLISION")
    );
    assert_eq!(
        registry.tools().len(),
        before,
        "collision cannot partially register tools"
    );
    for name in ["read_memory", "write_memory", "list_memory"] {
        assert_eq!(
            registry
                .tools()
                .iter()
                .filter(|tool| tool.name() == name)
                .count(),
            1
        );
    }
    run(async {
        registry
            .get("write_memory")
            .unwrap()
            .execute(
                "a1",
                json!({"key":"handoff","content":"A","expectedRevision":"absent"}),
                None,
            )
            .await
            .unwrap();
        let read = snapshot
            .get("recall")
            .unwrap()
            .execute("a2", json!({"scope":"session","key":"handoff"}), None)
            .await
            .unwrap();
        assert_eq!(read.details.unwrap()["value"]["content"], "A");
        registry.bind_job_session_resolver(Arc::new(|| {
            Box::pin(async { Some("registry-b".to_string()) })
        }));
        let missing = snapshot
            .get("read_memory")
            .unwrap()
            .execute("b1", json!({"key":"handoff"}), None)
            .await
            .unwrap_err();
        assert!(missing.to_string().contains("PI_SHARED_MEMORY_NOT_FOUND"));
        snapshot
            .get("retain")
            .unwrap()
            .execute(
                "b2",
                json!({"scope":"session","key":"handoff","content":"B"}),
                None,
            )
            .await
            .unwrap();
        let page = registry
            .get("list_memory")
            .unwrap()
            .execute("b3", json!({}), None)
            .await
            .unwrap();
        assert_eq!(
            page.details.unwrap()["entries"].as_array().unwrap().len(),
            1
        );
        registry.bind_job_session_resolver(Arc::new(|| {
            Box::pin(async { Some("registry-a".to_string()) })
        }));
        let read = snapshot
            .get("read_memory")
            .unwrap()
            .execute("a3", json!({"key":"handoff"}), None)
            .await
            .unwrap();
        assert_eq!(read.details.unwrap()["value"]["content"], "A");
    });
    assert!(project.list(10).unwrap().is_empty());
}

#[test]
fn explicit_null_arguments_never_weaken_write_preconditions_or_turn_reads_into_lists() {
    let dir = tempfile::tempdir().unwrap();
    let project = bank(dir.path());
    let shared = SharedMemoryStore::new(Arc::clone(&project), "session").unwrap();
    let original = shared.write("plan", "kept", None).unwrap();
    let mut writer = SharedMemoryTool::write(Arc::clone(&project));
    let mut reader = SharedMemoryTool::read(Arc::clone(&project));
    let mut list = SharedMemoryTool::list(project);
    for tool in [&mut writer as &mut dyn Tool, &mut reader, &mut list] {
        tool.bind_job_session_scope(JobSessionScope::fixed("session"));
    }
    run(async {
        let error = writer
            .execute(
                "w",
                json!({"key":"plan","content":"must not overwrite","expectedRevision":null}),
                None,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("PI_SHARED_MEMORY_INVALID_INPUT"));
        assert!(!error.to_string().contains("must not overwrite"));
        assert!(
            reader
                .execute("r", json!({"key":null}), None)
                .await
                .is_err()
        );
        for input in [
            json!({"prefix":null}),
            json!({"after":null}),
            json!({"limit":null}),
        ] {
            assert!(list.execute("l", input, None).await.is_err());
        }
    });
    let current = shared.read("plan").unwrap().unwrap();
    assert_eq!(current.version.revision, original.revision);
    assert_eq!(current.content, "kept");
}

#[test]
fn host_cleanup_requires_current_revision_and_cannot_remove_another_sessions_key() {
    let dir = tempfile::tempdir().unwrap();
    let project = bank(dir.path());
    let a = SharedMemoryStore::new(Arc::clone(&project), "session-a").unwrap();
    let b = SharedMemoryStore::new(project, "session-b").unwrap();
    let first = a.write("plan", "old", Some("absent")).unwrap();
    let current = a.write("plan", "new", Some(&first.revision)).unwrap();
    assert!(
        a.remove("plan", &first.revision)
            .unwrap_err()
            .to_string()
            .contains("PI_SHARED_MEMORY_CONFLICT")
    );
    assert!(!b.remove("plan", &current.revision).unwrap());
    assert_eq!(a.read("plan").unwrap().unwrap().content, "new");
    assert!(a.remove("plan", &current.revision).unwrap());
    assert!(a.read("plan").unwrap().is_none());
    assert!(a.list("", None, 10).unwrap().entries.is_empty());
    assert!(!a.remove("plan", &current.revision).unwrap());
    let replacement = a.write("plan", "replacement", Some("absent")).unwrap();
    assert_ne!(replacement.revision, current.revision);
    assert!(a.remove("plan", &current.revision).is_err());
    assert_eq!(a.read("plan").unwrap().unwrap().content, "replacement");
    assert!(a.remove("plan", "absent").is_err());
}
