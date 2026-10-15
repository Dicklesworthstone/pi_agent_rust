use super::*;

fn log(case: &str) {
    eprintln!("{}", json!({"suite":"browser-navigation","case":case}));
}

fn frame(address: &str, loader: &str) -> Value {
    json!({"id":"main","loaderId":loader,"url":address,"securityOrigin":"https://app.example"})
}

fn navigation() -> Navigation {
    Navigation {
        before: Snapshot::parse(&frame("https://app.example/start", "old")).unwrap(),
        options: Options::parse(&json!({"url":"https://app.example/done"})).unwrap(),
    }
}

#[test]
fn exact_destination_urls_and_options_are_bounded_literal_values() {
    log("literal-options");
    assert_eq!(Options::parse(&json!({"url":"https://APP.example:443/done?x=1#ready"})).unwrap()
        .target.as_str(), "https://app.example/done?x=1#ready");
    for address in ["/done", "https:app.example/done", "file:///tmp/a", "javascript:void(0)",
        "https://user:PRIVATE-CANARY@app.example/", "https://app.example/\n", "https://app.example/a b"] {
        let error = Options::parse(&json!({"url":address})).err().unwrap().to_string();
        assert!(!error.contains("PRIVATE-CANARY"));
    }
    for value in [Value::Null, json!({}), json!({"url":"https://app.example/","wait_until":"networkidle"}),
        json!({"url":"https://app.example/","wait_until":null}),
        json!({"url":"https://app.example/","timeout_ms":1000}),
        json!({"url":format!("https://app.example/{}", "x".repeat(MAX_URL_BYTES))})] {
        assert!(Options::parse(&value).is_err());
    }
}

#[test]
fn navigation_is_explicit_and_cannot_redirect_input_or_expand_authority() {
    log("admission");
    let base = json!({"action":"click","tab":"app","selector":"#continue",
        "wait_after":{"navigation":{"url":"https://app.example/done"}}});
    assert!(super::super::after_condition(&base).unwrap().is_some());
    let mut key = base.clone(); key["action"] = json!("press"); key["key"] = json!("Enter");
    assert!(super::super::after_condition(&key).is_ok());
    for (key, value) in [("action",json!("fill")), ("tab",Value::Null), ("frame",json!("child")),
        ("button",json!("right")), ("click_count",json!(2)), ("modifiers",json!(["Control"])),
        ("dialog_response",json!({}))] {
        let mut invalid = base.clone(); invalid[key] = value;
        assert!(super::super::after_condition(&invalid).is_err());
    }
    let mut invalid = base.clone(); invalid["wait_after"]["selector"] = json!("@e1");
    assert!(super::super::after_condition(&invalid).is_err());
    let mut invalid = base.clone(); invalid["wait_after"]["stable_ms"] = json!(10);
    assert!(super::super::after_condition(&invalid).is_err(), "DOM options require a selector");
    let mut invalid = base; invalid["navigation"] = json!({"url":"https://app.example/"});
    assert!(super::super::validate_input(&invalid).is_err());
}

#[test]
fn frame_metadata_requires_real_security_origin_and_nonerror_document() {
    log("security-origin");
    for origin in [Value::Null, json!("null"), json!("https://other.example"),
        json!("https://app.example/scope"), json!("https://app.example/?private=1")] {
        let mut value = frame("https://app.example/start", "old"); value["securityOrigin"] = origin;
        assert!(Snapshot::parse(&value).is_err());
    }
    for field in ["id", "loaderId"] {
        let mut value = frame("https://app.example/start", "old"); value[field] = json!("");
        assert!(Snapshot::parse(&value).is_err());
    }
    let mut value = frame("https://app.example/start", "old"); value["unreachableUrl"] = json!("https://app.example/start");
    assert!(Snapshot::parse(&value).unwrap_err().to_string().contains("BROWSER_NAVIGATION_FAILED"));
}

#[test]
fn separated_fragments_are_included_without_double_appending() {
    log("frame-fragment");
    let mut value = frame("https://app.example/start?x=1", "old"); value["urlFragment"] = json!("#step");
    assert_eq!(Snapshot::parse(&value).unwrap().url.as_str(), "https://app.example/start?x=1#step");
    value["url"] = json!("https://app.example/start?x=1#step");
    assert_eq!(Snapshot::parse(&value).unwrap().url.fragment(), Some("step"));
    value["urlFragment"] = json!("#other");
    assert!(Snapshot::parse(&value).is_err());
    value["urlFragment"] = json!("step");
    assert!(Snapshot::parse(&value).is_err());
}

#[test]
fn unchanged_pages_never_satisfy_navigation_but_history_and_reload_can() {
    log("transition-required");
    let mut nav = navigation();
    assert!(!nav.matches(&nav.before).unwrap());
    assert!(nav.matches(&Snapshot::parse(&frame("https://app.example/done", "new")).unwrap()).unwrap());
    assert!(nav.matches(&Snapshot::parse(&frame("https://app.example/done", "old")).unwrap()).unwrap());
    assert!(!nav.matches(&Snapshot::parse(&frame("https://app.example/intermediate", "new")).unwrap()).unwrap());
    nav.options.target = nav.before.url.clone();
    assert!(!nav.matches(&nav.before).unwrap());
    assert!(nav.matches(&Snapshot::parse(&frame("https://app.example/start", "reloaded")).unwrap()).unwrap());
    let mut outside = frame("https://other.example/done", "new"); outside["securityOrigin"] = json!("https://other.example");
    assert!(nav.matches(&Snapshot::parse(&outside).unwrap()).is_err());
    let mut other_frame = frame("https://app.example/start", "new"); other_frame["id"] = json!("replacement");
    assert!(nav.matches(&Snapshot::parse(&other_frame).unwrap()).is_err());
}

#[test]
fn actual_event_timing_not_ready_state_controls_load_milestones() {
    log("event-not-ready-state");
    let interactive = json!({"state":"interactive","domContentLoaded":false,"load":false});
    assert!(!Until::DomContentLoaded.ready(&interactive).unwrap(), "deferred scripts can still be loading");
    let complete = json!({"state":"complete","domContentLoaded":true,"load":false});
    assert!(!Until::Load.ready(&complete).unwrap(), "complete precedes load dispatch");
    assert!(Until::DomContentLoaded.ready(&complete).unwrap());
    let loaded = json!({"state":"complete","domContentLoaded":true,"load":true});
    assert!(Until::Load.ready(&loaded).unwrap());
    for value in [json!({"state":"interactive"}),
        json!({"state":"loading","domContentLoaded":true,"load":false}),
        json!({"state":"interactive","domContentLoaded":true,"load":true}),
        json!({"state":"complete","domContentLoaded":false,"load":true}),
        json!({"state":"complete","domContentLoaded":"true","load":true})] {
        assert!(Until::Load.ready(&value).is_err());
    }
}

#[test]
fn navigation_errors_preserve_completed_input_without_reflecting_urls() {
    log("retained-input");
    for (message, kind) in [("BROWSER_NAVIGATION_SCOPE: PRIVATE-CANARY", "navigation_scope"),
        ("BROWSER_NAVIGATION_STALE: PRIVATE-CANARY", "document_changed"),
        ("BROWSER_NAVIGATION_FAILED: PRIVATE-CANARY", "navigation_failed")] {
        let result = super::super::after_result(output("Input acknowledged", json!({"dispatch_acknowledged":true})),
            Err(error(message)));
        assert!(result.is_error);
        let details = result.details.unwrap();
        assert_eq!(details["dispatch_acknowledged"], true);
        assert_eq!(details["wait_after"]["error_kind"], kind);
        assert!(!details.to_string().contains("PRIVATE-CANARY"));
    }
}
