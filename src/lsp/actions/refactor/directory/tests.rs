//! Exercise real tool dispatch, child framing, refactor caching and filesystem
//! mutation. The Python peer is deterministic protocol coverage, not evidence
//! of interoperability with rust-analyzer or another production server.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use crate::config::{Config, LspServerSettings, LspSettings};
use crate::error::Result;
use crate::lsp::LspTool;
use crate::tools::{Tool as _, ToolOutput};

struct Fixture {
    tool: LspTool,
    runtime: asupersync::runtime::Runtime,
    root: PathBuf,
    _temp: tempfile::TempDir,
}

impl Fixture {
    fn new(mode: &str) -> Option<Self> {
        let python = ["python3", "python"].into_iter().find(|program| {
            Command::new(program).arg("--version").stdout(Stdio::null())
                .stderr(Stdio::null()).status().is_ok_and(|status| status.success())
        });
        let Some(python) = python else {
            assert!(std::env::var_os("PI_LSP_REQUIRE_PROTOCOL").is_none(),
                "Python required for directory LSP protocol tests");
            eprintln!("SKIP directory LSP protocol fixture: Python unavailable");
            return None;
        };
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        std::fs::create_dir_all(root.join("old/sub")).unwrap();
        std::fs::create_dir(root.join("old/empty")).unwrap();
        let root = root.canonicalize().unwrap();
        for path in ["old/lib.refactor", "old/sub/use.refactor", "imports.refactor"] {
            std::fs::write(root.join(path), "old\n").unwrap();
        }
        std::fs::write(root.join("old/asset.bin"), [0, 255, 128]).unwrap();
        std::fs::write(root.parent().unwrap().join("outside.refactor"), "old\n").unwrap();
        std::fs::write(root.join(".refactor-root"), "").unwrap();
        let script = root.join("directory_peer.py");
        std::fs::write(&script, include_str!("test_server.py")).unwrap();
        let config = Config {
            lsp: Some(LspSettings {
                servers: Some(HashMap::from([("directory-fixture".to_string(), LspServerSettings {
                    command: Some(python.to_string()),
                    args: Some(vec!["-I".to_string(), "-u".to_string(), script.display().to_string(), mode.to_string()]),
                    extensions: Some(vec![".refactor".to_string()]),
                    languages: Some(vec!["plaintext".to_string()]),
                    root_markers: Some(vec![".refactor-root".to_string()]),
                    ..Default::default()
                })])),
                ..Default::default()
            }),
            ..Default::default()
        };
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
        eprintln!("{}", json!({"case":mode,"event":"directory_protocol_start","movedFiles":3}));
        Some(Self { tool: LspTool::new(&root, Some(&config)), runtime, root, _temp: temp })
    }

    fn run(&self, args: Value) -> Result<ToolOutput> {
        self.runtime.block_on(self.tool.execute("directory-case", args, None))
    }

    fn rename(&self, preview: bool) -> Result<ToolOutput> {
        self.run(json!({"action":"rename_file","file":"old","newFile":"nested/new",
            "apply":!preview,"timeout":5}))
    }

    fn flush(&self) {
        self.run(json!({"action":"request","file":"imports.refactor",
            "method":"fixture/barrier","timeout":5})).unwrap();
    }

    fn requests(&self, method: &str) -> Vec<Value> {
        std::fs::read_to_string(self.root.join("requests.jsonl")).unwrap_or_default()
            .lines().map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|message| message["method"] == method).collect()
    }

    fn unchanged(&self) {
        assert_eq!(std::fs::read(self.root.join("old/lib.refactor")).unwrap(), b"old\n");
        assert_eq!(std::fs::read(self.root.join("imports.refactor")).unwrap(), b"old\n");
        assert!(!self.root.join("nested/new").exists());
    }
}

#[test]
fn public_directory_move_updates_imports_and_notifies_after_commit() {
    let Some(fixture) = Fixture::new("normal") else { return };
    let result = fixture.rename(false).unwrap();
    assert!(!result.is_error);
    let details = result.details.unwrap();
    assert_eq!(details["directory"], true);
    assert_eq!(details["filesMoved"], 3);
    assert_eq!(details["willRenamePairs"], 2);
    assert_eq!(details["didRenamePairs"], 2);
    fixture.flush();
    let notification: Value = serde_json::from_slice(&std::fs::read(fixture.root.join("notification.json")).unwrap()).unwrap();
    assert_eq!(notification["oldExists"], false);
    assert_eq!(notification["newExists"], true);
    assert_eq!(notification["newText"], "new\n");
    assert_eq!(notification["imports"], "new\n");
    assert_eq!(notification["binary"], json!([0, 255, 128]));
    assert_eq!(notification["emptyDirectory"], true);
    let requests = fixture.requests("workspace/willRenameFiles");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["params"]["files"].as_array().unwrap().len(), 2);
    assert!(fixture.requests("workspace/executeCommand").is_empty());
}

#[test]
fn reviewed_directory_plan_is_exact_and_one_shot() {
    let Some(fixture) = Fixture::new("normal") else { return };
    let preview = fixture.rename(true).unwrap().details.unwrap();
    fixture.unchanged();
    assert!(!fixture.root.join("nested").exists());
    assert_eq!(preview["preview"], true);
    assert!(preview["files"].as_array().unwrap().iter().any(|file| file["kind"] == "directoryRename"));
    let args = json!({"action":"rename_file","refactorId":preview["refactorId"],"apply":true});
    let applied = fixture.run(args.clone()).unwrap().details.unwrap();
    assert_eq!(applied["applied"], true);
    fixture.flush();
    assert_eq!(fixture.requests("workspace/willRenameFiles").len(), 1);
    assert_eq!(fixture.requests("workspace/didRenameFiles").len(), 1);
    assert!(fixture.run(args).is_err());
}

#[test]
fn preview_drift_refuses_approval_without_a_move_or_notification() {
    let Some(fixture) = Fixture::new("normal") else { return };
    let preview = fixture.rename(true).unwrap().details.unwrap();
    std::fs::write(fixture.root.join("old/added"), "keep").unwrap();
    let error = fixture.run(json!({"action":"rename_file","refactorId":preview["refactorId"],"apply":true})).unwrap_err();
    assert!(error.to_string().contains("tree changed"), "{error}");
    fixture.unchanged();
    fixture.flush();
    assert!(fixture.requests("workspace/didRenameFiles").is_empty());
}

#[test]
fn server_errors_invalid_edits_and_workspace_escape_do_not_move_or_edit() {
    for mode in ["error", "invalid", "escape"] {
        let Some(fixture) = Fixture::new(mode) else { return };
        assert!(fixture.rename(false).is_err(), "{mode}");
        fixture.unchanged();
        assert_eq!(std::fs::read(fixture.root.parent().unwrap().join("outside.refactor")).unwrap(), b"old\n");
        fixture.flush();
        assert!(fixture.requests("workspace/didRenameFiles").is_empty());
    }
}

#[test]
fn folder_and_file_registrations_are_independent_and_unregistered_moves_are_explicit() {
    let Some(fixture) = Fixture::new("folder") else { return };
    let details = fixture.rename(false).unwrap().details.unwrap();
    assert_eq!(details["willRenamePairs"], 1);
    assert_eq!(details["didRenamePairs"], 2);
    fixture.flush();
    let requests = fixture.requests("workspace/willRenameFiles");
    let old_uri = requests[0]["params"]["files"][0]["oldUri"].as_str().unwrap();
    assert!(old_uri.ends_with("/old"));
    let Some(unregistered) = Fixture::new("unregistered") else { return };
    let details = unregistered.rename(false).unwrap().details.unwrap();
    assert_eq!(details["willRenameFiles"], false);
    assert_eq!(details["notificationRequested"], false);
    assert_eq!(std::fs::read(unregistered.root.join("nested/new/lib.refactor")).unwrap(), b"old\n");
    assert!(unregistered.requests("workspace/willRenameFiles").is_empty());
}

#[test]
fn mixed_servers_and_nested_roots_are_rejected_before_startup() {
    for nested_root in [false, true] {
        let Some(fixture) = Fixture::new("normal") else { return };
        if nested_root {
            std::fs::write(fixture.root.join("old/sub/.refactor-root"), "").unwrap();
        } else {
            std::fs::write(fixture.root.join("old/other.rs"), "fn main() {}\n").unwrap();
        }
        assert!(fixture.rename(false).is_err());
        fixture.unchanged();
        assert!(!fixture.root.join("requests.jsonl").exists());
    }
}

#[test]
fn unedited_file_drift_while_preparing_imports_cannot_be_blessed_by_restaging() {
    let Some(fixture) = Fixture::new("drift") else { return };
    assert!(fixture.rename(false).is_err());
    fixture.unchanged();
    assert_eq!(std::fs::read(fixture.root.join("old/asset.bin")).unwrap(), b"external");
    fixture.flush();
    assert!(fixture.requests("workspace/didRenameFiles").is_empty());
}

#[test]
fn request_budget_expires_without_a_move() {
    let Some(fixture) = Fixture::new("timeout") else { return };
    let started = std::time::Instant::now();
    let result = fixture.run(json!({"action":"rename_file","file":"old","newFile":"nested/new","timeout":1}));
    assert!(result.is_err());
    assert!(started.elapsed() < std::time::Duration::from_secs(4));
    fixture.unchanged();
}
