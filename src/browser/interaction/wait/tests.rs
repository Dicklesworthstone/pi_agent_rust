use super::*;

#[allow(clippy::needless_pass_by_value)] // callers pass json! temporaries
fn log(case: &str, details: Value) {
    eprintln!(
        "{}",
        json!({"suite":"browser-wait","case":case,"details":details})
    );
}

#[test]
fn conditions_are_typed_and_do_not_echo_invalid_values() {
    let valid = json!({"action":"wait_for","selector":"#ready","text":"  Ready\u{0085} now  ","text_match":"exact","stable_ms":10});
    let condition = Condition::parse(&valid).unwrap();
    assert_eq!(condition.text.as_deref(), Some("Ready now"));
    assert!(condition.exact);
    for (key, value) in [
        ("state", json!("PRIVATE-CANARY")),
        ("state", Value::Null),
        ("stable_ms", json!(-1)),
        ("stable_ms", json!(0.5)),
        ("stable_ms", json!(10001)),
        ("text", json!(" \n ")),
        ("text", json!("x".repeat(MAX_TEXT_BYTES + 1))),
        ("text_match", json!("regex")),
    ] {
        let mut invalid = valid.clone();
        invalid[key] = value;
        let message = Condition::parse(&invalid).err().unwrap().to_string();
        assert!(!message.contains("PRIVATE-CANARY"));
    }
    assert!(Condition::parse(&json!({"selector":"#ready","text_match":"exact"})).is_err());
    log(
        "typed-conditions",
        json!({"normalized":true,"invalid_rejected":9}),
    );
}

#[test]
fn absence_cannot_be_proved_by_visibility_filtered_semantics() {
    for state in ["hidden", "attached", "detached"] {
        assert!(Condition::parse(&json!({"selector":"text/Ready","state":state})).is_err());
        assert!(Condition::parse(&json!({"selector":"#ready","state":state})).is_ok());
        assert!(Condition::parse(&json!({"selector":"@e1","state":state})).is_ok());
    }
    assert!(Condition::parse(&json!({"selector":"text/Ready"})).is_ok());
    for state in ["hidden", "detached"] {
        assert!(
            Condition::parse(&json!({"selector":"#ready","state":state,"text":"Ready"})).is_err()
        );
    }
    log(
        "absence-semantics",
        json!({"semantic_absence_refused":true}),
    );
}

#[test]
fn wait_options_cannot_be_ignored_on_other_actions() {
    for field in ["state", "stable_ms", "text_match"] {
        let mut args = json!({"action":"click","selector":"#b"});
        args[field] = json!("PRIVATE-CANARY");
        assert!(validate_input(&args).is_err());
    }
    for field in ["script", "url", "files", "button", "dialog_response"] {
        let mut args = json!({"action":"wait_for","selector":"#b"});
        args[field] = json!("PRIVATE-CANARY");
        assert!(validate_input(&args).is_err());
    }
    assert!(!validate_input(&json!({"action":"fill","selector":"#i","text":"literal"})).unwrap());
    assert!(validate_input(&json!({"action":"wait_for","selector":"#b","text":"Ready"})).unwrap());
    log("foreign-options", json!({"ignored_options_rejected":8}));
}

#[test]
fn hidden_and_detached_are_distinct_observations() {
    let hidden = Observation {
        attached: true,
        visible: false,
        text_matches: None,
    };
    let visible = Observation {
        attached: true,
        visible: true,
        text_matches: None,
    };
    for (state, wanted) in [
        ("visible", false),
        ("hidden", true),
        ("attached", true),
        ("detached", false),
    ] {
        let condition = Condition::parse(&json!({"selector":"#r","state":state})).unwrap();
        assert_eq!(condition.matches(&hidden), wanted);
    }
    let visible_condition = Condition::parse(&json!({"selector":"#r"})).unwrap();
    assert!(visible_condition.matches(&visible));
    assert!(!visible_condition.matches(&Observation::absent(false)));
    log(
        "state-distinction",
        json!({"attached_hidden_is_not_detached":true}),
    );
}

#[test]
fn stability_restarts_on_mismatch_replacement_and_clock_rewind() {
    let ms = Duration::from_millis;
    let mut stable = Stability::default();
    assert!(!stable.sample(Some(1), true, ms(0), ms(200)));
    assert!(!stable.sample(Some(1), true, ms(100), ms(200)));
    assert!(!stable.sample(Some(2), true, ms(200), ms(200)));
    assert!(!stable.sample(Some(2), false, ms(300), ms(200)));
    assert!(!stable.sample(Some(2), true, ms(400), ms(200)));
    assert!(!stable.sample(Some(2), true, ms(300), ms(200)));
    assert!(!stable.sample(Some(2), true, ms(499), ms(200)));
    assert!(stable.sample(Some(2), true, ms(500), ms(200)));
    let mut absent = Stability::default();
    assert!(!absent.sample(None, true, ms(0), ms(200)));
    assert!(absent.sample(None, true, ms(200), ms(200)));
    log(
        "sampled-stability",
        json!({"same_node_required":true,"absence_supported":true}),
    );
}

#[test]
fn malformed_observations_do_not_become_ready() {
    for value in [
        json!({"attached":false,"visible":true,"textMatches":null}),
        json!({"attached":false,"visible":false,"textMatches":true}),
        json!({"attached":true,"visible":"yes","textMatches":true}),
        json!({"attached":true,"visible":true,"textMatches":null}),
        json!({"attached":true,"visible":true,"textMatches":true,"extra":"private"}),
    ] {
        assert!(Observation::parse(value, true).is_err());
    }
    assert!(
        Observation::parse(
            json!({"attached":true,"visible":true,"textMatches":true}),
            true
        )
        .is_ok()
    );
    let response =
        json!({"exceptionDetails":{"exception":{"description":"Error: PRIVATE-CANARY"}}});
    assert!(
        !probe_value(&response)
            .unwrap_err()
            .to_string()
            .contains("PRIVATE-CANARY")
    );
    assert!(probe_value(&json!({"result":{}})).is_err());
    log(
        "protocol-refusal",
        json!({"malformed_rejected":true,"exception_values_redacted":true}),
    );
}

#[test]
fn postconditions_inherit_scope_and_cannot_replace_the_trigger() {
    for action in ["click", "type", "fill", "press"] {
        let args = json!({"action":action,"selector":"#trigger","tab":"original","timeout_ms":500,
            "wait_after":{"selector":"#result","text":"Done","stable_ms":10}});
        let condition = after_condition(&args).unwrap().unwrap();
        assert_eq!(condition["action"], "wait_for");
        assert_eq!(condition["selector"], "#result");
        assert!(condition.get("tab").is_none());
        assert!(condition.get("timeout_ms").is_none());
        assert!(validate_input(&args).unwrap());
    }
    for action in ["evaluate", "open", "upload", "set_checked", "wait_for"] {
        assert!(
            validate_input(&json!({"action":action,"selector":"#trigger",
            "wait_after":{"selector":"#ready"}}))
            .is_err()
        );
    }
    for field in [
        "tab",
        "frame",
        "timeout_ms",
        "action",
        "script",
        "diagnostics",
    ] {
        let mut args =
            json!({"action":"click","selector":"#trigger","wait_after":{"selector":"#result"}});
        args["wait_after"][field] = json!("PRIVATE-CANARY");
        assert!(after_condition(&args).is_err());
    }
    for value in [
        Value::Null,
        json!(true),
        json!({}),
        json!({"selector":"#r","stable_ms":10001}),
    ] {
        assert!(after_condition(&json!({"action":"click","wait_after":value})).is_err());
    }
    assert!(
        after_condition(
            &json!({"action":"click","wait_after":{"selector":"#ready"},"dialog_response":{}})
        )
        .is_err()
    );
    log(
        "postcondition-admission",
        json!({"inherited_scope":true,"foreign_actions_rejected":true}),
    );
}

#[test]
fn postcondition_success_preserves_action_facts_without_claiming_causality() {
    let action = output(
        "Input acknowledged",
        json!({"dispatch_acknowledged":true,"delivery_verified":false}),
    );
    let wait = output("Wait ready", json!({"matched":true,"samples":3}));
    let result = after_result(action, Ok(wait));
    assert!(!result.is_error);
    assert_eq!(result.content.len(), 2);
    let details = result.details.unwrap();
    assert_eq!(details["dispatch_acknowledged"], true);
    assert_eq!(details["delivery_verified"], false);
    assert_eq!(details["wait_after"]["satisfied"], true);
    assert_eq!(details["wait_after"]["causality_verified"], false);
    assert_eq!(details["wait_after"]["action_replayed"], false);
    log(
        "postcondition-success",
        json!({"action_facts_retained":true,"causality_claimed":false}),
    );
}

#[test]
fn postcondition_failure_keeps_completed_action_and_redacts_page_errors() {
    let action = output(
        "Input acknowledged",
        json!({"dispatch_acknowledged":true,"click_count":1}),
    );
    let result = after_result(action, Err(error("PRIVATE-CANARY")));
    assert!(result.is_error);
    let details = result.details.unwrap();
    assert_eq!(details["dispatch_acknowledged"], true);
    assert_eq!(details["click_count"], 1);
    assert_eq!(details["wait_after"]["satisfied"], false);
    assert_eq!(details["wait_after"]["action_replayed"], false);
    assert!(!details.to_string().contains("PRIVATE-CANARY"));
    let action = output("Input acknowledged", json!(["previous"]));
    let result = after_result(action, Err(error("BROWSER_WAIT_STALE")));
    let details = result.details.unwrap();
    assert_eq!(details["action_details"], json!(["previous"]));
    assert_eq!(details["wait_after"]["error_kind"], "document_changed");
    log(
        "postcondition-failure",
        json!({"completed_action_retained":true,"page_error_redacted":true}),
    );
}
