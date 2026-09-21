//! Approval through real tool dispatch, an independent peer and real files.

use super::*;
use crate::config::{Config, LspServerSettings, LspSettings};
use crate::tools::Tool as _;
use std::collections::HashMap;

fn fixture(root: &Path, startup: &str) -> Option<LspTool> {
    let python = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|directory| directory.join("python3"))
            .find(|path| path.is_absolute() && path.is_file())
    });
    let Some(python) = python else {
        assert!(
            std::env::var_os("PI_LSP_REQUIRE_PROTOCOL").is_none(),
            "Python required for reviewed formatting tests"
        );
        eprintln!("SKIP reviewed formatting: Python unavailable; no protocol assertion executed");
        return None;
    };
    let root = root.canonicalize().unwrap();
    std::fs::write(root.join(".review-format-root"), "").unwrap();
    std::fs::write(root.join("source.reviewfmt"), "let x=1;\n").unwrap();
    std::fs::write(root.join("other.reviewfmt"), "untouched\n").unwrap();
    mode(&root, "normal");
    let server = root.join("review_server.py");
    std::fs::write(&server, include_str!("review_server.py")).unwrap();
    let config = Config {
        lsp: Some(LspSettings {
            servers: Some(HashMap::from([(
                "review-format-peer".to_string(),
                LspServerSettings {
                    command: Some(python.display().to_string()),
                    args: Some(vec![
                        "-I".into(),
                        "-u".into(),
                        server.display().to_string(),
                        startup.into(),
                    ]),
                    extensions: Some(vec![".reviewfmt".into()]),
                    languages: Some(vec!["plaintext".into()]),
                    root_markers: Some(vec![".review-format-root".into()]),
                    ..Default::default()
                },
            )])),
            ..Default::default()
        }),
        ..Default::default()
    };
    Some(LspTool::new(&root, Some(&config)))
}

fn runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap()
}

fn mode(root: &Path, value: &str) {
    std::fs::write(
        root.join("review-mode.json"),
        serde_json::to_string(value).unwrap(),
    )
    .unwrap();
}

fn input() -> Value {
    json!({"action":"format","file":"source.reviewfmt","timeout":5})
}

fn approve(id: &Value) -> Value {
    json!({"action":"format","refactorId":id,"apply":true,"timeout":5})
}

fn run(tool: &LspTool, rt: &asupersync::runtime::Runtime, args: Value) -> Result<Value> {
    let output = rt.block_on(tool.execute("review-format", args, None))?;
    assert!(!output.is_error);
    Ok(output.details.unwrap())
}

fn calls(root: &Path) -> Vec<Value> {
    std::fs::read_to_string(root.join("review-requests.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn count(root: &Path, method: &str) -> usize {
    // Startup may exhaust its budget before the child writes its first log.
    if !root.join("review-requests.jsonl").exists() {
        return 0;
    }
    calls(root)
        .iter()
        .filter(|frame| frame["method"] == method)
        .count()
}

fn source(root: &Path) -> String {
    std::fs::read_to_string(root.join("source.reviewfmt")).unwrap()
}

fn assert_original(root: &Path) {
    assert_eq!(source(root), "let x=1;\n");
    assert_eq!(
        std::fs::read_to_string(root.join("other.reviewfmt")).unwrap(),
        "untouched\n"
    );
}

#[test]
fn review_inspect_approve_uses_one_formatter_response_and_consumes_the_plan() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "normal") else {
        return;
    };
    let rt = runtime();
    let reviewed = run(&tool, &rt, input()).unwrap();
    let id = &reviewed["refactorId"];
    assert!(id.as_str().is_some());
    assert_eq!(reviewed["previewOnly"], true);
    assert_eq!(reviewed["workspaceEditComplete"], true);
    assert_eq!(
        reviewed["workspaceEdit"]["documentChanges"][0]["edits"][0]["newText"],
        "let x = 1;"
    );
    assert_original(temp.path());
    mode(temp.path(), "error"); // Any accidental request during approval fails loudly.
    let inspected = run(&tool, &rt, json!({"action":"format","refactorId":id})).unwrap();
    assert_eq!(inspected["workspaceEdit"], reviewed["workspaceEdit"]);
    assert_original(temp.path());
    let applied = run(&tool, &rt, approve(id)).unwrap();
    assert_eq!(applied["applied"], true);
    assert_eq!(applied["previewOnly"], false);
    assert_eq!(applied["preview"], false);
    assert_eq!(applied["changed"], true);
    assert_eq!(applied["notificationRequested"], false);
    assert_eq!(source(temp.path()), "let x = 1;\n");
    assert!(
        run(&tool, &rt, approve(id))
            .unwrap_err()
            .to_string()
            .contains("LSP_REFACTOR_STALE")
    );
    assert_eq!(count(temp.path(), "textDocument/formatting"), 1);
    assert_eq!(count(temp.path(), "workspace/executeCommand"), 0);
}

#[test]
fn a_fresh_apply_is_a_new_computation_and_retires_the_old_preview() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "normal") else {
        return;
    };
    let rt = runtime();
    let reviewed = run(&tool, &rt, input()).unwrap();
    let mut fresh = input();
    fresh["apply"] = json!(true);
    assert_eq!(run(&tool, &rt, fresh).unwrap()["applied"], true);
    assert_eq!(source(temp.path()), "RECOMPUTED\n");
    assert!(run(&tool, &rt, approve(&reviewed["refactorId"])).is_err());
    assert_eq!(count(temp.path(), "textDocument/formatting"), 2);
}

#[test]
fn range_approval_retains_unicode_selection_and_format_options() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "normal") else {
        return;
    };
    let rt = runtime();
    std::fs::write(temp.path().join("source.reviewfmt"), "😀x\rnext\r\n").unwrap();
    let range = json!({"start":{"line":0,"character":2},"end":{"line":0,"character":3}});
    let options = json!({"tabSize":8,"insertSpaces":false,"trimTrailingWhitespace":true});
    let mut args = input();
    args["range"] = range.clone();
    args["formatOptions"] = options.clone();
    let reviewed = run(&tool, &rt, args).unwrap();
    assert_eq!(reviewed["mode"], "range");
    assert_eq!(reviewed["formatOptions"], options);
    assert_eq!(reviewed["range"], range);
    run(&tool, &rt, approve(&reviewed["refactorId"])).unwrap();
    assert_eq!(source(temp.path()), "😀formatted\rnext\r\n");
    assert_eq!(count(temp.path(), "textDocument/rangeFormatting"), 1);
    let requests = calls(temp.path());
    let request = requests
        .iter()
        .find(|frame| frame["method"] == "textDocument/rangeFormatting")
        .unwrap();
    assert_eq!(request["params"]["range"], range);
    assert_eq!(request["params"]["options"], options);
}

#[test]
fn approved_noops_are_consumed_without_rewriting_original_files() {
    for setting in ["null", "empty", "same"] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), "normal") else {
            return;
        };
        let rt = runtime();
        mode(temp.path(), setting);
        let reviewed = run(&tool, &rt, input()).unwrap();
        assert_eq!(reviewed["changed"], false);
        let before = std::fs::metadata(temp.path().join("source.reviewfmt"))
            .unwrap()
            .modified()
            .unwrap();
        let applied = run(&tool, &rt, approve(&reviewed["refactorId"])).unwrap();
        assert_eq!(applied["changed"], false);
        assert_eq!(applied["applied"], false);
        assert_eq!(applied["filesChanged"], json!([]));
        assert_eq!(applied["previewOnly"], false);
        assert_eq!(applied["returnedNull"], setting == "null");
        assert_eq!(
            before,
            std::fs::metadata(temp.path().join("source.reviewfmt"))
                .unwrap()
                .modified()
                .unwrap()
        );
        assert_original(temp.path());
        assert!(run(&tool, &rt, approve(&reviewed["refactorId"])).is_err());
    }
}

#[test]
fn external_edits_or_removal_reject_approval_without_a_second_request() {
    for remove in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), "normal") else {
            return;
        };
        let rt = runtime();
        let reviewed = run(&tool, &rt, input()).unwrap();
        let path = temp.path().join("source.reviewfmt");
        if remove {
            std::fs::rename(&path, temp.path().join("saved.reviewfmt")).unwrap();
        } else {
            std::fs::write(&path, "external edit\n").unwrap();
        }
        let error = run(&tool, &rt, approve(&reviewed["refactorId"])).unwrap_err();
        assert!(error.to_string().contains("LSP_EDIT_CONFLICT"), "{error}");
        if remove {
            assert!(!path.exists());
        } else {
            assert_eq!(source(temp.path()), "external edit\n");
        }
        assert_eq!(count(temp.path(), "textDocument/formatting"), 1);
    }
}

#[test]
fn same_text_close_reopen_invalidates_versioned_and_unversioned_previews() {
    for startup in ["normal", "nosync"] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), startup) else {
            return;
        };
        let rt = runtime();
        let reviewed = run(&tool, &rt, input()).unwrap();
        let path = temp.path().join("source.reviewfmt").canonicalize().unwrap();
        let (uri, entry) = rt.block_on(tool.synced(&path)).unwrap();
        entry.client.invalidate(&uri);
        entry.client.ensure_synced(&path, "plaintext").unwrap();
        let error = run(&tool, &rt, approve(&reviewed["refactorId"])).unwrap_err();
        assert!(error.to_string().contains("LSP_REFACTOR_STALE"), "{error}");
        assert_original(temp.path());
        assert_eq!(count(temp.path(), "textDocument/formatting"), 1);
    }
}

#[test]
fn selectors_and_other_actions_cannot_override_or_consume_a_valid_format_plan() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "normal") else {
        return;
    };
    let rt = runtime();
    let reviewed = run(&tool, &rt, input()).unwrap();
    for extra in [
        json!({"file":"other.reviewfmt"}),
        json!({"formatOptions":{"tabSize":2}}),
        json!({"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}}),
        json!({"action":"rename"}),
        json!({"query":"new action"}),
    ] {
        let mut args = approve(&reviewed["refactorId"]);
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert!(
            run(&tool, &rt, args)
                .unwrap_err()
                .to_string()
                .contains("LSP_USAGE")
        );
        assert_original(temp.path());
    }
    run(&tool, &rt, approve(&reviewed["refactorId"])).unwrap();
    assert_eq!(count(temp.path(), "textDocument/formatting"), 1);
}

#[test]
fn invalid_fresh_requests_leave_the_existing_preview_usable() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "normal") else {
        return;
    };
    let rt = runtime();
    let reviewed = run(&tool, &rt, input()).unwrap();
    for extra in [
        json!({"formatOptions":{"tabSize":0}}),
        json!({"query":"other"}),
        json!({"range":{"start":{"line":99,"character":0},"end":{"line":99,"character":0}}}),
    ] {
        let mut args = input();
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert!(run(&tool, &rt, args).is_err());
    }
    run(&tool, &rt, approve(&reviewed["refactorId"])).unwrap();
    assert_eq!(count(temp.path(), "textDocument/formatting"), 1);
}

#[test]
fn full_edit_remains_reviewable_when_the_overview_is_shortened() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "normal") else {
        return;
    };
    let rt = runtime();
    mode(temp.path(), "summary");
    let reviewed = run(&tool, &rt, input()).unwrap();
    assert_eq!(reviewed["previewTruncated"], true);
    assert_eq!(reviewed["edits"], json!([]));
    assert_eq!(reviewed["workspaceEditComplete"], true);
    assert_eq!(
        reviewed["workspaceEdit"]["documentChanges"][0]["edits"][0]["newText"]
            .as_str()
            .unwrap()
            .len(),
        70000
    );
    run(&tool, &rt, approve(&reviewed["refactorId"])).unwrap();
    assert_eq!(source(temp.path()), format!("{}\n", "x".repeat(70000)));
    assert_eq!(count(temp.path(), "textDocument/formatting"), 1);
}

#[test]
fn failed_or_oversized_fresh_computation_retires_previous_approval() {
    for setting in [
        "error",
        "invalid",
        "overlap",
        "preview-limit",
        "response-limit",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), "normal") else {
            return;
        };
        let rt = runtime();
        let reviewed = run(&tool, &rt, input()).unwrap();
        mode(temp.path(), setting);
        let error = run(&tool, &rt, input()).unwrap_err();
        if setting == "preview-limit" {
            assert!(
                error.to_string().contains("LSP_EDIT_LIMIT"),
                "{setting}: {error}"
            );
        }
        assert!(
            run(&tool, &rt, approve(&reviewed["refactorId"]))
                .unwrap_err()
                .to_string()
                .contains("LSP_REFACTOR_STALE")
        );
        assert_original(temp.path());
        assert!(!std::fs::read_dir(temp.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".pi-lsp-")
        }));
    }
}

#[test]
fn preview_never_opens_command_or_unsolicited_edit_permission() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "normal") else {
        return;
    };
    let rt = runtime();
    mode(temp.path(), "callback");
    let reviewed = run(&tool, &rt, input()).unwrap();
    let reply: Value = serde_json::from_str(
        &std::fs::read_to_string(temp.path().join("format-probe.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(reply["result"]["applied"], false);
    assert_original(temp.path());
    run(&tool, &rt, approve(&reviewed["refactorId"])).unwrap();
    assert_eq!(
        std::fs::read_to_string(temp.path().join("other.reviewfmt")).unwrap(),
        "untouched\n"
    );
    assert_eq!(count(temp.path(), "workspace/executeCommand"), 0);
}

#[test]
fn restricted_formatting_is_rejected_before_startup_and_cannot_consume_approval() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "normal") else {
        return;
    };
    let rt = runtime();
    rt.block_on(async {
        let restricted = asupersync::Cx::for_request().restrict::<asupersync::cx::cap::None>();
        let _guard = restricted.set_current_restricted();
        let error = tool.execute("restricted", input(), None).await.unwrap_err();
        assert!(error.to_string().contains("LSP_EDIT_PERMISSION"), "{error}");
    });
    assert!(tool.registry.status().is_empty());
    assert!(!temp.path().join("review-requests.jsonl").exists());
    let reviewed = run(&tool, &rt, input()).unwrap();
    rt.block_on(async {
        let restricted = asupersync::Cx::for_request().restrict::<asupersync::cx::cap::None>();
        let _guard = restricted.set_current_restricted();
        let error = tool
            .execute(
                "restricted-approval",
                approve(&reviewed["refactorId"]),
                None,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("LSP_EDIT_PERMISSION"), "{error}");
    });
    run(&tool, &rt, approve(&reviewed["refactorId"])).unwrap();
}

#[test]
fn dropped_format_request_cancels_and_releases_the_workflow_lane() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "normal") else {
        return;
    };
    let rt = runtime();
    mode(temp.path(), "hang");
    rt.block_on(async {
        let mut pending = Box::pin(tool.execute("dropped-format", input(), None));
        let owner = AgentCx::for_current_or_request();
        let mut posted = false;
        for _ in 0..1000 {
            assert!(futures::poll!(pending.as_mut()).is_pending());
            if let Ok(log) = std::fs::read_to_string(temp.path().join("review-requests.jsonl"))
                && log.lines().any(|line| {
                    serde_json::from_str::<Value>(line)
                        .is_ok_and(|frame| frame["method"] == "textDocument/formatting")
                })
            {
                posted = true;
                break;
            }
            owner.time().sleep(Duration::from_millis(5)).await;
        }
        assert!(posted);
        drop(pending);
        tool.execute(
            "barrier",
            json!({"action":"request","file":"source.reviewfmt","method":"test/count","timeout":5}),
            None,
        )
        .await
        .unwrap();
    });
    let frames = calls(temp.path());
    let request = frames
        .iter()
        .find(|frame| frame["method"] == "textDocument/formatting")
        .unwrap();
    let cancel = frames
        .iter()
        .find(|frame| frame["method"] == "$/cancelRequest")
        .unwrap();
    assert_eq!(cancel["params"]["id"], request["id"]);
    assert_original(temp.path());
}

#[test]
fn startup_is_inside_the_format_request_budget() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "startup-hang") else {
        return;
    };
    let mut args = input();
    args["timeout"] = json!(1);
    let error = run(&tool, &runtime(), args).unwrap_err();
    assert!(error.to_string().contains("LSP_TIMEOUT"), "{error}");
    assert_original(temp.path());
    assert_eq!(count(temp.path(), "textDocument/formatting"), 0);
}

#[test]
fn format_budget_does_not_restart_and_checks_cancellation() {
    let mut budget = FormatBudget::new(Duration::from_secs(3600));
    assert!(budget.remaining().is_ok());
    budget.started = Instant::now()
        .checked_sub(Duration::from_secs(7200))
        .unwrap();
    assert!(
        budget
            .remaining()
            .unwrap_err()
            .to_string()
            .contains("LSP_TIMEOUT")
    );
    let budget = FormatBudget::new(Duration::from_secs(10));
    budget.owner.cancel_with(
        asupersync::types::CancelKind::User,
        Some("cancel formatting"),
    );
    assert!(
        budget
            .remaining()
            .unwrap_err()
            .to_string()
            .contains("LSP_CANCELLED")
    );
}

#[test]
fn fresh_format_rejects_unrelated_selectors_before_reading_any_source() {
    let temp = tempfile::tempdir().unwrap();
    let tool = LspTool::new(temp.path(), None);
    let rt = runtime();
    for extra in [
        json!({"query":"1"}),
        json!({"limit":1}),
        json!({"newName":"x"}),
        json!({"newFile":"x"}),
        json!({"actionId":"x"}),
        json!({"hierarchyId":"x"}),
        json!({"method":"workspace/executeCommand"}),
        json!({"payload":{}}),
    ] {
        let mut args = input();
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert!(
            run(&tool, &rt, args)
                .unwrap_err()
                .to_string()
                .contains("LSP_USAGE")
        );
    }
    assert!(tool.registry.status().is_empty());
}
