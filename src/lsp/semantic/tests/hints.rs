//! Inlay hints through the same public tool, ownership and real stdio path.

use super::*;

fn request(resolve: bool) -> Value {
    json!({"action":"inlay_hints","file":"source.pisig","resolve":resolve,"timeout":5})
}

#[test]
fn inferred_type_and_argument_hints_reach_the_agent_as_read_only_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "hints") else {
        return;
    };
    let payload = runtime()
        .block_on(tool.execute("hints", request(false), None))
        .unwrap()
        .details
        .unwrap();
    assert_eq!(payload["count"], 3);
    assert_eq!(payload["readOnly"], true);
    assert_eq!(
        payload["range"],
        json!({"start":{"line":0,"character":0},"end":{"line":2,"character":0}})
    );
    assert_eq!(payload["hints"][0]["kind"], "parameter");
    assert_eq!(payload["hints"][0]["label"], "value: Text");
    assert_eq!(payload["hints"][1]["kind"], "type");
    assert_eq!(payload["hints"][0]["textEditsPresent"], true);
    assert_eq!(payload["hints"][0]["labelParts"][1]["commandPresent"], true);
    assert!(!payload.to_string().contains("untrusted.command"));
    assert!(!payload.to_string().contains("never insert"));
    assert_eq!(
        std::fs::read_to_string(temp.path().join("source.pisig")).unwrap(),
        TEXT
    );
    let log = events(temp.path());
    assert!(
        !log.iter()
            .any(|event| event["method"] == "inlayHint/resolve"
                || event["method"] == "workspace/executeCommand")
    );
    let init = log
        .iter()
        .find(|event| event["method"] == "initialize")
        .unwrap();
    assert_eq!(
        init.pointer("/params/capabilities/textDocument/inlayHint/resolveSupport/properties"),
        Some(&json!(["tooltip", "label.tooltip", "label.location"]))
    );
}

#[test]
fn exact_selected_ranges_preserve_same_position_hint_order() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "hints") else {
        return;
    };
    let mut input = request(false);
    input["range"] = json!({"start":{"line":1,"character":12},"end":{"line":1,"character":16}});
    let payload = runtime()
        .block_on(tool.execute("range", input.clone(), None))
        .unwrap()
        .details
        .unwrap();
    assert_eq!(payload["count"], 2);
    assert_eq!(payload["hints"][0]["label"], ": bool");
    assert_eq!(payload["hints"][1]["label"], "inferred");
    assert_eq!(
        payload["hints"][0]["position"],
        payload["hints"][1]["position"]
    );
    let log = events(temp.path());
    let call = log
        .iter()
        .find(|event| event["method"] == "textDocument/inlayHint")
        .unwrap();
    assert_eq!(call["params"]["range"], input["range"]);
}

#[test]
fn lazy_details_resolve_using_the_exact_original_opaque_hint() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "hints") else {
        return;
    };
    let mut input = request(true);
    input["limit"] = json!(1);
    let payload = runtime()
        .block_on(tool.execute("resolved", input, None))
        .unwrap()
        .details
        .unwrap();
    assert_eq!(payload["hints"][0]["resolved"], true);
    assert_eq!(payload["hints"][0]["tooltip"]["value"], "resolved detail");
    assert_eq!(
        payload["hints"][0]["labelParts"][0]["tooltip"],
        "resolved part"
    );
    assert_eq!(
        payload["hints"][0]["labelParts"][0]["location"]["uri"],
        "https://invalid.example/type"
    );
    let log = events(temp.path());
    let calls: Vec<_> = log
        .iter()
        .filter(|event| event["method"] == "inlayHint/resolve")
        .collect();
    assert_eq!(calls.len(), 1);
    let params = &calls[0]["params"];
    assert_eq!(params["data"], json!({"token":["opaque",7]}));
    assert!(params.get("index").is_none());
    assert!(params.get("resolved").is_none());
    assert_eq!(params["textEdits"][0]["newText"], "never insert");
    assert_eq!(
        std::fs::read_to_string(temp.path().join("source.pisig")).unwrap(),
        TEXT
    );
}

#[test]
fn only_retained_hints_are_resolved_and_truncation_is_explicit() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "hints-many") else {
        return;
    };
    let mut input = request(true);
    input["limit"] = json!(2);
    let payload = runtime()
        .block_on(tool.execute("many", input, None))
        .unwrap()
        .details
        .unwrap();
    assert_eq!(payload["count"], 2);
    assert_eq!(payload["total"], 160);
    assert_eq!(payload["truncated"], true);
    let log = events(temp.path());
    let calls: Vec<_> = log
        .iter()
        .filter(|event| event["method"] == "inlayHint/resolve")
        .collect();
    assert_eq!(calls.len(), 2);
    for (index, call) in calls.iter().enumerate() {
        assert_eq!(call["params"]["data"]["index"], json!(index));
        assert_eq!(payload["hints"][index]["label"], format!("hint {index}"));
    }
}

#[test]
fn empty_results_and_inline_only_providers_remain_supported() {
    for mode in [
        "hints-null",
        "hints-empty",
        "hints-no-resolve",
        "hints-boolean",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), mode) else {
            return;
        };
        let payload = runtime()
            .block_on(tool.execute("empty", request(false), None))
            .unwrap()
            .details
            .unwrap();
        let empty = matches!(mode, "hints-null" | "hints-empty");
        assert_eq!(payload["count"], if empty { json!(0) } else { json!(3) });
        if !empty {
            assert_eq!(payload["resolveSupported"], false);
        }
    }
}

#[test]
fn requesting_unavailable_resolution_does_not_send_a_hint_or_resolve_request() {
    for mode in ["hints-no-resolve", "hints-boolean", "hints-unsupported"] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), mode) else {
            return;
        };
        let error = runtime()
            .block_on(tool.execute("unsupported", request(true), None))
            .unwrap_err();
        assert!(error.to_string().contains("LSP_UNSUPPORTED"), "{error}");
        assert!(
            !events(temp.path())
                .iter()
                .any(|event| event["method"] == "textDocument/inlayHint"
                    || event["method"] == "inlayHint/resolve")
        );
    }
}

#[test]
fn malformed_omitted_items_and_provider_failures_never_become_partial_success() {
    for (mode, code) in [
        ("hints-error", "hint engine failed"),
        ("hints-malformed", "LSP_SEMANTIC_MALFORMED"),
        ("hints-oversized", "LSP_SEMANTIC_LIMIT"),
        ("hints-outside", "LSP_SEMANTIC_MALFORMED"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), mode) else {
            return;
        };
        let mut input = request(true);
        input["limit"] = json!(1);
        input["range"] = json!({"start":{"line":1,"character":0},"end":{"line":1,"character":17}});
        let error = runtime()
            .block_on(tool.execute("failure", input, None))
            .unwrap_err();
        assert!(error.to_string().contains(code), "{mode}: {error}");
        assert!(
            !events(temp.path())
                .iter()
                .any(|event| event["method"] == "inlayHint/resolve")
        );
    }
}

#[test]
fn resolver_cannot_substitute_hints_or_introduce_unnegotiated_changes() {
    for (mode, code) in [
        ("hints-resolve-change", "LSP_SEMANTIC_MALFORMED"),
        ("hints-resolve-data", "LSP_SEMANTIC_MALFORMED"),
        ("hints-resolve-command", "LSP_SEMANTIC_MALFORMED"),
        ("hints-resolve-oversized", "LSP_SEMANTIC_LIMIT"),
        ("hints-resolve-error", "hint resolution failed"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), mode) else {
            return;
        };
        let error = runtime()
            .block_on(tool.execute("resolve-failure", request(true), None))
            .unwrap_err();
        assert!(error.to_string().contains(code), "{mode}: {error}");
        assert_eq!(
            std::fs::read_to_string(temp.path().join("source.pisig")).unwrap(),
            TEXT
        );
    }
}

#[test]
fn source_drift_during_listing_or_lazy_resolution_retires_the_result() {
    for mode in ["hints-drift", "hints-resolve-drift"] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), mode) else {
            return;
        };
        let error = runtime()
            .block_on(tool.execute("drift", request(true), None))
            .unwrap_err();
        assert!(error.to_string().contains("LSP_SEMANTIC_STALE"), "{error}");
        assert_eq!(
            std::fs::read_to_string(temp.path().join("source.pisig")).unwrap(),
            "external hint edit\n"
        );
    }
}

#[test]
fn neither_hint_listing_nor_resolution_authorizes_workspace_edits() {
    for mode in ["hints-unsolicited", "hints-resolve-unsolicited"] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), mode) else {
            return;
        };
        let mut input = request(true);
        input["limit"] = json!(1);
        runtime()
            .block_on(tool.execute("unsolicited", input, None))
            .unwrap();
        let log = events(temp.path());
        let reply = log
            .iter()
            .find(|event| event["id"] == "unexpected-edit" && event.get("method").is_none())
            .unwrap();
        assert_eq!(reply["result"]["applied"], false);
        assert!(
            !log.iter()
                .any(|event| event["method"] == "workspace/executeCommand")
        );
        assert_eq!(
            std::fs::read_to_string(temp.path().join("source.pisig")).unwrap(),
            TEXT
        );
    }
}

#[test]
fn conflicting_selectors_and_invalid_ranges_fail_before_startup() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "hints") else {
        return;
    };
    let rt = runtime();
    for (key, value) in [
        ("position", json!({"line":1,"character":0})),
        ("apply", json!(false)),
        ("symbol", json!("call")),
        (
            "range",
            json!({"start":{"line":0,"character":4},"end":{"line":1,"character":0}}),
        ),
        ("limit", json!(0)),
    ] {
        let mut input = request(false);
        input[key] = value;
        assert!(rt.block_on(tool.execute("invalid", input, None)).is_err());
    }
    for action in ["signature_help", "completion", "status"] {
        let input = json!({"action":action,"file":"source.pisig","position":{"line":1,"character":5},"resolve":true});
        let error = rt
            .block_on(tool.execute("invalid-action", input, None))
            .unwrap_err();
        assert!(
            error.to_string().contains("resolve is supported only"),
            "{error}"
        );
    }
    assert!(tool.registry.status().is_empty());
}

#[test]
fn one_deadline_covers_hint_listing_and_lazy_resolution() {
    for mode in ["hints-hang", "hints-resolve-hang"] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), mode) else {
            return;
        };
        let mut input = request(true);
        input["timeout"] = json!(1);
        let started = Instant::now();
        let error = runtime()
            .block_on(tool.execute("deadline", input, None))
            .unwrap_err();
        assert!(error.to_string().contains("LSP_TIMEOUT"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}

#[test]
fn aggregate_resolve_output_is_bounded_before_later_requests_are_issued() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "hints-resolve-output") else {
        return;
    };
    let error = runtime()
        .block_on(tool.execute("bounded", request(true), None))
        .unwrap_err();
    assert!(error.to_string().contains("LSP_SEMANTIC_LIMIT"), "{error}");
    let count = events(temp.path())
        .iter()
        .filter(|event| event["method"] == "inlayHint/resolve")
        .count();
    assert!(
        (1..8).contains(&count),
        "must not resolve every item once output is exhausted: {count}"
    );
}

#[test]
fn dropping_a_lazy_hint_request_cancels_the_resolve_and_releases_the_lane() {
    use std::future::Future as _;
    use std::task::Poll;
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "hints-resolve-hang") else {
        return;
    };
    let has_method = |method: &str| {
        std::fs::read_to_string(temp.path().join("semantic-requests.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .any(|event| event["method"] == method)
    };
    runtime().block_on(async {
        let owner = AgentCx::for_current_or_request();
        let mut pending = Box::pin(tool.execute("cancel-hint", request(true), None));
        let started = Instant::now();
        while !has_method("inlayHint/resolve") {
            assert!(started.elapsed() < Duration::from_secs(5));
            let result = std::future::poll_fn(|cx| Poll::Ready(pending.as_mut().poll(cx))).await;
            assert!(result.is_pending(), "{result:?}");
            owner.time().sleep(Duration::from_millis(10)).await;
        }
        drop(pending);
        let started = Instant::now();
        while !has_method("$/cancelRequest") {
            assert!(started.elapsed() < Duration::from_secs(2));
            owner.time().sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !tool
                .execute("after-drop", json!({"action":"status"}), None)
                .await
                .unwrap()
                .is_error
        );
    });
}
