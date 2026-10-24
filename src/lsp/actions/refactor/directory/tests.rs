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
        Self::with_second(mode, None)
    }

    fn with_second(mode: &str, secondary: Option<&str>) -> Option<Self> {
        Self::configured(mode, secondary, false)
    }

    fn with_matching_servers(mode: &str, secondary: &str) -> Option<Self> {
        Self::configured(mode, Some(secondary), true)
    }

    fn configured(mode: &str, secondary: Option<&str>, same_source: bool) -> Option<Self> {
        let python = ["python3", "python"].into_iter().find(|program| {
            Command::new(program)
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        });
        let Some(python) = python else {
            assert!(
                std::env::var_os("PI_LSP_REQUIRE_PROTOCOL").is_none(),
                "Python required for directory LSP protocol tests"
            );
            eprintln!("SKIP directory LSP protocol fixture: Python unavailable");
            return None;
        };
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        std::fs::create_dir_all(root.join("old/sub")).unwrap();
        std::fs::create_dir(root.join("old/empty")).unwrap();
        let root = root.canonicalize().unwrap();
        for path in [
            "old/lib.refactor",
            "old/sub/use.refactor",
            "imports.refactor",
        ] {
            std::fs::write(root.join(path), "old\n").unwrap();
        }
        std::fs::write(root.join("old/asset.bin"), [0, 255, 128]).unwrap();
        std::fs::write(root.parent().unwrap().join("outside.refactor"), "old\n").unwrap();
        std::fs::write(root.join(".refactor-root"), "").unwrap();
        let script = root.join("directory_peer.py");
        std::fs::write(&script, include_str!("test_server.py")).unwrap();
        let mut servers = HashMap::from([(
            "directory-fixture".to_string(),
            LspServerSettings {
                command: Some(python.to_string()),
                args: Some(vec![
                    "-I".to_string(),
                    "-u".to_string(),
                    script.display().to_string(),
                    mode.to_string(),
                ]),
                extensions: Some(vec![".refactor".to_string()]),
                languages: Some(vec!["plaintext".to_string()]),
                root_markers: Some(vec![".refactor-root".to_string()]),
                ..Default::default()
            },
        )]);
        if let Some(mode) = secondary {
            for path in ["old/secondary.other", "imports.other"] {
                std::fs::write(root.join(path), "old\n").unwrap();
            }
            std::fs::write(root.join("shared.refactor"), "abcdef\n").unwrap();
            let mut args = vec![
                "-I".to_string(),
                "-u".to_string(),
                script.display().to_string(),
                mode.to_string(),
                "secondary".to_string(),
            ];
            let mut extensions = vec![".other".to_string()];
            if same_source {
                args.push("same-source".to_string());
                extensions.push(".refactor".to_string());
            }
            servers.insert(
                "secondary-fixture".to_string(),
                LspServerSettings {
                    command: Some(python.to_string()),
                    args: Some(args),
                    extensions: Some(extensions),
                    languages: Some(vec!["plaintext".to_string()]),
                    root_markers: Some(vec![".refactor-root".to_string()]),
                    ..Default::default()
                },
            );
        }
        if same_source {
            servers.insert(
                "destination-only".to_string(),
                LspServerSettings {
                    command: Some("pi-lsp-destination-must-not-start".to_string()),
                    extensions: Some(vec![".destination".to_string()]),
                    ..Default::default()
                },
            );
        }
        let config = Config {
            lsp: Some(LspSettings {
                servers: Some(servers),
                ..Default::default()
            }),
            ..Default::default()
        };
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        eprintln!(
            "{}",
            json!({"case":mode,"secondary":secondary,"event":"directory_protocol_start","movedFiles":if secondary.is_some() {4} else {3}})
        );
        Some(Self {
            tool: LspTool::new(&root, Some(&config)),
            runtime,
            root,
            _temp: temp,
        })
    }

    fn run(&self, args: Value) -> Result<ToolOutput> {
        self.runtime
            .block_on(self.tool.execute("directory-case", args, None))
    }

    fn rename(&self, preview: bool) -> Result<ToolOutput> {
        self.run(
            json!({"action":"rename_file","file":"old","newFile":"nested/new",
            "apply":!preview,"timeout":5}),
        )
    }

    fn flush(&self) {
        // A source can now match several servers, so routing a normal request
        // through the first match would not fence every participant's log.
        for status in self.tool.registry.status() {
            let entry = self
                .tool
                .registry
                .entry_for_root(&status.name, &status.root)
                .unwrap();
            self.runtime
                .block_on(entry.client.call(
                    "fixture/barrier",
                    json!({}),
                    std::time::Duration::from_secs(5),
                ))
                .unwrap();
        }
    }

    fn requests(&self, method: &str) -> Vec<Value> {
        self.peer_requests("requests.jsonl", method)
    }

    fn peer_requests(&self, log: &str, method: &str) -> Vec<Value> {
        std::fs::read_to_string(self.root.join(log))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|message| message["method"] == method)
            .collect()
    }

    fn unchanged(&self) {
        assert_eq!(
            std::fs::read(self.root.join("old/lib.refactor")).unwrap(),
            b"old\n"
        );
        assert_eq!(
            std::fs::read(self.root.join("imports.refactor")).unwrap(),
            b"old\n"
        );
        assert!(!self.root.join("nested/new").exists());
    }
}

#[test]
fn file_and_directory_moves_refresh_open_imports_on_every_contributing_server() {
    for directory in [false, true] {
        for preview in [false, true] {
            let Some(fixture) = Fixture::with_matching_servers("fresh", "fresh") else {
                return;
            };
            for (name, file) in [
                ("directory-fixture", "imports.refactor"),
                ("secondary-fixture", "imports.other"),
            ] {
                let path = fixture.root.join(file);
                let spec = fixture
                    .tool
                    .registry
                    .specs_for_file(&path)
                    .into_iter()
                    .find(|spec| spec.name == name)
                    .unwrap();
                fixture
                    .runtime
                    .block_on(fixture.tool.synced_for_spec_with_timeout(
                        &path,
                        spec,
                        std::time::Duration::from_secs(5),
                    ))
                    .unwrap();
            }
            std::fs::write(
                fixture.root.join("imports.refactor"),
                "header\r\n😀 old\r\n",
            )
            .unwrap();
            std::fs::write(
                fixture.root.join("imports.other"),
                "first\nsecond\n🦀 old\n",
            )
            .unwrap();
            let source = if directory { "old" } else { "old/lib.refactor" };
            let target = if directory {
                "nested/new"
            } else {
                "nested/new.refactor"
            };
            let details = fixture
                .run(json!({"action":"rename_file","file":source,
                "newFile":target,"apply":!preview,"timeout":5}))
                .unwrap()
                .details
                .unwrap();
            assert_eq!(details["serverCount"], 2);
            if preview {
                assert_eq!(
                    std::fs::read_to_string(fixture.root.join("imports.refactor")).unwrap(),
                    "header\r\n😀 old\r\n"
                );
                fixture.run(json!({"action":"rename_file","refactorId":details["refactorId"],"apply":true})).unwrap();
            }
            fixture.flush();
            assert_eq!(
                std::fs::read_to_string(fixture.root.join("imports.refactor")).unwrap(),
                "header\r\n😀 new\r\n"
            );
            assert_eq!(
                std::fs::read_to_string(fixture.root.join("imports.other")).unwrap(),
                "first\nsecond\n🦀 new\n"
            );
            assert!(!fixture.root.join(source).exists());
            assert!(fixture.root.join(target).exists());
            for prefix in ["", "secondary-"] {
                let messages = fixture
                    .peer_requests(&format!("{prefix}requests.jsonl"), "textDocument/didChange");
                assert_eq!(
                    messages.len(),
                    1,
                    "each contributor refreshes its own retained import"
                );
                let requests = fixture.peer_requests(
                    &format!("{prefix}requests.jsonl"),
                    "workspace/willRenameFiles",
                );
                assert_eq!(
                    requests.len(),
                    1,
                    "approval must reuse the original response"
                );
            }
        }
    }
}

#[test]
fn public_directory_move_updates_imports_and_notifies_after_commit() {
    let Some(fixture) = Fixture::new("normal") else {
        return;
    };
    let result = fixture.rename(false).unwrap();
    assert!(!result.is_error);
    let details = result.details.unwrap();
    assert_eq!(details["directory"], true);
    assert_eq!(details["filesMoved"], 3);
    assert_eq!(details["willRenamePairs"], 2);
    assert_eq!(details["didRenamePairs"], 2);
    fixture.flush();
    let notification: Value =
        serde_json::from_slice(&std::fs::read(fixture.root.join("notification.json")).unwrap())
            .unwrap();
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
    let Some(fixture) = Fixture::new("normal") else {
        return;
    };
    let preview = fixture.rename(true).unwrap().details.unwrap();
    fixture.unchanged();
    assert!(!fixture.root.join("nested").exists());
    assert_eq!(preview["preview"], true);
    assert!(
        preview["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| file["kind"] == "directoryRename")
    );
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
    let Some(fixture) = Fixture::new("normal") else {
        return;
    };
    let preview = fixture.rename(true).unwrap().details.unwrap();
    std::fs::write(fixture.root.join("old/added"), "keep").unwrap();
    let error = fixture
        .run(json!({"action":"rename_file","refactorId":preview["refactorId"],"apply":true}))
        .unwrap_err();
    assert!(error.to_string().contains("tree changed"), "{error}");
    fixture.unchanged();
    fixture.flush();
    assert!(fixture.requests("workspace/didRenameFiles").is_empty());
}

#[test]
fn server_errors_invalid_edits_and_workspace_escape_do_not_move_or_edit() {
    for mode in ["error", "invalid", "escape"] {
        let Some(fixture) = Fixture::new(mode) else {
            return;
        };
        assert!(fixture.rename(false).is_err(), "{mode}");
        fixture.unchanged();
        assert_eq!(
            std::fs::read(fixture.root.parent().unwrap().join("outside.refactor")).unwrap(),
            b"old\n"
        );
        fixture.flush();
        assert!(fixture.requests("workspace/didRenameFiles").is_empty());
    }
}

#[test]
fn folder_and_file_registrations_are_independent_and_unregistered_moves_are_explicit() {
    let Some(fixture) = Fixture::new("folder") else {
        return;
    };
    let details = fixture.rename(false).unwrap().details.unwrap();
    assert_eq!(details["willRenamePairs"], 1);
    assert_eq!(details["didRenamePairs"], 2);
    fixture.flush();
    let requests = fixture.requests("workspace/willRenameFiles");
    let old_uri = requests[0]["params"]["files"][0]["oldUri"]
        .as_str()
        .unwrap();
    assert!(old_uri.ends_with("/old"));
    let Some(unregistered) = Fixture::new("unregistered") else {
        return;
    };
    let details = unregistered.rename(false).unwrap().details.unwrap();
    assert_eq!(details["willRenameFiles"], false);
    assert_eq!(details["notificationRequested"], false);
    assert_eq!(
        std::fs::read(unregistered.root.join("nested/new/lib.refactor")).unwrap(),
        b"old\n"
    );
    assert!(
        unregistered
            .requests("workspace/willRenameFiles")
            .is_empty()
    );
}

#[test]
fn nested_roots_are_rejected_before_startup() {
    let Some(fixture) = Fixture::new("normal") else {
        return;
    };
    std::fs::write(fixture.root.join("old/sub/.refactor-root"), "").unwrap();
    assert!(fixture.rename(false).is_err());
    fixture.unchanged();
    assert!(!fixture.root.join("requests.jsonl").exists());
}

#[test]
fn unedited_file_drift_while_preparing_imports_cannot_be_blessed_by_restaging() {
    let Some(fixture) = Fixture::new("drift") else {
        return;
    };
    assert!(fixture.rename(false).is_err());
    fixture.unchanged();
    assert_eq!(
        std::fs::read(fixture.root.join("old/asset.bin")).unwrap(),
        b"external"
    );
    fixture.flush();
    assert!(fixture.requests("workspace/didRenameFiles").is_empty());
}

#[test]
fn request_budget_expires_without_a_move() {
    let Some(fixture) = Fixture::new("timeout") else {
        return;
    };
    let started = std::time::Instant::now();
    let result = fixture
        .run(json!({"action":"rename_file","file":"old","newFile":"nested/new","timeout":1}));
    assert!(result.is_err());
    assert!(started.elapsed() < std::time::Duration::from_secs(4));
    fixture.unchanged();
}

#[test]
fn multiple_languages_share_one_move_and_post_commit_notifications() {
    let Some(fixture) = Fixture::with_second("normal", Some("normal")) else {
        return;
    };
    let details = fixture.rename(false).unwrap().details.unwrap();
    assert_eq!(details["serverCount"], 2);
    assert_eq!(details["filesMoved"], 4);
    assert_eq!(details["willRenamePairs"], 3);
    assert_eq!(details["serverNotifications"].as_array().unwrap().len(), 2);
    fixture.flush();
    for name in ["lib.refactor", "secondary.other"] {
        assert_eq!(
            std::fs::read(fixture.root.join("nested/new").join(name)).unwrap(),
            b"new\n"
        );
    }
    for log in ["notification.json", "secondary-notification.json"] {
        let value: Value =
            serde_json::from_slice(&std::fs::read(fixture.root.join(log)).unwrap()).unwrap();
        assert_eq!(value["oldExists"], false);
        assert_eq!(value["imports"], "new\n");
        assert_eq!(value["otherImports"], "new\n");
    }
    assert_eq!(fixture.requests("workspace/willRenameFiles").len(), 1);
    assert_eq!(
        fixture
            .peer_requests("secondary-requests.jsonl", "workspace/willRenameFiles")
            .len(),
        1
    );
    eprintln!(
        "{}",
        json!({"case":"multi_language_commit","event":"verified","servers":2,"moves":1})
    );
}

#[test]
fn independent_duplicate_edits_apply_once_and_conflicts_apply_nothing() {
    let Some(fixture) = Fixture::with_second("shared", Some("shared")) else {
        return;
    };
    let details = fixture.rename(false).unwrap().details.unwrap();
    assert_eq!(details["deduplicatedEdits"], 1);
    assert_eq!(
        std::fs::read(fixture.root.join("shared.refactor")).unwrap(),
        b"Xbcdef\n"
    );
    let Some(fixture) = Fixture::with_second("shared", Some("conflict")) else {
        return;
    };
    let error = fixture.rename(false).unwrap_err();
    assert!(error.to_string().contains("LSP_RENAME_CONFLICT"), "{error}");
    fixture.unchanged();
    assert_eq!(
        std::fs::read(fixture.root.join("imports.other")).unwrap(),
        b"old\n"
    );
    assert_eq!(
        std::fs::read(fixture.root.join("shared.refactor")).unwrap(),
        b"abcdef\n"
    );
    fixture.flush();
    assert!(fixture.requests("workspace/didRenameFiles").is_empty());
    assert!(
        fixture
            .peer_requests("secondary-requests.jsonl", "workspace/didRenameFiles")
            .is_empty()
    );
    eprintln!(
        "{}",
        json!({"case":"multi_language_conflict","event":"verified","writes":0})
    );
}

#[test]
fn same_language_servers_both_contribute_to_a_directory_rename() {
    let Some(fixture) = Fixture::with_matching_servers("normal", "normal") else {
        return;
    };
    let details = fixture.rename(false).unwrap().details.unwrap();
    assert_eq!(details["serverCount"], 2);
    assert_eq!(details["willRenamePairs"], 4);
    assert_eq!(details["deduplicatedEdits"], 1);
    fixture.flush();
    assert_eq!(
        fixture
            .peer_requests("secondary-requests.jsonl", "workspace/willRenameFiles")
            .len(),
        1
    );
    assert_eq!(
        std::fs::read(fixture.root.join("imports.other")).unwrap(),
        b"new\n"
    );
    let requests = fixture.peer_requests("secondary-requests.jsonl", "textDocument/didOpen");
    assert!(requests.iter().any(|request| {
        request["params"]["textDocument"]["uri"]
            .as_str()
            .is_some_and(|uri| uri.ends_with("/old/lib.refactor"))
    }));
}

#[test]
fn single_file_review_retains_all_matching_servers_and_applies_once() {
    let Some(fixture) = Fixture::with_matching_servers("shared", "shared") else {
        return;
    };
    let preview = fixture
        .run(json!({"action":"rename_file","file":"old/lib.refactor",
        "newFile":"nested/new.destination","apply":false,"timeout":5}))
        .unwrap()
        .details
        .unwrap();
    fixture.unchanged();
    assert!(!fixture.root.join("nested").exists());
    assert_eq!(preview["directory"], false);
    assert_eq!(preview["serverCount"], 2);
    assert_eq!(preview["filesMoved"], 1);
    assert_eq!(preview["willRenamePairs"], 2);
    assert_eq!(preview["deduplicatedEdits"], 2);
    let approval = json!({"action":"rename_file","refactorId":preview["refactorId"],"apply":true});
    let applied = fixture.run(approval.clone()).unwrap().details.unwrap();
    assert_eq!(applied["applied"], true);
    fixture.flush();
    assert!(!fixture.root.join("old/lib.refactor").exists());
    assert_eq!(
        std::fs::read(fixture.root.join("nested/new.destination")).unwrap(),
        b"new\n"
    );
    assert_eq!(
        std::fs::read(fixture.root.join("imports.refactor")).unwrap(),
        b"new\n"
    );
    assert_eq!(
        std::fs::read(fixture.root.join("imports.other")).unwrap(),
        b"new\n"
    );
    assert_eq!(
        std::fs::read(fixture.root.join("shared.refactor")).unwrap(),
        b"Xbcdef\n"
    );
    for prefix in ["", "secondary-"] {
        assert_eq!(
            fixture
                .peer_requests(
                    &format!("{prefix}requests.jsonl"),
                    "workspace/willRenameFiles"
                )
                .len(),
            1
        );
        let evidence: Value = serde_json::from_slice(
            &std::fs::read(fixture.root.join(format!("{prefix}notification.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(evidence["oldExists"], false);
        assert_eq!(evidence["newExists"], true);
        assert_eq!(evidence["newText"], "new\n");
        assert_eq!(evidence["otherImports"], "new\n");
    }
    assert!(fixture.run(approval).is_err());
}

#[test]
fn a_second_matching_servers_conflict_or_error_prevents_a_single_file_move() {
    for mode in ["conflict", "error"] {
        let Some(fixture) = Fixture::with_matching_servers("shared", mode) else {
            return;
        };
        let error = fixture
            .run(json!({"action":"rename_file","file":"old/lib.refactor",
            "newFile":"nested/new.refactor","timeout":5}))
            .unwrap_err();
        if mode == "conflict" {
            assert!(error.to_string().contains("LSP_RENAME_CONFLICT"), "{error}");
        }
        fixture.unchanged();
        assert!(!fixture.root.join("nested/new.refactor").exists());
        assert_eq!(
            std::fs::read(fixture.root.join("imports.other")).unwrap(),
            b"old\n"
        );
        assert_eq!(
            std::fs::read(fixture.root.join("shared.refactor")).unwrap(),
            b"abcdef\n"
        );
        fixture.flush();
        assert!(fixture.requests("workspace/didRenameFiles").is_empty());
        assert!(
            fixture
                .peer_requests("secondary-requests.jsonl", "workspace/didRenameFiles")
                .is_empty()
        );
    }
}

#[test]
fn reload_invalidates_every_matching_participant_of_a_reviewed_file_move() {
    let Some(fixture) = Fixture::with_matching_servers("normal", "normal") else {
        return;
    };
    let preview = fixture
        .run(json!({"action":"rename_file","file":"old/lib.refactor",
        "newFile":"nested/new.refactor","apply":false,"timeout":5}))
        .unwrap()
        .details
        .unwrap();
    let reloaded = fixture
        .run(json!({"action":"reload","file":"old/lib.refactor"}))
        .unwrap()
        .details
        .unwrap();
    assert_eq!(reloaded["killed"], 2);
    assert!(
        fixture
            .run(json!({"action":"rename_file","refactorId":preview["refactorId"],"apply":true}))
            .is_err()
    );
    fixture.unchanged();
    assert!(!fixture.root.join("nested/new.refactor").exists());
}
