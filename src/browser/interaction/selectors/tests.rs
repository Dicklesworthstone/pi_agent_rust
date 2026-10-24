use super::*;

fn case(name: &str) {
    eprintln!(
        "{}",
        json!({"suite":"browser_semantic_selectors","case":name,"phase":"start"})
    );
}

fn locator(text: &str) -> Locator {
    parse(text).unwrap().unwrap()
}

fn node(id: u64, role: &str, name: &str) -> Value {
    json!({"backendDOMNodeId":id,"ignored":false,"role":{"value":role},"name":{"value":name}})
}

#[test]
fn recognized_prefixes_parse_without_reinterpreting_css_or_references() {
    case("recognized-prefixes");
    for selector in [
        "#save",
        "input[name='x/y']",
        "@e1",
        "text\\/example",
        "svg|a",
    ] {
        assert!(parse(selector).unwrap().is_none(), "{selector}");
    }
    for (prefix, kind) in [
        ("text", Kind::Text),
        ("label", Kind::Label),
        ("placeholder", Kind::Placeholder),
        ("testid", Kind::Testid),
        ("alt", Kind::Alt),
        ("title", Kind::Title),
    ] {
        let selected = locator(&format!("{prefix}/example/value"));
        assert_eq!(selected.kind, kind);
        assert_eq!(selected.value, "example/value");
    }
}

#[test]
fn text_normalizes_whitespace_but_test_ids_remain_exact_literals() {
    case("literal-and-whitespace-semantics");
    assert_eq!(locator("text/  雪\n\t board\u{00a0} ").value, "雪 board");
    let id = locator("testid/ literal / id ");
    assert_eq!(id.value, " literal / id ");
    assert!(id.exact);
    let text = locator("text/'; globalThis.secret = true; //");
    assert_eq!(text.value, "'; globalThis.secret = true; //");
}

#[test]
fn role_filters_decode_quoted_names_and_explicit_exact_matching() {
    case("role-filter-grammar");
    let plain = locator("role/button");
    assert_eq!(plain.value, "button");
    assert_eq!(plain.name, None);
    let selected = locator(r#"role/button[name="Save \"draft\"" exact]"#);
    assert_eq!(selected.name.as_deref(), Some("Save \"draft\""));
    assert!(selected.exact);
    let selected = locator(r#"role/textbox[name="Account\u0020 name"]"#);
    assert_eq!(selected.name.as_deref(), Some("Account name"));
    assert!(!selected.exact);
}

#[test]
fn malformed_role_filters_never_fall_back_to_css_or_echo_input() {
    case("invalid-role-filter");
    for selector in [
        "role/",
        "role/Button",
        "role/button junk",
        "role/button[name='PRIVATE-CANARY']",
        "role/button[name=42]",
        "role/button[name=null]",
        "role/button[other=\"x\"]",
        "role/button[name=\"x\"",
        "role/button[name=\"x\"exact]",
        "role/button[name=\"x\"]tail",
        "role/button[name=\"\"]",
        "role/button[name=\"\\u0000\"]",
        "role/button[]",
    ] {
        let failure = parse(selector).unwrap_err().to_string();
        assert!(
            failure.contains("BROWSER_SELECTOR_USAGE"),
            "{selector}: {failure}"
        );
        assert!(!failure.contains("PRIVATE-CANARY"));
    }
}

#[test]
fn known_prefixes_have_utf8_byte_limits_and_reject_empty_or_nul_queries() {
    case("input-limits");
    assert!(parse(&format!("text/{}", "x".repeat(MAX_SELECTOR_BYTES - 5))).is_ok());
    for selector in [
        "text/".into(),
        "label/ \n ".into(),
        "testid/".into(),
        "title/a\0b".into(),
        format!("text/{}", "x".repeat(MAX_SELECTOR_BYTES - 4)),
        format!("alt/{}", "雪".repeat(MAX_SELECTOR_BYTES / 3)),
    ] {
        assert!(parse(&selector).is_err());
    }
}

#[test]
fn preflight_refuses_ignored_selectors_before_any_browser_work() {
    case("public-preflight");
    for action in [
        "click",
        "type",
        "fill",
        "press",
        "wait_for",
        "select_option",
        "set_checked",
        "upload",
    ] {
        assert!(validate_input(&json!({"action":action,"selector":"text/Save"})).unwrap());
    }
    for action in [
        "evaluate",
        "open",
        "goto",
        "snapshot",
        "screenshot",
        "download",
        "scroll",
    ] {
        assert!(validate_input(&json!({"action":action,"selector":"text/Save"})).is_err());
    }
    assert!(
        validate_input(&json!({"action":"scroll","frame":"frame-1","selector":"text/Save"}))
            .unwrap()
    );
    assert!(!validate_input(&json!({"action":"snapshot"})).unwrap());
    assert!(!validate_input(&json!({"action":"click","selector":"#save"})).unwrap());
}

#[test]
fn role_matching_uses_accessible_names_and_never_editable_values() {
    case("browser-accessible-name");
    let mut password = node(1, "textbox", "Password");
    password["value"] = json!({"value":"PRIVATE-CANARY"});
    let nodes = vec![
        password,
        node(2, "button", "Save changes"),
        node(3, "button", "Save draft"),
    ];
    assert!(
        ax_candidates(&nodes, &locator("label/PRIVATE-CANARY"))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        ax_candidates(&nodes, &locator("label/Password")).unwrap(),
        BTreeSet::from([1])
    );
    assert_eq!(
        ax_candidates(&nodes, &locator(r#"role/button[name="Save"]"#)).unwrap(),
        BTreeSet::from([2, 3])
    );
    assert_eq!(
        ax_candidates(
            &nodes,
            &locator(r#"role/button[name="Save changes" exact]"#)
        )
        .unwrap(),
        BTreeSet::from([2])
    );
    assert!(
        ax_candidates(&nodes, &locator(r#"role/button[name="save"]"#))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn ignored_duplicate_and_non_element_accessibility_nodes_are_not_new_matches() {
    case("accessibility-node-selection");
    let mut ignored = node(2, "button", "Save");
    ignored["ignored"] = json!(true);
    let nodes = vec![
        node(1, "button", "Save"),
        node(1, "button", "Save"),
        ignored,
        node(0, "button", "Save"),
        node(3, "StaticText", "Save"),
    ];
    assert_eq!(
        ax_candidates(&nodes, &locator("role/button")).unwrap(),
        BTreeSet::from([1])
    );
    assert_eq!(
        ax_candidates(&nodes, &locator("label/Save")).unwrap(),
        BTreeSet::from([1])
    );
}

#[test]
fn oversized_accessibility_results_fail_instead_of_selecting_a_partial_tree() {
    case("accessibility-limits");
    let query = locator("role/button");
    assert!(ax_candidates(&vec![Value::Null; MAX_AX_NODES + 1], &query).is_err());
    let nodes: Vec<_> = (1..=MAX_CANDIDATES + 1)
        .map(|id| node(id as u64, "button", "Save"))
        .collect();
    assert!(ax_candidates(&nodes, &query).is_err());
    let failure = ax_candidates(
        &[node(1, "button", &"x".repeat(MAX_SELECTOR_BYTES + 1))],
        &query,
    )
    .unwrap_err()
    .to_string();
    assert!(failure.contains("BROWSER_SELECTOR_LIMIT"));
}

#[test]
fn query_errors_publish_only_static_classifications_not_page_supplied_details() {
    case("error-redaction");
    assert!(check_query(&json!({"result":{"type":"object","subtype":"null"}})).is_ok());
    for message in [
        "PRIVATE-CANARY",
        "Error: BROWSER_SELECTOR_AMBIGUOUS PRIVATE-CANARY",
        "BROWSER_SELECTOR_LIMIT PRIVATE-CANARY",
    ] {
        let failure =
            check_query(&json!({"exceptionDetails":{"exception":{"description":message}}}))
                .unwrap_err()
                .to_string();
        assert!(!failure.contains("PRIVATE-CANARY"));
        assert!(failure.contains("BROWSER_SELECTOR_"));
    }
}
