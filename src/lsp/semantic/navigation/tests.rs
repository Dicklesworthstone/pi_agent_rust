use super::*;
use std::path::Path;
use std::time::Duration;

pub(super) fn span(start: u32, end: u32) -> Value {
    json!({"start":{"line":0,"character":start},"end":{"line":0,"character":end}})
}

fn plain() -> Value {
    json!({"uri":"file:///not-opened.rs","range":span(1,4)})
}

fn link() -> Value {
    json!({"targetUri":"generated:module/definition","targetRange":span(0,10),
        "targetSelectionRange":span(2,5),"originSelectionRange":span(2,3)})
}

fn output(raw: &Value, limit: usize, references: bool) -> Result<Value> {
    location_output(raw, "😀x", Path::new("/workspace"), limit, references,
        json!({"action":"definition"}), &Budget::new(Duration::from_secs(10)))
}

#[test]
fn location_forms_preserve_full_ranges_and_original_uris() {
    for raw in [plain(), json!([plain()])] {
        let out = output(&raw, 100, false).unwrap();
        assert_eq!(out["count"], 1);
        assert_eq!(out["locations"][0]["range"], span(1,4));
        assert_eq!(out["locations"][0]["uri"], "file:///not-opened.rs");
        assert_eq!(out["locations"][0]["character"], 2);
        assert_eq!(out["responseComplete"], true);
    }
    assert!(output(&plain(), 100, true).is_err());
    assert_eq!(output(&json!([plain()]), 100, true).unwrap()["count"], 1);
}

#[test]
fn link_selection_and_definition_body_remain_distinct() {
    let out = output(&json!([link()]), 100, false).unwrap();
    let item = &out["locations"][0];
    assert_eq!(item["uri"], "generated:module/definition");
    assert_eq!(item["file"], item["uri"]);
    assert_eq!(item["range"], span(2,5));
    assert_eq!(item["targetRange"], span(0,10));
    assert_eq!(item["targetSelectionRange"], span(2,5));
    assert_eq!(item["originSelectionRange"], span(2,3));
    assert!(output(&link(), 100, false).is_err());
    assert!(output(&json!([link()]), 100, true).is_err());
}

#[test]
fn missing_or_outside_link_selections_never_fall_back_to_body() {
    for change in [json!({"targetSelectionRange":null}),
        json!({"targetSelectionRange":span(2,11)}), json!({"targetRange":span(4,10)}),
        json!({"uri":"file:///mixed"}), json!({"originSelectionRange":span(1,2)})]
    {
        let mut item = link();
        item.as_object_mut().unwrap().extend(change.as_object().unwrap().clone());
        assert!(output(&json!([item]), 1, false).is_err());
    }
}

#[test]
fn bad_tail_is_rejected_even_after_count_or_byte_truncation() {
    assert!(output(&json!([plain(),false]), 1, false).is_err());
    let mut items = vec![plain(); 40];
    for item in &mut items { item["uri"] = json!(format!("generated:{}", "x".repeat(8000))); }
    let out = output(&json!(items), 1000, false).unwrap();
    assert_eq!(out["truncated"], true);
    assert!(out["count"].as_u64().unwrap() < 40);
    items.push(json!({"uri":"file:///broken","range":span(3,1)}));
    assert!(output(&json!(items), 1000, false).is_err());
}

#[test]
fn output_preserves_order_and_duplicates_and_exposes_omitted_count() {
    let raw = json!([plain(),plain(),plain()]);
    let out = output(&raw, 2, true).unwrap();
    assert_eq!(out["total"], 3);
    assert_eq!(out["count"], 2);
    assert_eq!(out["locations"][0], out["locations"][1]);
    assert_eq!(out["responseComplete"], false);
    assert_eq!(out["truncated"], true);
    assert!(output(&json!([plain(),link()]), 1, false).is_err());
}

#[test]
fn malformed_coordinate_and_uri_types_fail_instead_of_empty_success() {
    for value in [json!(-1),json!(1.5),json!(2_147_483_648_u64),json!("1"),Value::Null] {
        let mut item = plain(); item["range"]["start"]["line"] = value;
        assert!(output(&item, 1, false).is_err());
    }
    for uri in ["relative.rs", "", "file:///bad\n.rs", "file:///has space.rs"] {
        let mut item = plain(); item["uri"] = json!(uri);
        assert!(output(&item, 1, false).is_err());
    }
    for raw in [json!(false),json!(42),json!({}),json!([null])] {
        assert!(output(&raw, 1, false).is_err());
    }
}

#[test]
fn valid_null_and_empty_reports_remain_successful_and_complete() {
    for raw in [Value::Null, json!([])] {
        let out = output(&raw, 100, false).unwrap();
        assert_eq!(out["total"], 0);
        assert_eq!(out["locations"], json!([]));
        assert_eq!(out["responseComplete"], true);
    }
}

#[test]
fn navigation_limits_fail_closed_and_maximum_coordinates_do_not_overflow() {
    assert!(output(&json!(vec![plain();MAX_SERVER_LOCATIONS+1]), 1, false).is_err());
    let mut item = plain();
    item["range"] = json!({"start":{"line":MAX_LSP_UINTEGER,"character":MAX_LSP_UINTEGER},
        "end":{"line":MAX_LSP_UINTEGER,"character":MAX_LSP_UINTEGER}});
    let out = output(&item, 1, false).unwrap();
    assert_eq!(out["locations"][0]["line"], 2_147_483_648_u64);
    item["uri"] = json!(format!("generated:{}", "x".repeat(MAX_RESPONSE_BYTES)));
    assert!(output(&item, 1, false).is_err());
}

#[test]
fn navigation_accepts_exact_cursor_or_symbol_but_never_both() {
    for target in [json!({"position":{"line":0,"character":2}}),json!({"symbol":"x#2","line":1})] {
        let mut raw = json!({"action":"definition","file":"a.rs"});
        raw.as_object_mut().unwrap().extend(target.as_object().unwrap().clone());
        input_file(&serde_json::from_value(raw).unwrap()).unwrap();
    }
    for extra in [json!({"symbol":"x"}),json!({"line":1}),json!({"apply":false}),
        json!({"query":"x"}),json!({"range":span(0,1)}),json!({"limit":0}),
        json!({"refactorId":"id"}),json!({"formatOptions":{}}),json!({"resolve":false})]
    {
        let mut raw = json!({"action":"definition","file":"a.rs","position":{"line":0,"character":2}});
        raw.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        assert!(input_file(&serde_json::from_value(raw).unwrap()).is_err());
    }
    assert!(input_file(&serde_json::from_value(json!({"action":"hover","file":"a.rs"})).unwrap()).is_err());
}

#[test]
fn hover_preserves_markup_language_and_exact_source_range() {
    for contents in [json!("docs"), json!({"kind":"markdown","value":"**docs**"}),
        json!({"kind":"plaintext","value":"<literal>"}),
        json!([{"language":"rust","value":"fn x()"},"docs"]), json!([])]
    {
        let raw = json!({"contents":contents,"range":span(2,3)});
        let out = hover_output(&raw,"😀x",json!({})).unwrap();
        assert_eq!(out["contents"], contents);
        assert_eq!(out["range"], span(2,3));
        assert_eq!(out["returnedNull"], false);
    }
    let out = hover_output(&Value::Null,"😀x",json!({})).unwrap();
    assert_eq!(out["returnedNull"], true);
    assert_eq!(out["hover"], "no hover information");
}

#[test]
fn invalid_hover_items_cannot_disappear_in_a_successful_summary() {
    for raw in [json!({}),json!({"contents":null}),json!({"contents":["valid",false]}),
        json!({"contents":{"kind":"html","value":"x"}}),
        json!({"contents":[{"kind":"markdown","value":"x"}]}),
        json!({"contents":{"value":"x"}}),json!({"contents":"x","range":span(1,2)})]
    {
        assert!(hover_output(&raw,"😀x",json!({})).is_err());
    }
    assert!(hover_output(&json!({"contents":"x".repeat(MAX_PAYLOAD_BYTES)}),"x",json!({})).is_err());
}

#[test]
fn explicit_disabled_and_malformed_capabilities_do_not_dispatch() {
    for action in ["definition","type_definition","implementation","references","hover"] {
        capability(&json!({}), action).unwrap();
    }
    for value in [json!(false),json!(3),json!("enabled")] {
        assert!(capability(&json!({"definitionProvider":value}), "definition").is_err());
    }
    capability(&json!({"definitionProvider":{}}), "definition").unwrap();
}
