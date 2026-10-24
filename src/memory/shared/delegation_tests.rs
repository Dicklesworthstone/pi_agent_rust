//! Grant/registry tests use the real bank; subprocess fixtures use the same API
//! an SDK child host calls at startup. They do not stand in for CLI integration.

use super::*;
use serde_json::json;
use std::ffi::OsString;
use std::sync::Mutex;
use std::time::Instant;

fn run<F: std::future::Future>(future: F) -> F::Output {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap()
        .block_on(future)
}

fn bank(root: &Path) -> Arc<MemoryStore> {
    Arc::new(MemoryStore {
        db_path: root.join("bank.sqlite"),
        project_key: "grant-fixture".to_string(),
        project_root: root.canonicalize().unwrap(),
        secret_patterns: Vec::new(),
    })
}

fn grant(root: &Path) -> SharedMemoryGrant {
    run(
        SharedMemoryBinding::new(bank(root), JobSessionScope::fixed("parent"))
            .resolve(Duration::from_secs(5)),
    )
    .unwrap()
}

fn command(grant: &SharedMemoryGrant, cwd: &Path, id: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.current_dir(cwd);
    grant.configure_command(&mut command, cwd, id).unwrap();
    command
}

fn env(command: &Command, name: &str) -> OsString {
    command
        .get_envs()
        .find(|(key, _)| *key == name)
        .and_then(|(_, value)| value)
        .unwrap()
        .to_os_string()
}

fn decode(command: &Command, cwd: &Path) -> SharedMemoryGrant {
    SharedMemoryGrant::decode(
        &env(command, GRANT_ENV),
        cwd,
        Some(&env(command, PARENT_ENV)),
        Some(&env(command, RUN_ENV)),
    )
    .unwrap()
}

#[test]
fn one_resolution_freezes_the_live_owner_across_later_switches() {
    let dir = tempfile::tempdir().unwrap();
    let scope = JobSessionScope::fixed("initial");
    let owner = Arc::new(Mutex::new("first".to_string()));
    let current = Arc::clone(&owner);
    scope.bind(Arc::new(move || {
        let value = current.lock().unwrap().clone();
        Box::pin(async move { Some(value) })
    }));
    let binding = SharedMemoryBinding::new(bank(dir.path()), scope);
    let first = run(binding.resolve(Duration::from_secs(5))).unwrap();
    *owner.lock().unwrap() = "second".to_string();
    let second = run(binding.resolve(Duration::from_secs(5))).unwrap();
    first
        .store
        .write("key", "old namespace", Some("absent"))
        .unwrap();
    assert!(second.store.read("key").unwrap().is_none());
    let child = decode(&command(&first, dir.path(), "queued-child"), dir.path());
    assert_eq!(
        child.store.read("key").unwrap().unwrap().content,
        "old namespace"
    );
}

#[test]
fn child_job_rebinding_cannot_retarget_installed_shared_tools() {
    let dir = tempfile::tempdir().unwrap();
    let parent = grant(dir.path());
    parent
        .store
        .write("handoff", "parent content", Some("absent"))
        .unwrap();
    let child = decode(&command(&parent, dir.path(), "child"), dir.path());
    let mut registry = ToolRegistry::from_tools(Vec::new());
    child.install_tools(&mut registry, &TOOL_NAMES).unwrap();
    registry.bind_job_session_resolver(Arc::new(|| {
        Box::pin(async { Some("child-job".to_string()) })
    }));
    let output =
        run(registry
            .get("read_memory")
            .unwrap()
            .execute("read", json!({"key":"handoff"}), None))
        .unwrap();
    assert_eq!(
        output.details.unwrap()["value"]["content"],
        "parent content"
    );
    run(registry.get("write_memory").unwrap().execute(
        "write",
        json!({
            "key":"reply", "content":"exact\nchild reply\n", "expectedRevision":"absent"
        }),
        None,
    ))
    .unwrap();
    assert_eq!(
        parent.store.read("reply").unwrap().unwrap().content,
        "exact\nchild reply\n"
    );
    assert!(
        SharedMemoryStore::new(bank(dir.path()), "child-job")
            .unwrap()
            .read("reply")
            .unwrap()
            .is_none()
    );
}

#[test]
fn worktree_and_nested_grants_keep_the_original_bank() {
    let dir = tempfile::tempdir().unwrap();
    let worktree = tempfile::tempdir().unwrap();
    let parent = grant(dir.path());
    parent.check_source_directory(dir.path()).unwrap();
    assert!(parent.check_source_directory(worktree.path()).is_err());
    parent
        .store
        .write("handoff", "origin", Some("absent"))
        .unwrap();
    let child = decode(
        &command(&parent, worktree.path(), "worktree-child"),
        worktree.path(),
    );
    child.check_source_directory(worktree.path()).unwrap();
    assert_eq!(child.store.bank.db_path, parent.store.bank.db_path);
    let nested = run(child.binding().resolve(Duration::from_secs(5))).unwrap();
    let nested = decode(
        &command(&nested, worktree.path(), "grandchild"),
        worktree.path(),
    );
    assert_eq!(
        nested.store.read("handoff").unwrap().unwrap().content,
        "origin"
    );
    assert_eq!(nested.store.bank.db_path, parent.store.bank.db_path);
    assert!(!worktree.path().join("bank.sqlite").exists());
}

#[test]
fn nested_read_only_grants_reject_writes_even_if_the_writer_is_selected() {
    let dir = tempfile::tempdir().unwrap();
    let binding =
        SharedMemoryBinding::new(bank(dir.path()), JobSessionScope::fixed("parent")).read_only();
    let parent = run(binding.resolve(Duration::from_secs(5))).unwrap();
    parent
        .store
        .write("key", "preserved", Some("absent"))
        .unwrap();
    let selected = parent
        .for_tool_selection(Some(&["write_memory".to_string()]))
        .unwrap();
    let child = decode(&command(&selected, dir.path(), "readonly"), dir.path());
    let nested = run(child.binding().resolve(Duration::from_secs(5))).unwrap();
    let mut registry = ToolRegistry::from_tools(Vec::new());
    nested
        .install_tools(&mut registry, &["write_memory"])
        .unwrap();
    let error = run(registry.get("write_memory").unwrap().execute(
        "write",
        json!({
            "key":"key", "content":"must not persist"
        }),
        None,
    ))
    .unwrap_err();
    assert!(error.to_string().contains("PI_SHARED_MEMORY_READ_ONLY"));
    assert!(!error.to_string().contains("must not persist"));
    assert_eq!(
        parent.store.read("key").unwrap().unwrap().content,
        "preserved"
    );
}

#[test]
fn explicit_tool_pins_do_not_expand_the_delegation() {
    let dir = tempfile::tempdir().unwrap();
    let parent = grant(dir.path());
    assert!(parent.for_tool_selection(Some(&[])).is_none());
    assert!(
        parent
            .for_tool_selection(Some(&["read".to_string()]))
            .is_none()
    );
    let selected = parent
        .for_tool_selection(Some(&["read_memory".to_string()]))
        .unwrap();
    assert!(selected.access == Access::ReadOnly);
    let mut registry = ToolRegistry::from_tools(Vec::new());
    selected
        .install_tools(&mut registry, &["read_memory", "unrelated", "read_memory"])
        .unwrap();
    assert_eq!(registry.tools().len(), 1);
    assert!(registry.get("write_memory").is_none());
    assert!(registry.get("list_memory").is_none());
    assert!(parent.for_tool_selection(None).unwrap().access == Access::ReadWrite);
}

#[test]
fn name_collision_leaves_the_registry_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let parent = grant(dir.path());
    let mut registry =
        ToolRegistry::from_tools(vec![Box::new(SharedMemoryTool::read(bank(dir.path())))]);
    let before = registry.tools().len();
    let error = parent
        .install_tools(&mut registry, &TOOL_NAMES)
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("PI_SHARED_MEMORY_TOOL_COLLISION")
    );
    assert_eq!(registry.tools().len(), before);
    assert!(registry.get("write_memory").is_none());
    assert!(registry.get("list_memory").is_none());
}

#[test]
fn a_nested_host_cannot_widen_the_exact_role_tool_selection() {
    let dir = tempfile::tempdir().unwrap();
    let parent = grant(dir.path());
    let selected = parent
        .for_tool_selection(Some(&["read_memory".to_string()]))
        .unwrap();
    let child = decode(&command(&selected, dir.path(), "read-role"), dir.path());
    let nested = run(child.binding().resolve(Duration::from_secs(5))).unwrap();
    assert!(
        nested
            .for_tool_selection(Some(&["list_memory".to_string()]))
            .is_none()
    );
    let mut registry = ToolRegistry::from_tools(Vec::new());
    let error = nested
        .install_tools(&mut registry, &TOOL_NAMES)
        .unwrap_err();
    assert!(error.to_string().contains("PI_SHARED_MEMORY_TOOL_SCOPE"));
    assert!(
        registry.tools().is_empty(),
        "a rejected wider pin must not partially install"
    );
    nested
        .install_tools(&mut registry, &["read_memory"])
        .unwrap();
    assert_eq!(registry.tools().len(), 1);
    let mut mixed =
        ToolRegistry::from_tools(vec![Box::new(SharedMemoryTool::write(bank(dir.path())))]);
    let error = nested
        .install_tools(&mut mixed, &["read_memory"])
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("PI_SHARED_MEMORY_TOOL_COLLISION")
    );
    assert!(
        mixed.get("read_memory").is_none(),
        "never mix two alias namespaces"
    );
}

#[test]
fn malformed_and_mismatched_grants_never_select_a_fallback_namespace() {
    let dir = tempfile::tempdir().unwrap();
    let parent = grant(dir.path());
    let cmd = command(&parent, dir.path(), "child");
    let raw = env(&cmd, GRANT_ENV);
    let parent_id = env(&cmd, PARENT_ENV);
    let run_id = env(&cmd, RUN_ENV);
    let value: Value = serde_json::from_str(raw.to_str().unwrap()).unwrap();
    for (field, invalid) in [
        ("version", json!(2)),
        ("sessionId", json!("")),
        ("database", json!("relative.sqlite")),
        ("workingDirectory", json!(".")),
        ("parentPid", json!(0)),
        ("runId", json!("different")),
        ("access", json!("administrator")),
        ("unexpected", json!("secret-value")),
        ("allowedTools", json!(0)),
        ("allowedTools", json!(8)),
    ] {
        let mut candidate = value.clone();
        candidate[field] = invalid;
        let encoded = OsString::from(candidate.to_string());
        let error =
            SharedMemoryGrant::decode(&encoded, dir.path(), Some(&parent_id), Some(&run_id))
                .err()
                .unwrap();
        assert!(
            error.to_string().contains("PI_SHARED_MEMORY_DELEGATION"),
            "{field}"
        );
        assert!(!error.to_string().contains("secret-value"));
    }
    assert!(SharedMemoryGrant::decode(&raw, dir.path(), None, Some(&run_id)).is_err());
    assert!(SharedMemoryGrant::decode(&raw, dir.path(), Some(&parent_id), None).is_err());
    let other = tempfile::tempdir().unwrap();
    assert!(
        SharedMemoryGrant::decode(&raw, other.path(), Some(&parent_id), Some(&run_id)).is_err()
    );
    let oversized = OsString::from("x".repeat(MAX_GRANT_BYTES + 1));
    assert!(
        SharedMemoryGrant::decode(&oversized, dir.path(), Some(&parent_id), Some(&run_id)).is_err()
    );
}

#[test]
fn command_setup_does_not_expose_values_and_clears_failed_or_unshared_grants() {
    let dir = tempfile::tempdir().unwrap();
    let parent = grant(dir.path());
    parent
        .store
        .write("key", "private fixture content", Some("absent"))
        .unwrap();
    let mut cmd = command(&parent, dir.path(), "child");
    assert!(cmd.get_args().next().is_none(), "grant must not enter argv");
    assert_eq!(
        cmd.get_current_dir(),
        Some(dir.path().canonicalize().unwrap().as_path())
    );
    assert!(
        !env(&cmd, GRANT_ENV)
            .to_string_lossy()
            .contains("private fixture content")
    );
    assert!(
        parent
            .configure_command(&mut cmd, dir.path(), "bad/run")
            .is_err()
    );
    assert!(
        cmd.get_envs()
            .any(|(key, value)| key == GRANT_ENV && value.is_none())
    );
    parent
        .configure_command(&mut cmd, dir.path(), "valid")
        .unwrap();
    SharedMemoryGrant::clear_command(&mut cmd);
    assert!(
        cmd.get_envs()
            .any(|(key, value)| key == GRANT_ENV && value.is_none())
    );
}

#[test]
fn resolution_rejects_invalid_budgets_and_bounds_a_stalled_owner() {
    let dir = tempfile::tempdir().unwrap();
    let scope = JobSessionScope::fixed("unused");
    scope.bind(Arc::new(|| Box::pin(futures::future::pending())));
    let binding = SharedMemoryBinding::new(bank(dir.path()), scope);
    for budget in [
        Duration::ZERO,
        Duration::from_nanos(1),
        Duration::from_secs(86_401),
    ] {
        assert!(run(binding.resolve(budget)).is_err());
    }
    let error = run(binding.resolve(Duration::from_millis(10)))
        .err()
        .unwrap();
    assert!(error.to_string().contains("PI_SHARED_MEMORY_TIMEOUT"));
}

#[test]
fn separate_children_share_revisions_without_sharing_other_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let parent = grant(dir.path());
    let original = parent
        .store
        .write("counter", "before", Some("absent"))
        .unwrap();
    let left = decode(&command(&parent, dir.path(), "left"), dir.path());
    let right = decode(&command(&parent, dir.path(), "right"), dir.path());
    left.store
        .write("counter", "after", Some(&original.revision))
        .unwrap();
    let error = right
        .store
        .write("counter", "stale", Some(&original.revision))
        .unwrap_err();
    assert!(error.to_string().contains("PI_SHARED_MEMORY_CONFLICT"));
    assert_eq!(
        parent.store.read("counter").unwrap().unwrap().content,
        "after"
    );
    assert!(
        SharedMemoryStore::new(bank(dir.path()), "unrelated")
            .unwrap()
            .read("counter")
            .unwrap()
            .is_none()
    );
}

struct ChildGuard(Option<std::process::Child>);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
fn sdk_child_bootstrap_reads_and_writes_the_parent_bank_in_another_process() {
    let dir = tempfile::tempdir().unwrap();
    let parent = grant(dir.path());
    parent
        .store
        .write("handoff", "from parent", Some("absent"))
        .unwrap();
    let mut cmd = command(&parent, dir.path(), "subprocess");
    cmd.args([
        "--ignored",
        "--exact",
        "memory::shared::delegation::tests::child_host_fixture",
    ])
    .env("PI_GRANT_FIXTURE", "1");
    let mut child = ChildGuard(Some(cmd.spawn().unwrap()));
    let limit = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.0.as_mut().unwrap().try_wait().unwrap() {
            assert!(status.success(), "SDK child fixture failed");
            child.0.take();
            break;
        }
        assert!(Instant::now() < limit, "SDK child fixture timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        parent.store.read("reply").unwrap().unwrap().content,
        "from child"
    );
}

#[test]
#[ignore = "subprocess helper invoked only by the parent fixture"]
fn child_host_fixture() {
    assert_eq!(std::env::var("PI_GRANT_FIXTURE").unwrap(), "1");
    let cwd = std::env::current_dir().unwrap();
    let grant = SharedMemoryGrant::from_environment(&cwd).unwrap().unwrap();
    let mut registry = ToolRegistry::from_tools(Vec::new());
    grant.install_tools(&mut registry, &TOOL_NAMES).unwrap();
    registry.bind_job_session_resolver(Arc::new(|| {
        Box::pin(async { Some("isolated-child-job".to_string()) })
    }));
    let output =
        run(registry
            .get("read_memory")
            .unwrap()
            .execute("r", json!({"key":"handoff"}), None))
        .unwrap();
    assert_eq!(output.details.unwrap()["value"]["content"], "from parent");
    run(registry.get("write_memory").unwrap().execute(
        "w",
        json!({
            "key":"reply", "content":"from child", "expectedRevision":"absent"
        }),
        None,
    ))
    .unwrap();
}
