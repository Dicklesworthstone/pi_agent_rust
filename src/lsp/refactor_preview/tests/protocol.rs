//! Public-tool review/approval over a real Content-Length child connection.

use super::*;
use crate::config::Config;
use crate::tools::Tool as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

struct Fixture {
    root: PathBuf,
    tool: LspTool,
    runtime: asupersync::runtime::Runtime,
}

impl Fixture {
    fn new(root: &Path, mode: &str) -> Option<Self> {
        let python = ["python3", "python"].into_iter().find(|program| {
            Command::new(program).arg("--version").stdout(Stdio::null()).stderr(Stdio::null())
                .status().is_ok_and(|status| status.success())
        });
        let Some(python) = python else {
            assert!(std::env::var_os("PI_LSP_REQUIRE_PROTOCOL").is_none(),
                "PI_LSP_REQUIRE_PROTOCOL requires Python for the refactor approval peer");
            eprintln!("SKIP refactor approval protocol fixture: Python unavailable");
            return None;
        };
        std::fs::create_dir_all(root).unwrap();
        let root = root.canonicalize().unwrap();
        for name in ["source.preview", "sibling.preview"] {
            std::fs::write(root.join(name), "old\n").unwrap();
        }
        std::fs::write(root.join(".preview-root"), "").unwrap();
        let script = root.join("preview_peer.py");
        std::fs::write(&script, include_str!("server.py")).unwrap();
        let config: Config = serde_json::from_value(json!({"lsp":{"servers":{"preview-peer":{
            "command":python,"args":["-I","-u",script.to_str().unwrap(),mode],
            "extensions":[".preview"],"languages":["plaintext"],"rootMarkers":[".preview-root"]
        }}}})).unwrap();
        let tool = LspTool::new(&root, Some(&config));
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .worker_threads(1).enable_parking(false).build().unwrap();
        Some(Self { root, tool, runtime })
    }

    fn execute(&self, input: Value) -> Result<Value> {
        self.runtime.block_on(self.tool.execute("refactor-approval", input, None))
            .map(|output| { assert!(!output.is_error); output.details.unwrap() })
    }

    fn preview(&self, action: &str) -> Value {
        let input = if action == "rename" {
            json!({"action":"rename","file":"source.preview","symbol":"old","newName":"new","apply":false,"timeout":3})
        } else {
            json!({"action":"rename_file","file":"source.preview","newFile":"nested/new.preview","apply":false,"timeout":3})
        };
        self.execute(input).unwrap()
    }

    fn select(&self, action: &str, id: &Value, apply: bool) -> Result<Value> {
        self.execute(json!({"action":action,"refactorId":id,"apply":apply,"timeout":3}))
    }

    fn text(&self, file: &str) -> String {
        std::fs::read_to_string(self.root.join(file)).unwrap()
    }

    fn barrier(&self) -> Value {
        // A request on the same connection orders observation after notifications.
        self.execute(json!({"action":"request","file":"sibling.preview","method":"test/barrier","timeout":3}))
            .unwrap()["payload"]["result"].clone()
    }
}

#[test]
fn public_symbol_preview_inspect_and_approval_use_one_server_request() {
    let temp = tempfile::tempdir().unwrap();
    let Some(f) = Fixture::new(temp.path(), "plain") else { return };
    let preview = f.preview("rename");
    assert_eq!(preview["applied"], false);
    assert_eq!(preview["preview"], true);
    assert_eq!(preview["files"].as_array().unwrap().len(), 2);
    assert_eq!(preview["workspaceEdit"]["documentChanges"].as_array().unwrap().len(), 2);
    assert_eq!(f.text("source.preview"), "old\n");
    assert_eq!(f.text("sibling.preview"), "old\n");
    let inspected = f.execute(json!({"action":"rename","refactorId":preview["refactorId"]})).unwrap();
    assert_eq!(inspected["workspaceEdit"], preview["workspaceEdit"]);
    assert_eq!(inspected["refactorId"], preview["refactorId"]);
    let applied = f.select("rename", &preview["refactorId"], true).unwrap();
    assert_eq!(applied["applied"], true);
    assert_eq!(applied["preview"], false);
    assert_eq!(f.text("source.preview"), "new\n");
    assert_eq!(f.text("sibling.preview"), "new\n");
    assert!(f.select("rename", &preview["refactorId"], true).unwrap_err().to_string().contains("LSP_REFACTOR_STALE"));
    assert_eq!(f.barrier()["renames"], 1);
}

#[test]
fn file_preview_defers_imports_move_and_notification_until_approval() {
    let temp = tempfile::tempdir().unwrap();
    let Some(f) = Fixture::new(temp.path(), "plain") else { return };
    let preview = f.preview("rename_file");
    assert_eq!(preview["willRenameFiles"], true);
    assert_eq!(preview["notificationRequested"], true);
    assert_eq!(preview["notificationWritten"], false);
    assert_eq!(preview["workspaceEdit"]["documentChanges"].as_array().unwrap().last().unwrap()["kind"], "rename");
    assert_eq!(f.text("source.preview"), "old\n");
    assert_eq!(f.text("sibling.preview"), "old\n");
    assert!(!f.root.join("nested").exists());
    assert_eq!(f.barrier()["did"], 0);
    let applied = f.select("rename_file", &preview["refactorId"], true).unwrap();
    assert_eq!(applied["applied"], true);
    assert_eq!(applied["notificationWritten"], true);
    assert!(!f.root.join("source.preview").exists());
    assert_eq!(f.text("nested/new.preview"), "moved\n");
    assert_eq!(f.text("sibling.preview"), "moved\n");
    let observed = f.barrier();
    assert_eq!(observed["will"], 1);
    assert_eq!(observed["did"], 1);
    assert_eq!(observed["notifications"][0], json!({"oldExists":false,"newExists":true,"newText":"moved\n","siblingText":"moved\n"}));
    assert!(f.select("rename_file", &preview["refactorId"], true).is_err());
    assert_eq!(f.barrier()["did"], 1);
}

#[test]
fn approval_rejects_source_unopened_sibling_and_guard_only_drift() {
    for (mode, changed) in [("plain","source.preview"),("plain","sibling.preview"),("guard_only","source.preview")] {
        let temp = tempfile::tempdir().unwrap();
        let Some(f) = Fixture::new(temp.path(), mode) else { return };
        let preview = f.preview("rename");
        std::fs::write(f.root.join(changed), "external\n").unwrap();
        let error = f.select("rename", &preview["refactorId"], true).unwrap_err();
        assert!(error.to_string().contains("LSP_EDIT_CONFLICT"), "{mode}: {error}");
        for file in ["source.preview","sibling.preview"] {
            assert_eq!(f.text(file), if file == changed { "external\n" } else { "old\n" });
        }
        assert!(lock(&f.tool.refactors.0).is_none());
        assert_eq!(f.barrier()["renames"], 1);
    }
}

#[test]
fn approval_does_not_overwrite_a_recreated_move_destination_or_notify() {
    let temp = tempfile::tempdir().unwrap();
    let Some(f) = Fixture::new(temp.path(), "plain") else { return };
    let preview = f.preview("rename_file");
    std::fs::create_dir(f.root.join("nested")).unwrap();
    std::fs::write(f.root.join("nested/new.preview"), "external\n").unwrap();
    assert!(f.select("rename_file", &preview["refactorId"], true).is_err());
    assert_eq!(f.text("source.preview"), "old\n");
    assert_eq!(f.text("sibling.preview"), "old\n");
    assert_eq!(f.text("nested/new.preview"), "external\n");
    assert_eq!(f.barrier()["did"], 0);
    assert!(lock(&f.tool.refactors.0).is_none());
}

#[test]
fn wrong_action_and_override_selectors_do_not_consume_a_valid_plan() {
    let temp = tempfile::tempdir().unwrap();
    let Some(f) = Fixture::new(temp.path(), "plain") else { return };
    let preview = f.preview("rename");
    assert!(f.select("rename_file", &preview["refactorId"], true).unwrap_err().to_string().contains("LSP_USAGE"));
    assert!(f.execute(json!({"action":"rename","refactorId":preview["refactorId"],"newName":"wrong","apply":true}))
        .unwrap_err().to_string().contains("LSP_USAGE"));
    f.select("rename", &preview["refactorId"], true).unwrap();
    assert_eq!(f.text("source.preview"), "new\n");
    assert_eq!(f.barrier()["renames"], 1);
}

#[test]
fn foreign_tool_cannot_approve_another_tools_plan() {
    let temp = tempfile::tempdir().unwrap();
    let Some(f) = Fixture::new(temp.path(), "plain") else { return };
    let preview = f.preview("rename");
    let other = LspTool::new(&f.root, None);
    let error = f.runtime.block_on(other.execute("foreign", json!({"action":"rename","refactorId":preview["refactorId"],"apply":true}), None)).unwrap_err();
    assert!(error.to_string().contains("LSP_REFACTOR_STALE"));
    assert!(other.registry.status().is_empty());
    f.select("rename", &preview["refactorId"], true).unwrap();
}

#[test]
fn a_new_preview_retires_the_old_plan_without_changing_files() {
    let temp = tempfile::tempdir().unwrap();
    let Some(f) = Fixture::new(temp.path(), "plain") else { return };
    let first = f.preview("rename");
    let second = f.execute(json!({"action":"rename","file":"source.preview","symbol":"old","newName":"second","apply":false,"timeout":3})).unwrap();
    assert_ne!(first["refactorId"], second["refactorId"]);
    assert!(f.select("rename", &first["refactorId"], true).is_err());
    assert_eq!(f.text("source.preview"), "old\n");
    f.select("rename", &second["refactorId"], true).unwrap();
    assert_eq!(f.text("source.preview"), "second\n");
    assert_eq!(f.barrier()["renames"], 2);
}

#[test]
fn reload_and_dead_connections_retire_plans_without_new_servers() {
    for reload in [false,true] {
        let temp = tempfile::tempdir().unwrap();
        let Some(f) = Fixture::new(temp.path(), "plain") else { return };
        let preview = f.preview("rename");
        let entry = lock(&f.tool.refactors.0).as_ref().unwrap().entry.upgrade().unwrap();
        if reload { f.execute(json!({"action":"reload"})).unwrap(); } else { entry.client.kill(); }
        let error = f.select("rename", &preview["refactorId"], true).unwrap_err();
        assert!(error.to_string().contains("LSP_REFACTOR_STALE"));
        assert_eq!(f.text("source.preview"), "old\n");
        let log = std::fs::read_to_string(f.root.join("preview-requests.jsonl")).unwrap();
        let count = log.lines().filter(|line| serde_json::from_str::<Value>(line).unwrap()["method"] == "initialize").count();
        assert_eq!(count, 1);
    }
}

#[test]
fn closing_and_reopening_identical_text_invalidates_document_identity() {
    let temp = tempfile::tempdir().unwrap();
    let Some(f) = Fixture::new(temp.path(), "plain") else { return };
    let preview = f.preview("rename");
    let path = f.root.join("source.preview");
    let entry = lock(&f.tool.refactors.0).as_ref().unwrap().entry.upgrade().unwrap();
    entry.client.invalidate(&uri(&path));
    entry.client.ensure_synced(&path,"plaintext").unwrap();
    assert!(f.select("rename", &preview["refactorId"], true).unwrap_err().to_string().contains("LSP_REFACTOR_STALE"));
    assert_eq!(f.text("source.preview"), "old\n");
    assert_eq!(f.text("sibling.preview"), "old\n");
}

#[test]
fn inspecting_a_plan_does_not_extend_its_expiry() {
    let temp = tempfile::tempdir().unwrap();
    let Some(f) = Fixture::new(temp.path(), "plain") else { return };
    let preview = f.preview("rename");
    let original = Instant::now() - Duration::from_secs(120);
    lock(&f.tool.refactors.0).as_mut().unwrap().created = original;
    let inspected = f.select("rename", &preview["refactorId"], false).unwrap();
    assert!(inspected["expiresInSecs"].as_u64().unwrap() <= 180);
    assert_eq!(lock(&f.tool.refactors.0).as_ref().unwrap().created, original);
    lock(&f.tool.refactors.0).as_mut().unwrap().created = Instant::now() - PREVIEW_AGE - Duration::from_secs(1);
    assert!(f.select("rename", &preview["refactorId"], true).is_err());
    assert_eq!(f.text("source.preview"), "old\n");
}

#[test]
fn approval_requires_current_owner_io_and_cancellation_authority() {
    let temp = tempfile::tempdir().unwrap();
    let Some(f) = Fixture::new(temp.path(), "plain") else { return };
    let preview = f.preview("rename");
    let input: LspInput = serde_json::from_value(json!({"action":"rename","refactorId":preview["refactorId"],"apply":true})).unwrap();
    let restricted = asupersync::Cx::for_request().restrict::<asupersync::cx::cap::None>();
    let owner = {
        let _guard = restricted.set_current_restricted();
        AgentCx::for_current_or_request()
    };
    let error = f.tool.select_refactor(&input,&owner).unwrap_err();
    assert!(error.to_string().contains("LSP_EDIT_PERMISSION"));
    let cancelled = AgentCx::for_request();
    cancelled.cancel_with(asupersync::types::CancelKind::User, Some("approval cancelled"));
    let error = f.tool.select_refactor(&input,&cancelled).unwrap_err();
    assert!(error.to_string().contains("LSP_CANCELLED"));
    assert_eq!(f.text("source.preview"), "old\n");
    f.select("rename", &preview["refactorId"], true).unwrap();
}

#[test]
fn malformed_stale_oversized_and_failed_previews_never_issue_handles() {
    for (mode, code) in [("malformed","LSP_EDIT_MALFORMED"),("stale_version","LSP_EDIT_CONFLICT"),
        ("oversized","LSP_EDIT_LIMIT"),("error","refactor engine failed")] {
        let temp = tempfile::tempdir().unwrap();
        let Some(f) = Fixture::new(temp.path(), mode) else { return };
        let error = f.execute(json!({"action":"rename","file":"source.preview","symbol":"old","newName":"new","apply":false,"timeout":3})).unwrap_err();
        assert!(error.to_string().contains(code), "{mode}: {error}");
        assert!(lock(&f.tool.refactors.0).is_none());
        assert_eq!(f.text("source.preview"), "old\n");
        assert_eq!(f.text("sibling.preview"), "old\n");
    }
}

#[test]
fn out_of_workspace_preview_cannot_read_or_change_server_selected_targets() {
    let temp = tempfile::tempdir().unwrap();
    let outside = temp.path().join("outside.preview");
    std::fs::write(&outside,"outside\n").unwrap();
    let Some(f) = Fixture::new(&temp.path().join("workspace"), "escape") else { return };
    let error = f.execute(json!({"action":"rename","file":"source.preview","symbol":"old","newName":"new","apply":false,"timeout":3})).unwrap_err();
    assert!(error.to_string().contains("LSP_EDIT_SCOPE"), "{error}");
    assert!(lock(&f.tool.refactors.0).is_none());
    assert_eq!(std::fs::read_to_string(outside).unwrap(), "outside\n");
    assert_eq!(f.text("source.preview"), "old\n");
}

#[test]
fn preview_and_approval_never_authorize_unsolicited_server_edits() {
    for action in ["rename","rename_file"] {
        let temp = tempfile::tempdir().unwrap();
        let Some(f) = Fixture::new(temp.path(), "unsolicited") else { return };
        let preview = f.preview(action);
        assert_eq!(f.text("sibling.preview"), "old\n");
        let observed = f.barrier();
        assert_eq!(observed["unsolicitedApplied"], false);
        assert_eq!(observed["commands"], 0);
        f.select(action, &preview["refactorId"], true).unwrap();
        assert_eq!(f.text("sibling.preview"), if action == "rename" { "new\n" } else { "moved\n" });
        assert_eq!(f.barrier()["commands"], 0);
    }
}

#[test]
fn fresh_rename_keeps_immediate_application_when_apply_is_omitted() {
    let temp = tempfile::tempdir().unwrap();
    let Some(f) = Fixture::new(temp.path(), "plain") else { return };
    let output = f.execute(json!({"action":"rename","file":"source.preview","symbol":"old","newName":"direct","timeout":3})).unwrap();
    assert!(output.get("refactorId").is_none());
    assert_eq!(f.text("source.preview"), "direct\n");
    assert_eq!(f.text("sibling.preview"), "direct\n");
}

#[test]
fn unregistered_file_move_can_be_reviewed_without_import_or_notification_calls() {
    let temp = tempfile::tempdir().unwrap();
    let Some(f) = Fixture::new(temp.path(), "unregistered") else { return };
    let preview = f.preview("rename_file");
    assert_eq!(preview["willRenameFiles"], false);
    assert_eq!(preview["notificationRequested"], false);
    f.select("rename_file", &preview["refactorId"], true).unwrap();
    assert_eq!(f.text("nested/new.preview"), "old\n");
    assert_eq!(f.text("sibling.preview"), "old\n");
    let observed = f.barrier();
    assert_eq!(observed["will"], 0);
    assert_eq!(observed["did"], 0);
}
