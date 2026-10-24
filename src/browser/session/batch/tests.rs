use super::*;
use crate::agent_cx::AgentCx;
use crate::error::Error;
use std::time::{Duration, Instant};

fn batch(calls: Vec<Value>) -> Value {
    let steps: Vec<_> = calls.into_iter().map(|call| json!({"call":call})).collect();
    json!({"action":"batch", "steps":steps})
}

#[test]
fn all_step_envelopes_and_limits_are_checked_before_execution() {
    assert!(
        Plan::parse(&batch(vec![json!({"action":"evaluate","script":"42"})]))
            .unwrap()
            .is_some()
    );
    for action in ["batch", "start", "stop", "status", "release", "typo"] {
        assert!(
            Plan::parse(&batch(vec![
                json!({"action":"close"}),
                json!({"action":action})
            ]))
            .is_err()
        );
    }
    for invalid in [
        json!({"action":"batch","steps":[]}),
        json!({"action":"batch","steps":[{"action":"evaluate"}]}),
        json!({"action":"batch","steps":[{"call":null}]}),
        json!({"action":"batch","steps":[{"call":{"action":"evaluate"},"retry":true}]}),
        json!({"action":"batch","steps":[{"call":{"action":"evaluate","timeout_ms":null}}]}),
        json!({"action":"evaluate","steps":[]}),
        batch(vec![json!({"action":"evaluate","steps":[]})]),
        batch(vec![json!({"action":"evaluate","timeout_ms":0})]),
        batch(vec![json!({"action":"evaluate","timeout_ms":120_001})]),
        batch(vec![json!({"action":"evaluate"}); MAX_STEPS + 1]),
    ] {
        assert!(Plan::parse(&invalid).is_err(), "{invalid}");
    }
    for limit in [
        Value::Null,
        json!(0),
        json!(1023),
        json!(MAX_RESULT_BYTES + 1),
        json!("4096"),
    ] {
        let mut input = batch(vec![json!({"action":"evaluate"})]);
        input["max_result_bytes"] = limit;
        assert!(Plan::parse(&input).is_err());
    }
    let large = batch(vec![
        json!({"action":"evaluate","script":"x".repeat(MAX_INPUT_BYTES)}),
    ]);
    assert!(
        Plan::parse(&large)
            .err()
            .unwrap()
            .to_string()
            .contains("BROWSER_BATCH_INPUT_LIMIT")
    );
}

#[test]
fn step_deadlines_preserve_shorter_timeouts_and_never_refresh_the_outer_budget() {
    let owner = AgentCx::for_request();
    let budget = Budget {
        owner,
        deadline: Instant::now() + Duration::from_secs(60),
    };
    assert_eq!(
        timed_call(json!({"action":"wait_for"}), &budget).unwrap()["timeout_ms"],
        5000
    );
    assert_eq!(
        timed_call(json!({"action":"evaluate"}), &budget).unwrap()["timeout_ms"],
        30_000
    );
    assert_eq!(
        timed_call(json!({"action":"evaluate","timeout_ms":20}), &budget).unwrap()["timeout_ms"],
        20
    );
    let near = Budget {
        owner: budget.owner.clone(),
        deadline: Instant::now() + Duration::from_secs(2),
    };
    let call = timed_call(json!({"action":"evaluate","timeout_ms":120_000}), &near).unwrap();
    assert!(
        call["timeout_ms"]
            .as_u64()
            .is_some_and(|value| (1..=2000).contains(&value))
    );
    let expired = Budget {
        owner: budget.owner.clone(),
        deadline: Instant::now(),
    };
    assert!(timed_call(json!({"action":"evaluate"}), &expired).is_err());
    budget.owner.set_cancel_requested(true);
    assert!(timed_call(json!({"action":"evaluate"}), &budget).is_err());
}

#[test]
fn result_accounting_counts_json_escapes_and_preserves_exact_native_content() {
    let native = ToolOutput {
        content: vec![text("\"雪\"\n\0")],
        details: Some(json!({"result":{"ok":true}})),
        is_error: false,
    };
    let size = serde_json::to_vec(&native).unwrap().len();
    assert_eq!(encoded_size(&native, size), Some(size));
    assert_eq!(encoded_size(&native, size - 1), None);
    let mut report = Report::new(size);
    assert!(report.record(0, "evaluate", Ok(native.clone()), None));
    assert!(!report.record(1, "evaluate", Ok(native.clone()), None));
    let output = report.finish(3);
    assert!(output.is_error);
    let details = output.details.unwrap();
    assert_eq!(details["completed_steps"], 1);
    assert_eq!(details["remaining_steps"], 1);
    assert_eq!(details["steps"][0]["content_range"], json!([2, 3]));
    assert_eq!(details["steps"][0]["details"], native.details.unwrap());
    assert_eq!(
        serde_json::to_value(&output.content[2]).unwrap(),
        serde_json::to_value(&native.content[0]).unwrap()
    );
    assert_eq!(details["steps"][1]["result_omitted"], true);
    assert_eq!(details["stop_reason"], "result_limit");
}

#[test]
fn structured_tool_errors_are_not_promoted_to_success_or_discarded() {
    for failure in [
        Err(Error::tool("browser", "ordinary failure")),
        Ok(ToolOutput {
            content: vec![text("failed action")],
            details: Some(json!({"diagnostics":{"events":[]}})),
            is_error: true,
        }),
    ] {
        let mut report = Report::new(4096);
        assert!(!report.record(0, "evaluate", failure, None));
        let output = report.finish(2);
        assert!(output.is_error);
        let details = output.details.unwrap();
        assert_eq!(details["completed_steps"], 0);
        assert_eq!(details["attempted_steps"], 1);
        assert_eq!(details["failed_step"], 0);
        assert_eq!(details["remaining_steps"], 1);
        assert_eq!(details["stop_reason"], "tool_error");
    }
}

#[test]
fn expectations_reject_ambiguous_or_malformed_inputs_without_silently_disabling_them() {
    for invalid in [
        Value::Null,
        json!(false),
        json!({"pointer":"/result"}),
        json!({"pointer":"/result","equals":true,"exists":true}),
        json!({"pointer":"/result","exists":"true"}),
        json!({"pointer":"return true","equals":true}),
        json!({"pointer":"#/result","equals":true}),
        json!({"pointer":"/bad~2escape","equals":true}),
        json!({"pointer":"/bad~","equals":true}),
        json!({"pointer":"/control\n","equals":true}),
        json!({"pointer":"/a".repeat(33),"equals":true}),
        json!({"pointer":format!("/{}", "a".repeat(1024)),"equals":true}),
        json!({"pointer":"/result","equals":"secret".repeat(MAX_EXPECT_BYTES)}),
        json!({"pointer":"/result","equals":true,"unknown":0}),
    ] {
        assert!(Expectation::parse(&invalid).is_err());
        let input = json!({"action":"batch","steps":[
            {"call":{"action":"open","url":"about:blank"}},
            {"call":{"action":"evaluate","script":"42"},"expect":invalid}
        ]});
        assert!(Plan::parse(&input).is_err());
    }
    let misplaced = batch(vec![
        json!({"action":"evaluate","expect":{"pointer":"/result","equals":true}}),
    ]);
    assert!(Plan::parse(&misplaced).is_err());
    assert!(
        Plan::parse(&json!({"action":"evaluate","expect":{"pointer":"/result","equals":true}}))
            .is_err()
    );
    let input = json!({"action":"batch","steps":[
        {"call":{"action":"evaluate","script":"null"},"expect":{"pointer":"/result","equals":null}}
    ]});
    assert!(
        Plan::parse(&input).unwrap().unwrap().steps[0]
            .expect
            .is_some()
    );
}

#[test]
fn expectation_matching_preserves_json_types_pointer_escapes_and_missing_vs_null() {
    let details = json!({"result":{"a/b":{"~key":[false,null,"雪"]},"":7}});
    for (condition, matched) in [
        (
            json!({"pointer":"/result/a~1b/~0key/0","equals":false}),
            true,
        ),
        (json!({"pointer":"/result/a~1b/~0key/0","equals":0}), false),
        (
            json!({"pointer":"/result/a~1b/~0key/1","equals":null}),
            true,
        ),
        (
            json!({"pointer":"/result/a~1b/~0key/2","equals":"雪"}),
            true,
        ),
        (
            json!({"pointer":"/result/a~1b/~0key/1","exists":true}),
            true,
        ),
        (
            json!({"pointer":"/result/a~1b/~0key/1","exists":false}),
            false,
        ),
        (
            json!({"pointer":"/result/a~1b/~0key/9","exists":false}),
            true,
        ),
        (
            json!({"pointer":"/result/a~1b/~0key/9","equals":null}),
            false,
        ),
        (json!({"pointer":"/result/","equals":7}), true),
        (
            json!({"pointer":"/result/a~1b/~0key/00","equals":false}),
            false,
        ),
        (json!({"pointer":"","equals":details}), true),
    ] {
        assert_eq!(
            Expectation::parse(&condition)
                .unwrap()
                .matches(Some(&details)),
            matched
        );
    }
    let null = Expectation::parse(&json!({"pointer":"","equals":null})).unwrap();
    assert!(null.matches(Some(&Value::Null)));
    assert!(!null.matches(None));
}

#[test]
fn expectation_failure_preserves_native_results_but_never_promotes_a_tool_error() {
    let required =
        Expectation::parse(&json!({"pointer":"/result","equals":"EXPECTED-PRIVATE-CANARY"}))
            .unwrap();
    let result = ToolOutput {
        content: vec![text("observed")],
        details: Some(json!({"result":"different"})),
        is_error: false,
    };
    let mut report = Report::new(4096);
    assert!(!report.record(0, "evaluate", Ok(result), Some(&required)));
    let output = report.finish(2);
    assert!(output.is_error);
    assert!(
        !serde_json::to_string(&output)
            .unwrap()
            .contains("EXPECTED-PRIVATE-CANARY")
    );
    let details = output.details.unwrap();
    assert_eq!(details["stop_reason"], "expectation_failed");
    assert_eq!(details["completed_steps"], 0);
    assert_eq!(details["steps"][0]["tool_is_error"], false);
    assert_eq!(details["steps"][0]["expectation"], "failed");
    assert_eq!(details["steps"][0]["details"]["result"], "different");

    let absent = Expectation::parse(&json!({"pointer":"/result","exists":false})).unwrap();
    let mut report = Report::new(4096);
    assert!(!report.record(
        0,
        "evaluate",
        Err(Error::tool("browser", "failed")),
        Some(&absent)
    ));
    let output = report.finish(2);
    assert!(output.is_error);
    let details = output.details.unwrap();
    assert_eq!(details["stop_reason"], "tool_error");
    assert_eq!(details["steps"][0]["expectation"], "not_checked");

    let mut report = Report::new(1);
    assert!(!report.record(
        0,
        "evaluate",
        Ok(ToolOutput {
            content: vec![text("large")],
            details: Some(json!({"result":true})),
            is_error: false,
        }),
        Some(&absent)
    ));
    assert_eq!(
        report.finish(2).details.unwrap()["steps"][0]["expectation"],
        "not_checked"
    );
}

fn binding_log(case: &str) {
    eprintln!(
        "{}",
        json!({"suite":"browser_batch_bindings", "case":case, "phase":"start"})
    );
}

fn bound_step(call: Value, bind: Value) -> Value {
    json!({"action":"batch", "steps":[
        {"call":{"action":"snapshot","tab":"app"}}, {"call":call,"bind":bind}
    ]})
}

fn bindings(field: &str, pointer: &str) -> BTreeMap<String, Binding> {
    BTreeMap::from([(
        field.to_owned(),
        Binding {
            step: 0,
            pointer: pointer.to_owned(),
        },
    )])
}

fn source_report(details: Option<Value>) -> Report {
    let mut report = Report::new(MAX_RESULT_BYTES);
    assert!(report.record(
        0,
        "evaluate",
        Ok(ToolOutput {
            content: vec![text("source-result")],
            details,
            is_error: false,
        }),
        None
    ));
    report
}

#[test]
fn binding_shapes_dependencies_and_literal_authority_are_validated_upfront() {
    binding_log("admission");
    let call = json!({"action":"click","tab":"app"});
    let valid = json!({"selector":{"step":0,"pointer":"/snapshot/elements/0/ref_id"}});
    assert!(Plan::parse(&bound_step(call.clone(), valid.clone())).is_ok());
    for invalid in [
        Value::Null,
        json!([]),
        json!({"selector":null}),
        json!({"selector":{"step":0}}),
        json!({"selector":{"step":0,"pointer":"/x","fallback":"#a"}}),
        json!({"selector":{"step":1,"pointer":"/x"}}),
        json!({"selector":{"step":2,"pointer":"/x"}}),
        json!({"selector":{"step":-1,"pointer":"/x"}}),
        json!({"selector":{"step":0.5,"pointer":"/x"}}),
        json!({"selector":{"step":0,"pointer":"#/x"}}),
        json!({"selector":{"step":0,"pointer":"/x~2"}}),
    ] {
        assert!(Plan::parse(&bound_step(call.clone(), invalid)).is_err());
    }
    for field in [
        "action",
        "tab",
        "url",
        "script",
        "output_path",
        "state_path",
        "files",
        "timeout_ms",
        "wait_after",
        "dialog_response",
        "expect",
    ] {
        let invalid = json!({field:{"step":0,"pointer":"/PRIVATE-CANARY"}});
        let failure = Plan::parse(&bound_step(call.clone(), invalid))
            .err()
            .unwrap()
            .to_string();
        assert!(!failure.contains("PRIVATE-CANARY"));
    }
    for call in [
        json!({"action":"click"}),
        json!({"action":"click","tab":""}),
        json!({"action":"click","tab":"app","selector":"#literal"}),
        json!({"action":"click","tab":"app","selector":null}),
    ] {
        assert!(Plan::parse(&bound_step(call, valid.clone())).is_err());
    }
    assert!(Plan::parse(&json!({"action":"click","bind":valid})).is_err());
    assert!(Plan::parse(&batch(vec![json!({"action":"click","bind":valid})])).is_err());
    assert!(Plan::parse(&json!({"action":"batch","steps":[{"call":call,"bind":valid}]})).is_err());
}

#[test]
fn binding_fields_must_be_consumed_by_the_selected_native_action() {
    binding_log("field-routing");
    for (action, field) in [
        ("click", "selector"),
        ("hover", "selector"),
        ("drag", "target_selector"),
        ("fill", "text"),
        ("wait_for", "text"),
        ("press", "key"),
        ("set_checked", "checked"),
        ("select_option", "options"),
        ("snapshot", "frame"),
        ("upload", "selector"),
    ] {
        let input = bound_step(
            json!({"action":action,"tab":"app"}),
            json!({field:{"step":0,"pointer":"/x"}}),
        );
        assert!(Plan::parse(&input).is_ok(), "{action} {field}");
    }
    for (action, field) in [
        ("evaluate", "selector"),
        ("evaluate", "text"),
        ("hover", "frame"),
        ("drag", "frame"),
        ("screenshot", "selector"),
        ("scroll", "selector"),
        ("fill", "key"),
    ] {
        let input = bound_step(
            json!({"action":action,"tab":"app"}),
            json!({field:{"step":0,"pointer":"/x"}}),
        );
        assert!(Plan::parse(&input).is_err(), "{action} {field}");
    }
    assert!(
        Plan::parse(&bound_step(
            json!({"action":"scroll","tab":"app"}),
            json!({
                "selector":{"step":0,"pointer":"/selector"}, "frame":{"step":0,"pointer":"/frame"}
            })
        ))
        .is_ok()
    );
}

#[test]
fn bindings_preserve_literal_bytes_json_types_and_escaped_pointer_keys() {
    binding_log("typed-projection");
    let literal = "${not_evaluated} \"quoted\" 雪\n";
    let original = json!({"a/b":{"~key":[false,null,literal]},"options":[{"value":" x "}]});
    let report = source_report(Some(original.clone()));
    let call = report
        .bind_call(
            json!({"action":"fill","tab":"app","selector":"#a"}),
            &bindings("text", "/a~1b/~0key/2"),
        )
        .unwrap();
    assert_eq!(call["text"], literal);
    assert_eq!(report.steps[0]["details"], original);
    let checked = report
        .bind_call(
            json!({"action":"set_checked","tab":"app","selector":"#c"}),
            &bindings("checked", "/a~1b/~0key/0"),
        )
        .unwrap();
    assert_eq!(checked["checked"], false);
    assert!(checked["checked"].as_str().is_none());
    let selected = report
        .bind_call(
            json!({"action":"select_option","tab":"app","selector":"#s"}),
            &bindings("options", "/options"),
        )
        .unwrap();
    assert_eq!(selected["options"], json!([{"value":" x "}]));
    assert!(
        report
            .bind_call(
                json!({"action":"fill"}),
                &bindings("text", "/a~1b/~0key/02")
            )
            .is_err()
    );
}

#[test]
fn missing_native_details_are_not_conflated_with_explicit_json_null() {
    binding_log("absence-vs-null");
    let null = source_report(Some(Value::Null));
    assert_eq!(
        null.bind_call(json!({"action":"fill"}), &bindings("text", ""))
            .unwrap()["text"],
        Value::Null
    );
    let absent = source_report(None);
    assert!(
        absent
            .bind_call(json!({"action":"fill"}), &bindings("text", ""))
            .is_err()
    );
    let object = source_report(Some(json!({"value":null})));
    assert_eq!(
        object
            .bind_call(json!({"action":"fill"}), &bindings("text", "/value"))
            .unwrap()["text"],
        Value::Null
    );
    assert!(
        object
            .bind_call(json!({"action":"fill"}), &bindings("text", "/missing"))
            .is_err()
    );
    assert!(
        object
            .bind_call(
                json!({"action":"fill","text":"fallback"}),
                &bindings("text", "/value")
            )
            .is_err()
    );
}

#[test]
fn binding_copy_limits_count_escapes_and_the_combined_resolved_call() {
    binding_log("copy-budgets");
    let report = source_report(Some(json!({"value":"\"".repeat(MAX_BOUND_BYTES / 2)})));
    assert!(
        report
            .bind_call(json!({"action":"fill"}), &bindings("text", "/value"))
            .is_err()
    );
    let report = source_report(Some(json!({"value":"x".repeat(MAX_BOUND_BYTES / 2)})));
    let both = BTreeMap::from([
        (
            "selector".to_string(),
            Binding {
                step: 0,
                pointer: "/value".into(),
            },
        ),
        (
            "target_selector".to_string(),
            Binding {
                step: 0,
                pointer: "/value".into(),
            },
        ),
    ]);
    assert!(report.bind_call(json!({"action":"drag"}), &both).is_err());
    let report = source_report(Some(json!({"value":"x".repeat(MAX_BOUND_BYTES - 2)})));
    assert!(
        report
            .bind_call(json!({"action":"fill"}), &bindings("text", "/value"))
            .is_ok()
    );
    let large_literal =
        json!({"action":"fill","tab":"app","selector":"x".repeat(MAX_INPUT_BYTES - 100)});
    assert!(
        report
            .bind_call(large_literal, &bindings("text", "/value"))
            .is_err()
    );
}

#[test]
fn rejected_or_omitted_results_never_become_binding_sources() {
    binding_log("accepted-results-only");
    let native = ToolOutput {
        content: vec![text("source")],
        details: Some(json!({"value":"#a"})),
        is_error: false,
    };
    let mut report = Report::new(1);
    assert!(!report.record(0, "evaluate", Ok(native.clone()), None));
    assert!(
        report
            .bind_call(json!({"action":"click"}), &bindings("selector", "/value"))
            .is_err()
    );
    let mut report = Report::new(4096);
    let reject = Expectation::parse(&json!({"pointer":"/value","equals":"#other"})).unwrap();
    assert!(!report.record(0, "evaluate", Ok(native.clone()), Some(&reject)));
    assert!(
        report
            .bind_call(json!({"action":"click"}), &bindings("selector", "/value"))
            .is_err()
    );
    let mut report = Report::new(4096);
    assert!(!report.record(
        0,
        "evaluate",
        Ok(ToolOutput {
            is_error: true,
            ..native
        }),
        None
    ));
    assert!(
        report
            .bind_call(json!({"action":"click"}), &bindings("selector", "/value"))
            .is_err()
    );
}

#[test]
fn binding_failure_retains_earlier_evidence_and_marks_the_call_not_started() {
    binding_log("partial-report");
    let mut report = source_report(Some(json!({"value":"#a"})));
    let failure = report
        .bind_call(
            json!({"action":"click"}),
            &bindings("selector", "/PRIVATE-CANARY"),
        )
        .unwrap_err();
    report.binding_failure(1, "click", failure);
    let result = report.finish(3);
    assert!(result.is_error);
    assert!(
        !serde_json::to_string(&result)
            .unwrap()
            .contains("PRIVATE-CANARY")
    );
    let details = result.details.unwrap();
    assert_eq!(details["stop_reason"], "binding_error");
    assert_eq!(details["completed_steps"], 1);
    assert_eq!(details["attempted_steps"], 2);
    assert_eq!(details["remaining_steps"], 1);
    assert_eq!(details["failed_step"], 1);
    assert_eq!(details["steps"][1]["call_started"], false);
    assert_eq!(details["steps"][0]["details"]["value"], "#a");
    assert_eq!(
        serde_json::to_value(&result.content[2]).unwrap()["text"],
        "source-result"
    );
}

#[test]
fn binding_resolution_does_not_change_the_original_call_timeout_or_cancellation() {
    binding_log("same-budget");
    let report = source_report(Some(json!({"value":"#a"})));
    let call = report
        .bind_call(
            json!({"action":"click","tab":"app","timeout_ms":120000}),
            &bindings("selector", "/value"),
        )
        .unwrap();
    assert_eq!(call["timeout_ms"], 120000);
    let owner = AgentCx::for_request();
    let budget = Budget {
        owner,
        deadline: Instant::now() + Duration::from_secs(2),
    };
    let timed = timed_call(call, &budget).unwrap();
    assert!(
        timed["timeout_ms"]
            .as_u64()
            .is_some_and(|value| (1..=2000).contains(&value))
    );
    budget.owner.set_cancel_requested(true);
    assert!(timed_call(json!({"action":"click"}), &budget).is_err());
}
