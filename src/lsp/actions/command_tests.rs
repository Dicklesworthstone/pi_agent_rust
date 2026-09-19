// Included in actions::tests: run the public tool with a real framed peer.
fn command_request() -> Value {
    json!({"action":"code_actions","file":"source.lspfixture","apply":true,"query":"1","timeout":5})
}

fn command_results(root: &Path) -> Vec<Value> {
    serde_json::from_str(&std::fs::read_to_string(root.join("command-results.json")).unwrap()).unwrap()
}

#[test]
fn command_callbacks_accept_current_versions_then_continue_with_guarded_unversioned_edits() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "command-versions") else { return; };
    let runtime = runtime();
    let output = run(&tool, &runtime, command_request()).unwrap();
    assert!(!output.is_error);
    let details = output.details.unwrap();
    assert_eq!(details["serverEditRequests"], 2);
    assert_eq!(details["filesChanged"], json!(["source.lspfixture"]));
    assert_eq!(std::fs::read_to_string(temp.path().join("source.lspfixture")).unwrap(), "two\n");
    assert!(command_results(temp.path()).iter().all(|result| result["applied"] == true));
    assert!(lock(&tool.actions.active).is_none());
    assert_eq!(tool.actions.admission.load(Ordering::Acquire), 0);
    let late = run(&tool, &runtime, json!({"action":"request","file":"source.lspfixture","method":"test/lateEdit"})).unwrap();
    assert_eq!(late.details.unwrap()["payload"]["result"]["applied"], false);
}

#[test]
fn stale_or_unknown_callback_versions_revoke_the_whole_command_edit_window() {
    for mode in ["command-stale", "command-unknown"] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), mode) else { return; };
        let output = run(&tool, &runtime(), command_request()).unwrap();
        assert!(output.is_error, "{mode}");
        let details = output.details.unwrap();
        assert_eq!(details["partial"], false);
        assert_eq!(details["serverEditRequests"], 2);
        let results = command_results(temp.path());
        assert_eq!(results[0]["applied"], false);
        assert!(results[0]["failureReason"].as_str().unwrap().contains("LSP_EDIT_CONFLICT"));
        assert_eq!(results[1]["applied"], false);
        assert!(results[1]["failureReason"].as_str().unwrap().contains("LSP_EDIT_REVOKED"));
        for file in ["source.lspfixture", "sibling.lspfixture"] {
            assert_eq!(std::fs::read_to_string(temp.path().join(file)).unwrap(), "old\n");
        }
    }
}

#[test]
fn inline_edits_preserve_untouched_sibling_version_evidence_for_commands() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "command-inline-sibling") else { return; };
    let runtime = runtime();
    let sibling = temp.path().join("sibling.lspfixture").canonicalize().unwrap();
    runtime.block_on(tool.synced(&sibling)).unwrap();
    let output = run(&tool, &runtime, command_request()).unwrap();
    assert!(!output.is_error);
    assert_eq!(output.details.unwrap()["filesChanged"].as_array().unwrap().len(), 2);
    assert_eq!(std::fs::read_to_string(temp.path().join("source.lspfixture")).unwrap(), "fixed\n");
    assert_eq!(std::fs::read_to_string(sibling).unwrap(), "one\n");
    assert_eq!(command_results(temp.path())[0]["applied"], true);
}

#[test]
fn cached_command_retains_listing_time_sibling_evidence_even_after_inline_edits() {
    for resync in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), "command-inline-sibling") else { return; };
        let runtime = runtime();
        let sibling = temp.path().join("sibling.lspfixture").canonicalize().unwrap();
        runtime.block_on(tool.synced(&sibling)).unwrap();
        let listing = run(&tool, &runtime, json!({"action":"code_actions","file":"source.lspfixture"})).unwrap().details.unwrap();
        std::fs::write(&sibling, "external sibling\n").unwrap();
        if resync { runtime.block_on(tool.synced(&sibling)).unwrap(); }
        let output = run(&tool, &runtime, json!({"action":"code_actions","apply":true,
            "actionId":listing["actions"][0]["actionId"]})).unwrap();
        assert!(output.is_error);
        assert_eq!(output.details.unwrap()["partial"], true);
        assert_eq!(command_results(temp.path())[0]["applied"], false);
        assert_eq!(std::fs::read_to_string(sibling).unwrap(), "external sibling\n");
    }
}

#[test]
fn source_anchor_drift_blocks_callbacks_even_when_only_a_sibling_is_targeted() {
    for (mode, partial) in [("command-source-drift", false), ("command-inline-drift", true)] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), mode) else { return; };
        let output = run(&tool, &runtime(), command_request()).unwrap();
        assert!(output.is_error);
        assert_eq!(output.details.unwrap()["partial"], partial);
        assert_eq!(command_results(temp.path())[0]["applied"], false);
        assert_eq!(std::fs::read_to_string(temp.path().join("source.lspfixture")).unwrap(), "external\n");
        assert_eq!(std::fs::read_to_string(temp.path().join("sibling.lspfixture")).unwrap(), "old\n");
    }
}

#[test]
fn external_edits_between_accepted_callbacks_are_not_blessed_by_resnapshotting() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "command-between-drift") else { return; };
    let output = run(&tool, &runtime(), command_request()).unwrap();
    assert!(output.is_error);
    let details = output.details.unwrap();
    assert_eq!(details["partial"], true);
    assert_eq!(details["serverEditRequests"], 3);
    let results = command_results(temp.path());
    assert_eq!(results.iter().map(|result| result["applied"].clone()).collect::<Vec<_>>(), vec![json!(true), json!(false), json!(false)]);
    assert_eq!(std::fs::read_to_string(temp.path().join("source.lspfixture")).unwrap(), "external\n");
    assert_eq!(std::fs::read_to_string(temp.path().join("sibling.lspfixture")).unwrap(), "old\n");
    assert_eq!(methods(temp.path()).iter().filter(|method| *method == "workspace/executeCommand").count(), 1);
}

#[test]
fn ordered_moves_carry_evidence_to_followup_command_callbacks() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "command-move") else { return; };
    let output = run(&tool, &runtime(), command_request()).unwrap();
    assert!(!output.is_error);
    assert_eq!(output.details.unwrap()["fileOps"].as_array().unwrap().len(), 1);
    assert!(command_results(temp.path()).iter().all(|result| result["applied"] == true));
    assert!(!temp.path().join("source.lspfixture").exists());
    assert_eq!(std::fs::read_to_string(temp.path().join("moved.lspfixture")).unwrap(), "two\n");
}

#[test]
fn callback_receipts_protect_moved_away_paths_against_external_recreation() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "command-recreated") else { return; };
    let output = run(&tool, &runtime(), command_request()).unwrap();
    assert!(output.is_error);
    assert_eq!(output.details.unwrap()["partial"], true);
    let results = command_results(temp.path());
    assert_eq!(results[0]["applied"], true);
    assert_eq!(results[1]["applied"], false);
    assert_eq!(std::fs::read_to_string(temp.path().join("source.lspfixture")).unwrap(), "external\n");
    assert_eq!(std::fs::read_to_string(temp.path().join("moved.lspfixture")).unwrap(), "old\n");
}

#[test]
fn command_callback_budget_is_bounded_even_for_noop_requests() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "command-budget") else { return; };
    let output = run(&tool, &runtime(), command_request()).unwrap();
    assert!(output.is_error);
    let details = output.details.unwrap();
    assert_eq!(details["serverEditRequests"], 33);
    assert_eq!(details["partial"], false);
    let results = command_results(temp.path());
    assert_eq!(results.len(), 33);
    assert!(results[..32].iter().all(|result| result["applied"] == true));
    assert_eq!(results[32]["applied"], false);
    assert!(results[32]["failureReason"].as_str().unwrap().contains("LSP_EDIT_LIMIT"));
}

fn manual_command_lease(tool: &LspTool, runtime: &asupersync::runtime::Runtime, root: &Path) -> (Arc<ServerEntry>, CommandLease) {
    let source = root.join("source.lspfixture").canonicalize().unwrap();
    let (_, entry) = runtime.block_on(tool.synced(&source)).unwrap();
    let snapshot = refactor::RefactorSnapshot::capture(&entry, &source, file_hash(&source).unwrap()).unwrap();
    let lease = tool.actions.grant(&entry, snapshot.command_edits(AgentCx::for_current_or_request())).unwrap();
    (entry, lease)
}

fn callback_edit(root: &Path) -> Value {
    json!({"edit":{"changes":{crate::lsp::client::try_path_to_uri(&root.join("sibling.lspfixture").canonicalize().unwrap()).unwrap():[{
        "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}},"newText":"new"
    }]}}})
}

#[test]
fn reserved_leases_do_not_authorize_callbacks_until_command_activation() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "normal") else { return; };
    let runtime = runtime();
    let (entry, lease) = manual_command_lease(&tool, &runtime, temp.path());
    let params = callback_edit(temp.path());
    assert_eq!(tool.actions.apply_from_server(&entry, &params)["applied"], false);
    lease.activate(&entry, std::time::Duration::from_secs(5)).unwrap();
    assert_eq!(tool.actions.apply_from_server(&entry, &params)["applied"], true);
    let report = lease.finish();
    assert_eq!(report.requests, 1);
    assert_eq!(tool.actions.admission.load(Ordering::Acquire), 0);
    assert_eq!(tool.actions.apply_from_server(&entry, &params)["applied"], false);
}

#[test]
fn expired_command_windows_never_authorize_file_writes() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "normal") else { return; };
    let runtime = runtime();
    let (entry, lease) = manual_command_lease(&tool, &runtime, temp.path());
    let error = lease.activate(&entry, std::time::Duration::ZERO).unwrap_err();
    assert!(error.to_string().contains("LSP_TIMEOUT"));
    assert_eq!(tool.actions.admission.load(Ordering::Acquire), 0);
    assert_eq!(tool.actions.apply_from_server(&entry, &callback_edit(temp.path()))["applied"], false);
    drop(lease);
    assert!(lock(&tool.actions.active).is_none());
    assert_eq!(std::fs::read_to_string(temp.path().join("sibling.lspfixture")).unwrap(), "old\n");
}

#[test]
fn revoking_permission_during_staging_prevents_the_command_batch_from_committing() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "normal") else { return; };
    let runtime = runtime();
    let source = temp.path().join("source.lspfixture").canonicalize().unwrap();
    let (_, entry) = runtime.block_on(tool.synced(&source)).unwrap();
    let snapshot = refactor::RefactorSnapshot::capture(&entry, &source, file_hash(&source).unwrap()).unwrap();
    let mut edits = snapshot.command_edits(AgentCx::for_current_or_request());
    let calls = std::cell::Cell::new(0);
    let error = edits.apply(&entry, &callback_edit(temp.path())["edit"], || {
        calls.set(calls.get() + 1);
        if calls.get() == 1 { Ok(()) } else { Err(tool_err("LSP_CANCELLED", "lease dropped after staging")) }
    }).unwrap_err();
    assert_eq!(calls.get(), 2);
    assert!(error.to_string().contains("LSP_CANCELLED"));
    assert_eq!(std::fs::read_to_string(temp.path().join("sibling.lspfixture")).unwrap(), "old\n");
}

#[test]
fn server_callbacks_cannot_gain_io_authority_missing_from_the_selected_owner() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "normal") else { return; };
    let runtime = runtime();
    let source = temp.path().join("source.lspfixture").canonicalize().unwrap();
    let (_, entry) = runtime.block_on(tool.synced(&source)).unwrap();
    let snapshot = refactor::RefactorSnapshot::capture(&entry, &source, file_hash(&source).unwrap()).unwrap();
    let owner = AgentCx::for_testing();
    assert!(!owner.capabilities().io);
    let mut edits = snapshot.command_edits(owner);
    let error = edits.apply(&entry, &callback_edit(temp.path())["edit"], || Ok(())).unwrap_err();
    assert!(error.to_string().contains("LSP_EDIT_PERMISSION"));
    assert_eq!(std::fs::read_to_string(temp.path().join("sibling.lspfixture")).unwrap(), "old\n");
}
