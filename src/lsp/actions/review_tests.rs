// Included inside actions::tests. Uses the real tool, transaction and stdio peer.

fn review_listing(tool: &LspTool, runtime: &asupersync::runtime::Runtime) -> Value {
    run(tool, runtime, json!({"action":"code_actions","file":"source.lspfixture","timeout":5}))
        .unwrap().details.unwrap()
}

fn review_preview(tool: &LspTool, runtime: &asupersync::runtime::Runtime) -> Value {
    let listing = review_listing(tool, runtime);
    run(tool, runtime, json!({"action":"code_actions","actionId":listing["actions"][0]["actionId"],"timeout":5}))
        .unwrap().details.unwrap()
}

fn review_approve(tool: &LspTool, runtime: &asupersync::runtime::Runtime, id: &Value) -> Result<ToolOutput> {
    run(tool, runtime, json!({"action":"code_actions","refactorId":id,"apply":true,"timeout":5}))
}

fn review_read(root: &Path, file: &str) -> String {
    std::fs::read_to_string(root.join(file)).unwrap()
}

#[test]
fn action_review_approves_the_exact_inline_or_resolved_plan_once() {
    for mode in ["review-inline", "review-lazy"] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), mode) else { return };
        let runtime = runtime();
        let preview = review_preview(&tool, &runtime);
        assert_eq!(preview["preview"], true);
        assert_eq!(preview["applied"], false);
        assert_eq!(preview["executedCommand"], Value::Null);
        assert_eq!(preview["files"].as_array().unwrap().len(), 2);
        assert_eq!(review_read(temp.path(), "source.lspfixture"), "old\n");
        assert_eq!(review_read(temp.path(), "sibling.lspfixture"), "old\n");
        let id = &preview["refactorId"];
        for _ in 0..2 {
            let inspected = run(&tool, &runtime, json!({"action":"code_actions","refactorId":id})).unwrap().details.unwrap();
            assert_eq!(inspected["workspaceEdit"], preview["workspaceEdit"]);
        }
        let applied = review_approve(&tool, &runtime, id).unwrap();
        assert!(!applied.is_error);
        let result = applied.details.unwrap();
        assert_eq!(result["applied"], true);
        assert_eq!(result["filesChanged"].as_array().unwrap().len(), 2);
        assert_eq!(result["serverEditRequests"], 0);
        assert_eq!(review_read(temp.path(), "source.lspfixture"), "fixed\n");
        assert_eq!(review_read(temp.path(), "sibling.lspfixture"), "fixed\n");
        let calls = methods(temp.path());
        assert_eq!(calls.iter().filter(|m| *m == "textDocument/codeAction").count(), 1);
        assert_eq!(calls.iter().filter(|m| *m == "codeAction/resolve").count(), usize::from(mode == "review-lazy"));
        assert!(!calls.iter().any(|m| m == "workspace/executeCommand"));
        assert!(lock(&tool.actions.active).is_none());
        assert!(review_approve(&tool, &runtime, id).unwrap_err().to_string().contains("LSP_REFACTOR_STALE"));
    }
}

#[test]
fn action_review_fresh_query_preserves_selected_extraction_and_kind_filter() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = selection_fixture(temp.path(), "review-extract") else { return };
    let runtime = runtime();
    let mut request = extract_request();
    request["query"] = json!("1");
    request["apply"] = json!(false);
    let preview = run(&tool, &runtime, request).unwrap().details.unwrap();
    assert_eq!(preview["title"], "Extract selected expression");
    assert_eq!(preview["kind"], "refactor.extract.function");
    assert_eq!(review_read(temp.path(), "source.lspfixture"), SELECTION_SOURCE);
    review_approve(&tool, &runtime, &preview["refactorId"]).unwrap();
    assert_eq!(review_read(temp.path(), "source.lspfixture"), SELECTION_RESULT);
    assert!(!temp.path().join("command-started").exists());
}

#[test]
fn action_review_detects_source_unopened_sibling_and_guard_only_drift() {
    for (mode, changed) in [("review-lazy", "source.lspfixture"),
        ("review-lazy", "sibling.lspfixture"), ("review-guard", "source.lspfixture")] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), mode) else { return };
        let runtime = runtime();
        let preview = review_preview(&tool, &runtime);
        std::fs::write(temp.path().join(changed), "external\n").unwrap();
        let error = review_approve(&tool, &runtime, &preview["refactorId"]).unwrap_err();
        assert!(error.to_string().contains("LSP_EDIT_CONFLICT"), "{mode}: {error}");
        for path in ["source.lspfixture", "sibling.lspfixture"] {
            assert_eq!(review_read(temp.path(), path), if path == changed { "external\n" } else { "old\n" });
        }
        assert!(review_approve(&tool, &runtime, &preview["refactorId"]).unwrap_err().to_string().contains("LSP_REFACTOR_STALE"));
    }
}

#[test]
fn action_review_preserves_listing_time_evidence_for_synchronized_siblings() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "review-known-sibling") else { return };
    let runtime = runtime();
    let sibling = temp.path().join("sibling.lspfixture").canonicalize().unwrap();
    runtime.block_on(tool.synced(&sibling)).unwrap();
    let listing = review_listing(&tool, &runtime);
    std::fs::write(&sibling, "external\n").unwrap();
    let error = run(&tool, &runtime, json!({"action":"code_actions","actionId":listing["actions"][0]["actionId"]})).unwrap_err();
    assert!(error.to_string().contains("LSP_EDIT_CONFLICT"), "{error}");
    assert_eq!(review_read(temp.path(), "source.lspfixture"), "old\n");
    assert_eq!(review_read(temp.path(), "sibling.lspfixture"), "external\n");
}

#[test]
fn action_review_stages_ordered_resource_operations_without_creating_directories() {
    for conflict in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), "review-resource") else { return };
        let runtime = runtime();
        let preview = review_preview(&tool, &runtime);
        assert!(!temp.path().join("generated").exists());
        let destination = temp.path().join("generated/moved.lspfixture");
        if conflict {
            std::fs::create_dir(temp.path().join("generated")).unwrap();
            std::fs::write(&destination, "external\n").unwrap();
            assert!(review_approve(&tool, &runtime, &preview["refactorId"]).is_err());
            assert_eq!(review_read(temp.path(), "source.lspfixture"), "old\n");
            assert_eq!(review_read(temp.path(), "sibling.lspfixture"), "old\n");
            assert_eq!(std::fs::read_to_string(destination).unwrap(), "external\n");
        } else {
            review_approve(&tool, &runtime, &preview["refactorId"]).unwrap();
            assert_eq!(review_read(temp.path(), "source.lspfixture"), "fixed\n");
            assert!(!temp.path().join("sibling.lspfixture").exists());
            assert_eq!(std::fs::read_to_string(destination).unwrap(), "old\n");
        }
    }
}

#[test]
fn action_review_never_previews_only_the_inline_half_of_a_command() {
    for mode in ["review-command-inline", "review-command-lazy", "review-command-only"] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), mode) else { return };
        let runtime = runtime();
        let error = run(&tool, &runtime, json!({"action":"code_actions","file":"source.lspfixture","query":"1","apply":false})).unwrap_err();
        assert!(error.to_string().contains("LSP_ACTION_NOT_PREVIEWABLE"), "{error}");
        assert_eq!(review_read(temp.path(), "source.lspfixture"), "old\n");
        assert_eq!(review_read(temp.path(), "sibling.lspfixture"), "old\n");
        assert!(lock(&tool.actions.active).is_none());
        assert!(!temp.path().join("command-started").exists());
        if mode != "review-command-only" {
            let output = run(&tool, &runtime, json!({"action":"code_actions","file":"source.lspfixture","query":"1","apply":true})).unwrap();
            assert!(!output.is_error);
            assert!(temp.path().join("command-started").exists());
        }
    }
}

#[test]
fn action_review_rejects_bad_resolution_without_mutating_any_target() {
    for (mode, expected) in [("review-stale", "LSP_EDIT_CONFLICT"),
        ("review-changed", "LSP_ACTION_CHANGED"), ("review-malformed", "LSP_EDIT_MALFORMED"),
        ("review-error", "refactor resolution failed"), ("review-disabled", "LSP_ACTION_DISABLED"),
        ("review-drift", "LSP_EDIT_CONFLICT"), ("review-oversized", "LSP_EDIT_LIMIT")] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), mode) else { return };
        let error = run(&tool, &runtime(), json!({"action":"code_actions","file":"source.lspfixture","query":"1","apply":false,"timeout":5})).unwrap_err();
        assert!(error.to_string().contains(expected), "{mode}: {error}");
        assert_eq!(review_read(temp.path(), "source.lspfixture"), if mode == "review-drift" { "external\n" } else { "old\n" });
        assert_eq!(review_read(temp.path(), "sibling.lspfixture"), "old\n");
        assert!(!temp.path().join("command-started").exists());
    }
}

#[test]
fn action_review_scope_checks_precede_reading_server_selected_paths() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    let Some(tool) = fixture(&root, "review-scope") else { return };
    // The escaping target deliberately does not exist. Scope, not file I/O,
    // must be the reason for rejecting this server response.
    let error = run(&tool, &runtime(), json!({"action":"code_actions","file":"source.lspfixture","query":"1","apply":false})).unwrap_err();
    assert!(error.to_string().contains("LSP_EDIT_SCOPE"), "{error}");
    assert_eq!(review_read(&root, "source.lspfixture"), "old\n");
    assert!(!temp.path().join("outside.lspfixture").exists());
}

#[test]
fn action_review_resolution_does_not_authorize_unsolicited_callbacks() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "review-probe") else { return };
    let runtime = runtime();
    let preview = review_preview(&tool, &runtime);
    let probe: Value = serde_json::from_str(&review_read(temp.path(), "review-probe.json")).unwrap();
    assert_eq!(probe["applied"], false);
    assert_eq!(review_read(temp.path(), "sibling.lspfixture"), "old\n");
    assert!(lock(&tool.actions.active).is_none());
    review_approve(&tool, &runtime, &preview["refactorId"]).unwrap();
}

#[test]
fn action_review_rejects_overrides_and_wrong_action_without_consuming_the_plan() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "review-inline") else { return };
    let runtime = runtime();
    let preview = review_preview(&tool, &runtime);
    for extra in [json!({"action":"rename"}), json!({"file":"source.lspfixture"}),
        json!({"query":"1"}), json!({"actionId":"other"}), json!({"only":["refactor"]})] {
        let mut request = json!({"action":"code_actions","refactorId":preview["refactorId"],"apply":true});
        request.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        assert!(run(&tool, &runtime, request).unwrap_err().to_string().contains("LSP_USAGE"));
    }
    review_approve(&tool, &runtime, &preview["refactorId"]).unwrap();
}

#[test]
fn action_review_new_listing_reload_and_close_reopen_retire_old_plans() {
    for retirement in ["listing", "reload", "incarnation"] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), "review-inline") else { return };
        let runtime = runtime();
        let preview = review_preview(&tool, &runtime);
        match retirement {
            "listing" => { review_listing(&tool, &runtime); }
            "reload" => { run(&tool, &runtime, json!({"action":"reload"})).unwrap(); }
            _ => {
                let source = temp.path().join("source.lspfixture").canonicalize().unwrap();
                let (uri, entry) = runtime.block_on(tool.synced(&source)).unwrap();
                entry.client.invalidate(&uri);
                runtime.block_on(tool.synced(&source)).unwrap();
            }
        }
        assert!(review_approve(&tool, &runtime, &preview["refactorId"]).is_err(), "{retirement}");
        assert_eq!(review_read(temp.path(), "source.lspfixture"), "old\n");
    }
}

#[test]
fn action_review_admission_rejects_ambiguous_selectors_before_server_start() {
    let temp = tempfile::tempdir().unwrap();
    let tool = LspTool::new(temp.path(), None);
    let runtime = runtime();
    for extra in [json!({"actionId":""}), json!({"actionId":"x".repeat(129)}),
        json!({"actionId":"id","query":"1"}), json!({"query":" "}),
        json!({"query":"x".repeat(1025)}), json!({"newName":"ignored"}),
        json!({"method":"workspace/executeCommand"}), json!({"limit":1})] {
        let mut request = json!({"action":"code_actions","file":"missing.rs","apply":false});
        request.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        assert!(run(&tool, &runtime, request).unwrap_err().to_string().contains("LSP_USAGE"));
    }
    assert!(tool.registry.status().is_empty());
}

#[test]
fn action_review_missing_io_authority_blocks_listing_before_file_access() {
    let temp = tempfile::tempdir().unwrap();
    let tool = LspTool::new(temp.path(), None);
    runtime().block_on(async {
        let restricted = asupersync::Cx::for_request().restrict::<asupersync::cx::cap::None>();
        let _guard = restricted.set_current_restricted();
        let error = tool.execute("restricted-review", json!({"action":"code_actions","file":"missing.rs"}), None).await.unwrap_err();
        assert!(error.to_string().contains("LSP_EDIT_PERMISSION"), "{error}");
    });
    assert!(tool.registry.status().is_empty());
}

#[test]
fn action_review_dropping_pending_resolution_cancels_and_releases_tool_lane() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "review-stall") else { return };
    let runtime = runtime();
    runtime.block_on(async {
        let mut pending = Box::pin(tool.execute("review-drop", json!({"action":"code_actions","file":"source.lspfixture","query":"1","apply":false,"timeout":20}), None));
        let owner = AgentCx::for_current_or_request();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !temp.path().join("resolve-started").exists() {
            assert!(std::time::Instant::now() < deadline, "resolver never started");
            assert!(futures::poll!(pending.as_mut()).is_pending());
            owner.time().sleep(std::time::Duration::from_millis(10)).await;
        }
        drop(pending);
    });
    // A server-roundtrip barrier also drains the queued cancellation frame.
    run(&tool, &runtime, json!({"action":"request","file":"source.lspfixture","method":"test/versions","timeout":5})).unwrap();
    assert!(methods(temp.path()).iter().any(|method| method == "$/cancelRequest"));
    assert!(lock(&tool.actions.active).is_none());
    assert_eq!(review_read(temp.path(), "source.lspfixture"), "old\n");
}
