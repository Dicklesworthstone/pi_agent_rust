//! Integration tests for the agent hub (bd-cv653.5.3): the session-scoped
//! registry of subagent children with transcript persistence, steering
//! delivery, operator kill, revive lineage, and the peer bus.
//!
//! Hermetic: no live provider. The child process exercised here is a real
//! `sleep` so the kill path (process-tree signal + registry settle) runs
//! end-to-end; steering delivery is proven by draining the queue file exactly
//! as the child's print-mode fetcher does.
#![allow(clippy::missing_panics_doc)]

use std::process::{Command, Stdio};

use pi::agent_hub::{self, ChildStatus};

/// Serialize registry-mutating tests: the registry is process-global.
static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn test_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("pi-agent-hub-it-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

#[test]
fn roster_tracks_lifecycle_and_transcript() {
    let _guard = LOCK.lock().expect("lock");
    let dir = test_dir("lifecycle");
    let mut reg = agent_hub::AgentHubRegistry::default();
    reg.set_dir_for_tests(dir.clone());

    let entry = reg.register("worker", "build the thing").expect("register");
    assert_eq!(entry.status, ChildStatus::Starting);
    reg.mark_running(&entry.id, std::process::id());
    reg.append_transcript(&entry.id, "{\"type\":\"message_update\"}");
    reg.append_transcript(&entry.id, "{\"type\":\"message_end\"}");

    let page = reg.transcript_page(&entry.id).expect("page");
    assert!(page.contains("message_update"));
    assert!(page.contains("message_end"));

    let roster = reg.roster();
    assert!(roster.iter().any(|e| e.id == entry.id));
    reg.settle(&entry.id, ChildStatus::Done);
    assert_eq!(reg.get(&entry.id).expect("get").status, ChildStatus::Done);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn kill_terminates_real_process_tree() {
    let _guard = LOCK.lock().expect("lock");
    let dir = test_dir("kill");
    let mut reg = agent_hub::AgentHubRegistry::default();
    reg.set_dir_for_tests(dir.clone());

    // A real wedged child stand-in: sleep for an hour.
    let mut child = Command::new("sleep")
        .arg("3600")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sleep");
    let pid = child.id();

    let entry = reg
        .register("wedged", "infinite loop fixture")
        .expect("register");
    reg.mark_running(&entry.id, pid);

    // The operator kill path: tree signal + registry settle (mirrors
    // HubTool::dispatch_agent "kill").
    pi::tools::kill_process_group_tree(Some(pid));
    reg.mark_killed(&entry.id);
    // Reap the killed child so the probe below sees no zombie.
    let _ = child.wait();

    let settled = reg.get(&entry.id).expect("get");
    assert_eq!(settled.status, ChildStatus::Killed);
    assert!(settled.finished_ms.is_some());

    // The process is gone: signaling it again must fail.
    let probe = Command::new("kill").arg("-0").arg(pid.to_string()).output();
    assert!(
        probe.map_or(true, |o| !o.status.success()),
        "wedged child survived the kill path"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn steer_round_trips_through_queue_file() {
    let _guard = LOCK.lock().expect("lock");
    let dir = test_dir("steer");
    let mut reg = agent_hub::AgentHubRegistry::default();
    reg.set_dir_for_tests(dir.clone());

    let entry = reg.register("scout", "inspect").expect("register");
    reg.mark_running(&entry.id, 1234);
    reg.steer(&entry.id, "parent", "focus on src/auth.rs")
        .expect("steer");
    reg.bus_send(&entry.id, "worker-2", "I found the bug")
        .expect("bus");

    // The child's print-mode fetcher drains exactly this way.
    let drained = agent_hub::drain_steer_file(&entry.steer_path);
    assert_eq!(drained.len(), 2);
    assert!(drained[0].contains("focus on src/auth.rs"));
    assert!(drained[1].contains("I found the bug"));
    // Consumed exactly once.
    assert!(agent_hub::drain_steer_file(&entry.steer_path).is_empty());
    // Inbox ordering matches delivery order.
    let inbox = reg.inbox(&entry.id);
    assert_eq!(inbox.len(), 2);
    assert!(inbox[0].seq < inbox[1].seq);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn revive_registers_lineage_with_transcript_context() {
    let _guard = LOCK.lock().expect("lock");
    let dir = test_dir("revive");
    let mut reg = agent_hub::AgentHubRegistry::default();
    reg.set_dir_for_tests(dir.clone());

    let entry = reg
        .register("worker", "original task body")
        .expect("register");
    reg.mark_running(&entry.id, 55);
    reg.append_transcript(
        &entry.id,
        "{\"type\":\"message_end\",\"text\":\"partial progress\"}",
    );
    reg.settle(&entry.id, ChildStatus::Failed);

    let (revived, task) = reg.revive(&entry.id).expect("revive");
    assert_eq!(revived.revived_from.as_deref(), Some(entry.id.as_str()));
    assert_eq!(revived.status, ChildStatus::Starting);
    assert!(task.contains("original task body"));
    assert!(task.contains("partial progress"));
    let _ = std::fs::remove_dir_all(&dir);
}
