//! Workspace symbol contracts, including omitted results and resolve identity.
use super::*;

fn symbol() -> Value {
    json!({"name":"resolve_me","kind":12,"containerName":"Module",
        "location":{"uri":"file:///workspace/module.rs"},"data":{"ticket":17,"literal":"$(not a command)"}})
}

fn span() -> Value {
    json!({"start":{"line":3,"character":2},"end":{"line":3,"character":9}})
}

#[test]
fn accepts_modern_unresolved_and_legacy_inline_symbols() {
    let mut value = symbol();
    assert!(!item(&value).unwrap());
    value["location"]["range"] = span();
    value["deprecated"] = json!(true);
    value["tags"] = json!([1,27]);
    assert!(item(&value).unwrap());
    assert_eq!(items(&json!([value.clone()])).unwrap()[0], value);
}

#[test]
fn concrete_ranges_preserve_zero_width_and_utf16_coordinates_without_clamping() {
    let mut value = symbol();
    value["location"]["range"] = json!({"start":{"line":0,"character":2},"end":{"line":0,"character":2}});
    assert!(item(&value).unwrap());
    // The remote target is not opened: only shape and ordering are checked.
    value["location"]["range"] = span();
    assert!(item(&value).unwrap());
    assert_eq!(value["location"]["range"]["start"]["character"], 2);
}

#[test]
fn rejects_malformed_items_and_ranges_even_when_omitted_by_output_limit() {
    for bad in [json!(null), json!(false), json!({}), json!({"name":"bad","kind":0}),
        json!({"name":"bad","kind":12,"location":{"uri":"relative.rs"}})] {
        assert!(items(&json!([symbol(), bad])).is_err());
    }
    for bad in [Value::Null, json!({}), json!({"start":{"line":1,"character":0},"end":{"line":0,"character":0}}),
        json!({"start":{"line":0,"character":-1},"end":{"line":0,"character":0}}),
        json!({"start":{"line":0,"character":0.5},"end":{"line":0,"character":1}}),
        json!({"start":{"line":2_147_483_648_u64,"character":0},"end":{"line":2_147_483_648_u64,"character":0}})] {
        let mut value = symbol();
        value["location"]["range"] = bad;
        assert!(items(&json!([symbol(), value])).is_err());
    }
}

#[test]
fn null_and_empty_reports_are_successful_absence_not_malformed_success() {
    assert!(items(&Value::Null).unwrap().is_empty());
    assert!(items(&json!([])).unwrap().is_empty());
    assert!(items(&json!({"items":[]})).is_err());
    assert!(items(&json!(42)).is_err());
}

#[test]
fn preserves_unknown_numeric_kinds_tags_and_external_metadata() {
    let mut value = symbol();
    value["kind"] = json!(700);
    value["tags"] = json!([1, 700]);
    value["location"]["uri"] = json!("https://example.invalid/private.rs");
    assert!(!item(&value).unwrap());
    assert_eq!(items(&json!([value.clone()])).unwrap()[0], value);
    for bad in [json!(-1), json!(1.5), json!("function")] {
        value["kind"] = bad;
        assert!(item(&value).is_err());
    }
}

#[test]
fn bounds_count_whole_report_and_individual_symbols() {
    assert!(items(&json!(vec![symbol(); MAX_SERVER_ITEMS + 1])).is_err());
    let mut value = symbol();
    value["data"] = json!("x".repeat(MAX_ITEM_BYTES));
    assert!(item(&value).is_err());
    assert!(items(&json!([{"data":"x".repeat(MAX_RESPONSE_BYTES)}])).is_err());
}

#[test]
fn resolution_adds_only_a_range_and_keeps_the_exact_original_payload() {
    let before = symbol();
    let mut after = before.clone();
    after["location"]["range"] = span();
    assert_eq!(resolved(&before, after.clone()).unwrap(), after);
    assert!(before["location"].get("range").is_none());
    assert_eq!(before["data"]["ticket"], 17);
}

#[test]
fn resolution_cannot_swap_identity_or_smuggle_unnegotiated_fields() {
    for (key,value) in [("name",json!("wrong")), ("kind",json!(13)),
        ("data",json!({"ticket":18})), ("containerName",json!("Other")),
        ("command",json!({"command":"unapproved"})), ("tags",json!([1]))] {
        let before = symbol();
        let mut after = before.clone();
        after["location"]["range"] = span();
        after[key] = value;
        assert!(resolved(&before,after).is_err(), "{key}");
    }
    let before = symbol();
    let mut after = before.clone();
    after["location"] = json!({"uri":"file:///other.rs","range":span()});
    assert!(resolved(&before, after).is_err());
    assert!(resolved(&before, before.clone()).is_err());
    let mut after = before.clone();
    after["location"]["range"] = span();
    after.as_object_mut().unwrap().remove("data");
    assert!(resolved(&before, after).is_err());
}

#[test]
fn provider_capabilities_never_infer_lazy_resolution() {
    for caps in [json!({}),json!({"workspaceSymbolProvider":true}),json!({"workspaceSymbolProvider":{}})] {
        assert!(!resolve_supported(&caps).unwrap());
    }
    assert!(resolve_supported(&json!({"workspaceSymbolProvider":{"resolveProvider":true}})).unwrap());
    for caps in [json!({"workspaceSymbolProvider":false}),json!({"workspaceSymbolProvider":1}),
        json!({"workspaceSymbolProvider":{"resolveProvider":"yes"}})] {
        assert!(resolve_supported(&caps).is_err());
    }
}

#[test]
fn validates_query_and_anchor_before_any_file_or_server_access() {
    for raw in [json!({"action":"symbols","file":"a.rs","query":""}),
        json!({"action":"symbols","symbol":"a.rs","query":"Module","resolve":true,"limit":1})] {
        let input: LspInput = serde_json::from_value(raw).unwrap();
        assert_eq!(request(&input).unwrap().0,"a.rs");
    }
    for extra in [json!({"symbol":"other.rs"}), json!({"query":"x".repeat(MAX_QUERY_BYTES+1)}),
        json!({"query":"a\u{0}b"}), json!({"limit":0}), json!({"apply":false}),
        json!({"line":1}), json!({"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}}),
        json!({"newName":"other"}), json!({"actionId":"action"}), json!({"method":"anything"})] {
        let mut raw = json!({"action":"symbols","file":"a.rs","query":"Module"});
        raw.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        let input: LspInput = serde_json::from_value(raw).unwrap();
        assert!(request(&input).is_err());
    }
    let missing: LspInput = serde_json::from_value(json!({"action":"symbols","query":"Module"})).unwrap();
    assert!(request(&missing).is_err());
}

mod protocol;
