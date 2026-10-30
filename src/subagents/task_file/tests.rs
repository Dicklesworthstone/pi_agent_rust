use super::*;
use serde_json::json;
use std::time::Duration;

fn fixture() -> (tempfile::TempDir, SubagentTool) {
    let root = tempfile::tempdir().expect("task file workspace");
    let tool = SubagentTool::with_paths(
        root.path().to_path_buf(),
        root.path().join("global"),
        root.path().join("never-launched"),
    );
    (root, tool)
}

fn deadline() -> Deadline {
    Deadline::for_request(Some(Duration::from_secs(30)), None).expect("deadline")
}

fn task(source: &str) -> SubagentTask {
    serde_json::from_value(json!({"agent":"task", "taskFile":source})).expect("task")
}

#[test]
fn task_sources_are_explicit_and_cannot_forge_snapshot_metadata() {
    let file = json!({"agent":"task", "taskFile":"local://instructions"});
    for valid in [
        file.clone(),
        json!({"tasks":[file.clone(), {"agent":"task", "task":"inline"}]}),
        json!({"chain":[file.clone()]}),
    ] {
        validate_request(&valid).expect("supported request");
    }
    for invalid_input in [
        json!({"agent":"task"}),
        json!({"agent":"task", "task":null}),
        json!({"agent":"task", "taskFile":null}),
        json!({"agent":"task", "taskFile":true}),
        json!({"agent":"task", "taskFile":""}),
        json!({"agent":"task", "taskFile":"relative.md"}),
        json!({"agent":"task", "taskFile":"https://example.com/instructions"}),
        json!({"agent":"task", "taskFile":"local://"}),
        json!({"agent":"task", "taskFile":"local://bad\nname"}),
        json!({"agent":"task", "task":"", "taskFile":"local://instructions"}),
        json!({"agent":"task", "task":null, "taskFile":"local://instructions"}),
        json!({"agent":"task", "taskFile":"local://instructions", "taskFileSha256":"forged"}),
        json!({"agent":"task", "task":"inline", "taskFileSha256":"forged"}),
    ] {
        for input in [
            invalid_input.clone(),
            json!({"tasks":[file.clone(), invalid_input.clone()]}),
            json!({"chain":[file.clone(), invalid_input.clone()]}),
        ] {
            assert!(validate_request(&input).is_err(), "{input}");
        }
    }
    assert!(validate_request(&json!({"tasks":[file], "taskFile":null})).is_err());
    let injected: SubagentTask = serde_json::from_value(json!({
        "agent":"task", "taskFile":"local://instructions", "taskFileSha256":"forged"
    }))
    .expect("private snapshot field is skipped");
    assert!(injected.task_file_sha256.is_none());
}

#[test]
fn snapshot_preserves_original_utf8_bytes_and_is_not_reread() {
    let (root, tool) = fixture();
    let path = root.path().join("instructions.md");
    let original = "\u{feff}Read the résumé.\r\nKeep the trailing spaces.  \n";
    std::fs::write(&path, original).expect("source");
    let mut task = task(path.to_str().expect("path"));
    prepare(&tool, std::iter::once(&mut task), deadline()).expect("snapshot");
    assert_eq!(task.task, original);
    let digest = format!("{:x}", Sha256::digest(original.as_bytes()));
    assert_eq!(task.task_file_sha256.as_deref(), Some(digest.as_str()));
    std::fs::write(&path, "changed after admission").expect("replace source");
    prepare(&tool, std::iter::once(&mut task), deadline()).expect("retain snapshot");
    assert_eq!(task.task, original);
    assert_eq!(task.display_task(), path.to_str().expect("path"));
}

#[test]
fn invalid_and_oversized_sources_are_refused_without_truncating() {
    let (root, tool) = fixture();
    let path = root.path().join("instructions.md");
    for bytes in [
        Vec::new(),
        b" \r\n\t".to_vec(),
        vec![0xff, 0xfe],
        b"do\0not".to_vec(),
        vec![b'a'; MAX_BYTES + 1],
    ] {
        std::fs::write(&path, bytes).expect("source");
        let mut task = task(path.to_str().expect("path"));
        assert!(prepare(&tool, std::iter::once(&mut task), deadline()).is_err());
        assert!(task.task.is_empty());
        assert!(task.task_file_sha256.is_none());
    }
    std::fs::write(&path, vec![b'a'; MAX_BYTES]).expect("boundary source");
    let mut task = task(path.to_str().expect("path"));
    prepare(&tool, std::iter::once(&mut task), deadline()).expect("exact limit");
    assert_eq!(task.task.len(), MAX_BYTES);
    let mut directory = self::task(root.path().to_str().expect("directory"));
    assert!(prepare(&tool, std::iter::once(&mut directory), deadline()).is_err());
}

#[test]
fn local_scratch_references_use_the_existing_store_and_byte_limit() {
    let (_root, tool) = fixture();
    let name = format!("task-file-{}", uuid::Uuid::new_v4());
    let source = format!("local://{name}");
    let mut missing = task(&source);
    assert!(prepare(&tool, std::iter::once(&mut missing), deadline()).is_err());
    crate::url_router::write_local(&name, "Inspect the workspace.").expect("scratch");
    let mut snapshot = task(&source);
    prepare(&tool, std::iter::once(&mut snapshot), deadline()).expect("scratch snapshot");
    crate::url_router::write_local(&name, "Changed.").expect("replace scratch");
    assert_eq!(snapshot.task, "Inspect the workspace.");
    assert_eq!(snapshot.task_file.as_deref(), Some(source.as_str()));
    crate::url_router::write_local(&name, &"x".repeat(MAX_BYTES + 1)).expect("large scratch");
    let mut oversized = task(&source);
    assert!(prepare(&tool, std::iter::once(&mut oversized), deadline()).is_err());
}

#[test]
fn live_read_roots_admit_snapshot_and_can_revoke_its_later_launch() {
    let (root, mut tool) = fixture();
    let outside = tempfile::tempdir().expect("outside workspace");
    let path = outside.path().join("instructions.md");
    std::fs::write(&path, "Inspect the assigned work.").expect("source");
    let mut task = task(path.to_str().expect("path"));
    assert!(prepare(&tool, std::iter::once(&mut task), deadline()).is_err());
    let mut workspace = crate::workspace::WorkspaceHandle::single(root.path());
    workspace.add_root(outside.path());
    tool.workspace = workspace.clone();
    prepare(&tool, std::iter::once(&mut task), deadline()).expect("added-root source");
    let launch = admission(&tool, std::iter::once(&task)).expect("file launch check");
    launch().expect("still admitted");
    assert!(workspace.remove_root(outside.path()));
    assert!(launch().is_err());
    assert_eq!(task.task, "Inspect the assigned work.");
}

#[cfg(unix)]
#[test]
fn symlink_escapes_unreadable_files_and_fifos_are_rejected() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    let (root, tool) = fixture();
    let outside = tempfile::tempdir().expect("outside workspace");
    let path = outside.path().join("private.md");
    std::fs::write(&path, "outside").expect("outside source");
    let link = root.path().join("escape.md");
    symlink(&path, &link).expect("escape symlink");
    let mut escaped = task(link.to_str().expect("path"));
    assert!(prepare(&tool, std::iter::once(&mut escaped), deadline()).is_err());

    let unreadable = root.path().join("unreadable.md");
    std::fs::write(&unreadable, "private").expect("unreadable source");
    std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0)).expect("mode");
    let mut denied = task(unreadable.to_str().expect("path"));
    assert!(prepare(&tool, std::iter::once(&mut denied), deadline()).is_err());
    std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o600)).expect("mode");

    let fifo = root.path().join("fifo.md");
    rustix::fs::mkfifo(&fifo, rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR).expect("fifo");
    let mut special = task(fifo.to_str().expect("path"));
    assert!(prepare(&tool, std::iter::once(&mut special), deadline()).is_err());
}
