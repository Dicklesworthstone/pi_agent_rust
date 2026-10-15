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
    assert!(Plan::parse(&batch(vec![json!({"action":"evaluate","script":"42"})])).unwrap().is_some());
    for action in ["batch", "start", "stop", "status", "release", "typo"] {
        assert!(Plan::parse(&batch(vec![json!({"action":"close"}), json!({"action":action})])).is_err());
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
    for limit in [Value::Null, json!(0), json!(1023), json!(MAX_RESULT_BYTES + 1), json!("4096")] {
        let mut input = batch(vec![json!({"action":"evaluate"})]);
        input["max_result_bytes"] = limit;
        assert!(Plan::parse(&input).is_err());
    }
    let large = batch(vec![json!({"action":"evaluate","script":"x".repeat(MAX_INPUT_BYTES)})]);
    assert!(Plan::parse(&large).err().unwrap().to_string().contains("BROWSER_BATCH_INPUT_LIMIT"));
}

#[test]
fn step_deadlines_preserve_shorter_timeouts_and_never_refresh_the_outer_budget() {
    let owner = AgentCx::for_request();
    let budget = Budget { owner, deadline: Instant::now() + Duration::from_secs(60) };
    assert_eq!(timed_call(json!({"action":"wait_for"}), &budget).unwrap()["timeout_ms"], 5000);
    assert_eq!(timed_call(json!({"action":"evaluate"}), &budget).unwrap()["timeout_ms"], 30_000);
    assert_eq!(timed_call(json!({"action":"evaluate","timeout_ms":20}), &budget).unwrap()["timeout_ms"], 20);
    let near = Budget { owner: budget.owner.clone(), deadline: Instant::now() + Duration::from_secs(2) };
    let call = timed_call(json!({"action":"evaluate","timeout_ms":120_000}), &near).unwrap();
    assert!(call["timeout_ms"].as_u64().is_some_and(|value| (1..=2000).contains(&value)));
    let expired = Budget { owner: budget.owner.clone(), deadline: Instant::now() };
    assert!(timed_call(json!({"action":"evaluate"}), &expired).is_err());
    budget.owner.set_cancel_requested(true);
    assert!(timed_call(json!({"action":"evaluate"}), &budget).is_err());
}

#[test]
fn result_accounting_counts_json_escapes_and_preserves_exact_native_content() {
    let native = ToolOutput {
        content: vec![text("\"雪\"\n\0")], details: Some(json!({"result":{"ok":true}})), is_error: false,
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
    assert_eq!(details["steps"][0]["content_range"], json!([2,3]));
    assert_eq!(details["steps"][0]["details"], native.details.unwrap());
    assert_eq!(serde_json::to_value(&output.content[2]).unwrap(), serde_json::to_value(&native.content[0]).unwrap());
    assert_eq!(details["steps"][1]["result_omitted"], true);
    assert_eq!(details["stop_reason"], "result_limit");
}

#[test]
fn structured_tool_errors_are_not_promoted_to_success_or_discarded() {
    for failure in [
        Err(Error::tool("browser", "ordinary failure")),
        Ok(ToolOutput { content: vec![text("failed action")], details: Some(json!({"diagnostics":{"events":[]}})), is_error:true }),
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
    let misplaced = batch(vec![json!({"action":"evaluate","expect":{"pointer":"/result","equals":true}})]);
    assert!(Plan::parse(&misplaced).is_err());
    assert!(Plan::parse(&json!({"action":"evaluate","expect":{"pointer":"/result","equals":true}})).is_err());
    let input = json!({"action":"batch","steps":[
        {"call":{"action":"evaluate","script":"null"},"expect":{"pointer":"/result","equals":null}}
    ]});
    assert!(Plan::parse(&input).unwrap().unwrap().steps[0].expect.is_some());
}

#[test]
fn expectation_matching_preserves_json_types_pointer_escapes_and_missing_vs_null() {
    let details = json!({"result":{"a/b":{"~key":[false,null,"雪"]},"":7}});
    for (condition, matched) in [
        (json!({"pointer":"/result/a~1b/~0key/0","equals":false}), true),
        (json!({"pointer":"/result/a~1b/~0key/0","equals":0}), false),
        (json!({"pointer":"/result/a~1b/~0key/1","equals":null}), true),
        (json!({"pointer":"/result/a~1b/~0key/2","equals":"雪"}), true),
        (json!({"pointer":"/result/a~1b/~0key/1","exists":true}), true),
        (json!({"pointer":"/result/a~1b/~0key/1","exists":false}), false),
        (json!({"pointer":"/result/a~1b/~0key/9","exists":false}), true),
        (json!({"pointer":"/result/a~1b/~0key/9","equals":null}), false),
        (json!({"pointer":"/result/","equals":7}), true),
        (json!({"pointer":"/result/a~1b/~0key/00","equals":false}), false),
        (json!({"pointer":"","equals":details}), true),
    ] {
        assert_eq!(Expectation::parse(&condition).unwrap().matches(Some(&details)), matched);
    }
    let null = Expectation::parse(&json!({"pointer":"","equals":null})).unwrap();
    assert!(null.matches(Some(&Value::Null)));
    assert!(!null.matches(None));
}

#[test]
fn expectation_failure_preserves_native_results_but_never_promotes_a_tool_error() {
    let required = Expectation::parse(&json!({"pointer":"/result","equals":"EXPECTED-PRIVATE-CANARY"})).unwrap();
    let result = ToolOutput {
        content: vec![text("observed")], details: Some(json!({"result":"different"})), is_error:false,
    };
    let mut report = Report::new(4096);
    assert!(!report.record(0, "evaluate", Ok(result), Some(&required)));
    let output = report.finish(2);
    assert!(output.is_error);
    assert!(!serde_json::to_string(&output).unwrap().contains("EXPECTED-PRIVATE-CANARY"));
    let details = output.details.unwrap();
    assert_eq!(details["stop_reason"], "expectation_failed");
    assert_eq!(details["completed_steps"], 0);
    assert_eq!(details["steps"][0]["tool_is_error"], false);
    assert_eq!(details["steps"][0]["expectation"], "failed");
    assert_eq!(details["steps"][0]["details"]["result"], "different");

    let absent = Expectation::parse(&json!({"pointer":"/result","exists":false})).unwrap();
    let mut report = Report::new(4096);
    assert!(!report.record(0, "evaluate", Err(Error::tool("browser", "failed")), Some(&absent)));
    let output = report.finish(2);
    assert!(output.is_error);
    let details = output.details.unwrap();
    assert_eq!(details["stop_reason"], "tool_error");
    assert_eq!(details["steps"][0]["expectation"], "not_checked");

    let mut report = Report::new(1);
    assert!(!report.record(0, "evaluate", Ok(ToolOutput {
        content:vec![text("large")], details:Some(json!({"result":true})), is_error:false,
    }), Some(&absent)));
    assert_eq!(report.finish(2).details.unwrap()["steps"][0]["expectation"], "not_checked");
}
