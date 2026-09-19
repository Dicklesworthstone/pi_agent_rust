// Included inside actions::tests; reuse the framed peer and real tool runtime.
const SELECTION_SOURCE: &str = "prefix\nleft + right\nsuffix\n";
const SELECTION_RESULT: &str = "prefix\nextracted()\nsuffix\nfn extracted() { left + right }\n";

fn extract_request() -> Value {
    json!({"action":"code_actions","file":"source.lspfixture",
        "range":{"start":{"line":1,"character":0},"end":{"line":1,"character":12}},
        "only":["refactor.extract"]})
}

fn selection_fixture(root: &Path, mode: &str) -> Option<LspTool> {
    let tool = fixture(root, mode)?;
    std::fs::write(root.join("source.lspfixture"), SELECTION_SOURCE).unwrap();
    Some(tool)
}

fn requests(root: &Path) -> Vec<Value> {
    std::fs::read_to_string(root.join("requests.jsonl")).unwrap().lines()
        .map(|line| serde_json::from_str(line).unwrap()).collect()
}

#[test]
fn explicit_selection_preserves_multiline_crlf_and_utf16_positions() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("source.rs");
    std::fs::write(&path, "😀first\r\nsecond\rlast\n").unwrap();
    for range in [
        json!({"start":{"line":0,"character":2},"end":{"line":1,"character":6}}),
        json!({"start":{"line":2,"character":4},"end":{"line":3,"character":0}}),
        json!({"start":{"line":0,"character":2},"end":{"line":0,"character":2}}),
    ] {
        let input: LspInput = serde_json::from_value(json!({"action":"code_actions","range":range})).unwrap();
        assert_eq!(LspTool::code_action_range(&input, &path).unwrap(), range);
    }
}

#[test]
fn invalid_selection_is_never_clamped_or_expanded_to_a_whole_document() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("source.rs");
    std::fs::write(&path, "😀first\nsecond").unwrap();
    for range in [
        json!({"start":{"line":0,"character":1},"end":{"line":0,"character":2}}),
        json!({"start":{"line":0,"character":0},"end":{"line":0,"character":1}}),
        json!({"start":{"line":0,"character":0},"end":{"line":0,"character":99}}),
        json!({"start":{"line":0,"character":0},"end":{"line":99,"character":0}}),
        json!({"start":{"line":1,"character":0},"end":{"line":0,"character":0}}),
    ] {
        let input: LspInput = serde_json::from_value(json!({"action":"code_actions","range":range})).unwrap();
        assert!(LspTool::code_action_range(&input, &path).is_err(), "{range}");
    }
    for extra in [json!({"symbol":"first"}), json!({"line":1})] {
        let mut raw = extract_request();
        raw.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        let input: LspInput = serde_json::from_value(raw).unwrap();
        assert!(LspTool::code_action_range(&input, &path).is_err());
    }
    let input: LspInput = serde_json::from_value(json!({"action":"code_actions","line":1})).unwrap();
    assert!(LspTool::code_action_range(&input, &path).is_err());
}

#[test]
fn action_kind_filter_matches_complete_hierarchy_segments_only() {
    let only = vec!["refactor.extract".to_string(), "source.organizeImports".to_string()];
    for kind in ["refactor.extract", "refactor.extract.function", "source.organizeImports"] {
        assert!(matches_action_kind(&json!({"kind":kind}), &only));
    }
    for action in [json!({"kind":"refactor.extractMore"}), json!({"kind":"refactor"}),
        json!({"kind":"quickfix"}), json!({"command":"unclassified"}), json!({"kind":17})] {
        assert!(!matches_action_kind(&action, &only), "{action}");
    }
}

#[test]
fn invalid_action_kind_requests_fail_before_starting_a_server() {
    let temp = tempfile::tempdir().unwrap();
    let tool = LspTool::new(temp.path(), None);
    let runtime = runtime();
    for only in [json!([]), json!([""]), json!(["refactor..extract"]), json!(["refactor extract"]),
        json!(["refactor\nextract"]), json!(["x".repeat(129)]), json!(vec!["refactor"; 17])] {
        let error = run(&tool, &runtime, json!({"action":"code_actions","file":"missing.rs","only":only})).unwrap_err();
        assert!(error.to_string().contains("LSP_USAGE"), "{error}");
    }
    let error = run(&tool, &runtime, json!({"action":"hover","only":["refactor"]})).unwrap_err();
    assert!(error.to_string().contains("LSP_USAGE"));
    assert!(tool.registry.status().is_empty());
}

#[test]
fn selected_extraction_lists_then_resolves_applies_and_commands_without_relisting() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = selection_fixture(temp.path(), "selection") else { return; };
    let runtime = runtime();
    let listing = run(&tool, &runtime, extract_request()).unwrap().details.unwrap();
    assert_eq!(listing["count"], 1);
    assert_eq!(listing["filteredOut"], 3);
    assert_eq!(listing["range"], extract_request()["range"]);
    assert_eq!(listing["only"], json!(["refactor.extract"]));
    assert_eq!(listing["actions"][0]["index"], 1);
    assert_eq!(std::fs::read_to_string(temp.path().join("source.lspfixture")).unwrap(), SELECTION_SOURCE);
    assert!(!methods(temp.path()).iter().any(|method| method == "codeAction/resolve"));
    let calls = requests(temp.path());
    let request = calls.iter().find(|call| call["method"] == "textDocument/codeAction").unwrap();
    assert_eq!(request["params"]["range"], extract_request()["range"]);
    assert_eq!(request["params"]["context"]["only"], json!(["refactor.extract"]));
    assert_eq!(request["params"]["context"]["triggerKind"], 1);
    let output = run(&tool, &runtime, json!({"action":"code_actions","apply":true,
        "actionId":listing["actions"][0]["actionId"]})).unwrap();
    assert!(!output.is_error);
    assert_eq!(output.details.unwrap()["executedCommand"], "test.extracted");
    assert_eq!(std::fs::read_to_string(temp.path().join("source.lspfixture")).unwrap(), SELECTION_RESULT);
    let methods = methods(temp.path());
    for method in ["textDocument/codeAction", "codeAction/resolve", "workspace/executeCommand"] {
        assert_eq!(methods.iter().filter(|call| *call == method).count(), 1);
    }
    assert!(lock(&tool.actions.active).is_none());
}

#[test]
fn fresh_extract_selection_indexes_only_requested_kinds() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = selection_fixture(temp.path(), "selection") else { return; };
    let mut input = extract_request();
    input["apply"] = json!(true);
    input["query"] = json!("1");
    assert!(!run(&tool, &runtime(), input).unwrap().is_error);
    assert_eq!(std::fs::read_to_string(temp.path().join("source.lspfixture")).unwrap(), SELECTION_RESULT);
}

#[test]
fn no_matching_kind_cannot_fall_back_to_an_unclassified_command() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = selection_fixture(temp.path(), "selection") else { return; };
    let runtime = runtime();
    let mut input = extract_request();
    input["only"] = json!(["source.organizeImports"]);
    let listing = run(&tool, &runtime, input.clone()).unwrap().details.unwrap();
    assert_eq!(listing["count"], 0);
    assert_eq!(listing["filteredOut"], 4);
    input["apply"] = json!(true);
    input["query"] = json!("1");
    assert!(run(&tool, &runtime, input).is_err());
    assert!(!temp.path().join("command-started").exists());
    assert_eq!(std::fs::read_to_string(temp.path().join("source.lspfixture")).unwrap(), SELECTION_SOURCE);
}

#[test]
fn changing_a_cached_selection_is_rejected_without_consuming_the_handle() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = selection_fixture(temp.path(), "selection") else { return; };
    let runtime = runtime();
    let listing = run(&tool, &runtime, extract_request()).unwrap().details.unwrap();
    let apply = json!({"action":"code_actions","apply":true,"actionId":listing["actions"][0]["actionId"]});
    for extra in [json!({"range":extract_request()["range"]}), json!({"only":["quickfix"]}),
        json!({"symbol":"left"}), json!({"line":2}), json!({"query":"1"})] {
        let mut input = apply.clone();
        input.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        let error = run(&tool, &runtime, input).unwrap_err();
        assert!(error.to_string().contains("LSP_USAGE"));
    }
    assert!(!run(&tool, &runtime, apply).unwrap().is_error);
    assert_eq!(std::fs::read_to_string(temp.path().join("source.lspfixture")).unwrap(), SELECTION_RESULT);
}

#[test]
fn selecting_a_range_never_opens_the_server_edit_window_during_listing() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = selection_fixture(temp.path(), "selection-probe") else { return; };
    assert!(!run(&tool, &runtime(), extract_request()).unwrap().is_error);
    let probe: Value = serde_json::from_str(&std::fs::read_to_string(temp.path().join("selection-probe.json")).unwrap()).unwrap();
    assert_eq!(probe["applied"], false);
    assert_eq!(std::fs::read_to_string(temp.path().join("sibling.lspfixture")).unwrap(), "old\n");
    assert!(lock(&tool.actions.active).is_none());
}

#[test]
fn extraction_rejects_stale_and_unknown_versions_before_writing_or_commanding() {
    for mode in ["selection-stale", "selection-unknown"] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = selection_fixture(temp.path(), mode) else { return; };
        let runtime = runtime();
        let listing = run(&tool, &runtime, extract_request()).unwrap().details.unwrap();
        let error = run(&tool, &runtime, json!({"action":"code_actions","apply":true,
            "actionId":listing["actions"][0]["actionId"]})).unwrap_err();
        assert!(error.to_string().contains("LSP_EDIT_CONFLICT"), "{mode}: {error}");
        assert_eq!(std::fs::read_to_string(temp.path().join("source.lspfixture")).unwrap(), SELECTION_SOURCE);
        assert_eq!(std::fs::read_to_string(temp.path().join("sibling.lspfixture")).unwrap(), "old\n");
        assert!(!temp.path().join("command-started").exists());
    }
}

#[test]
fn cached_extraction_preserves_request_time_evidence_for_a_changed_sibling() {
    for resync in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = selection_fixture(temp.path(), "selection-sibling") else { return; };
        let runtime = runtime();
        let sibling = temp.path().join("sibling.lspfixture").canonicalize().unwrap();
        runtime.block_on(tool.synced(&sibling)).unwrap();
        let listing = run(&tool, &runtime, extract_request()).unwrap().details.unwrap();
        std::fs::write(&sibling, "external sibling\n").unwrap();
        if resync { runtime.block_on(tool.synced(&sibling)).unwrap(); }
        let error = run(&tool, &runtime, json!({"action":"code_actions","apply":true,
            "actionId":listing["actions"][0]["actionId"]})).unwrap_err();
        assert!(error.to_string().contains("LSP_EDIT_CONFLICT"), "{error}");
        assert_eq!(std::fs::read_to_string(temp.path().join("source.lspfixture")).unwrap(), SELECTION_SOURCE);
        assert_eq!(std::fs::read_to_string(&sibling).unwrap(), "external sibling\n");
        assert!(!temp.path().join("command-started").exists());
    }
}

#[test]
fn extraction_applies_versioned_source_and_sibling_changes_before_command() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = selection_fixture(temp.path(), "selection-sibling") else { return; };
    let runtime = runtime();
    let sibling = temp.path().join("sibling.lspfixture").canonicalize().unwrap();
    runtime.block_on(tool.synced(&sibling)).unwrap();
    let mut input = extract_request();
    input["apply"] = json!(true);
    input["query"] = json!("1");
    let output = run(&tool, &runtime, input).unwrap();
    assert!(!output.is_error);
    assert_eq!(output.details.unwrap()["filesChanged"].as_array().unwrap().len(), 2);
    assert_eq!(std::fs::read_to_string(temp.path().join("source.lspfixture")).unwrap(), SELECTION_RESULT);
    assert_eq!(std::fs::read_to_string(sibling).unwrap(), "fixed\n");
}
