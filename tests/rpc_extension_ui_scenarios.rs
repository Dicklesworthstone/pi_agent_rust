#![forbid(unsafe_code)]

use std::collections::BTreeSet;

use serde_json::Value;

const FIXTURE: &str = include_str!("fixtures/rpc_extension_ui_scenarios.json");

#[test]
fn rpc_extension_ui_fixture_has_required_parity_scenarios() {
    let fixture: Value = serde_json::from_str(FIXTURE).expect("valid extension UI scenario JSON");
    assert_eq!(fixture["schema"], "pi.rpc.extension_ui_scenarios.v1");
    assert_eq!(fixture["bead"], "bd-lnmtp.2.4");
    assert_eq!(
        fixture["wire"]["request_event_type"],
        "extension_ui_request"
    );
    assert_eq!(
        fixture["wire"]["response_command_type"],
        "extension_ui_response"
    );
    assert_eq!(
        fixture["wire"]["request_generation_field"],
        "requestGeneration"
    );

    let scenarios = fixture["scenarios"]
        .as_array()
        .expect("scenarios must be an array");
    assert!(
        scenarios.len() >= 10,
        "G05-T4 requires at least 10 extension UI scenarios"
    );

    let mut ids = BTreeSet::new();
    let mut methods = BTreeSet::new();
    let mut classes = BTreeSet::new();

    for scenario in scenarios {
        let id = scenario["id"].as_str().expect("scenario id");
        assert!(ids.insert(id), "duplicate scenario id: {id}");

        let method = scenario["method"].as_str().expect("scenario method");
        methods.insert(method);
        classes.insert(scenario["class"].as_str().expect("scenario class"));

        assert_eq!(scenario["request"]["method"], method);
        assert!(
            !scenario["request"]["id"]
                .as_str()
                .unwrap_or_default()
                .is_empty(),
            "request id must be present for {id}"
        );

        if let Some(response) = scenario.get("response").filter(|value| !value.is_null()) {
            assert_eq!(
                response["type"], "extension_ui_response",
                "response command type for {id}"
            );
            assert!(
                response.get("requestId").is_some() || response.get("id").is_some(),
                "response must carry requestId or legacy id alias for {id}"
            );
            assert!(
                response
                    .get("requestGeneration")
                    .and_then(Value::as_u64)
                    .is_some(),
                "response must carry an unsigned requestGeneration for {id}"
            );
        }
    }

    for method in ["confirm", "select", "input", "editor", "notify"] {
        assert!(
            methods.contains(method),
            "missing method scenario: {method}"
        );
    }

    for class in [
        "roundtrip",
        "timeout",
        "abort",
        "concurrent-ordering",
        "legacy-alias",
        "fire-and-forget",
    ] {
        assert!(classes.contains(class), "missing scenario class: {class}");
    }
}
