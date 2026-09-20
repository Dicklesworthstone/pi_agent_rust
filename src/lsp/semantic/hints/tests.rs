use super::*;

const TEXT: &str = "let 😀 = call(value, flag)\r\n";

fn range() -> Range {
    hint_range(TEXT, None).unwrap()
}

fn hint() -> Value {
    json!({"position":{"line":0,"character":6},"label":[
        {"value":": ","tooltip":"prefix"},
        {"value":"bool","location":{"uri":"https://invalid.example/type","range":{
            "start":{"line":1,"character":0},"end":{"line":1,"character":4}
        }},"command":{"title":"must not execute","command":"unsafe.run","arguments":["private"]}}
    ],"kind":1,"paddingLeft":true,"tooltip":{"kind":"markdown","value":"Inferred type"},
    "data":{"opaque":[1,"x"]},"textEdits":[{"arbitrary":"never apply"}]})
}

#[test]
fn compound_labels_kinds_padding_tooltips_and_locations_are_retained() {
    let result = normalize(&hint(), TEXT, range(), 4, false).unwrap();
    assert_eq!(result["index"], 4);
    assert_eq!(result["kind"], "type");
    assert_eq!(result["label"], ": bool");
    assert_eq!(result["paddingLeft"], true);
    assert_eq!(result["paddingRight"], false);
    assert_eq!(result["resolved"], false);
    assert_eq!(
        result["labelParts"][1]["location"]["uri"],
        "https://invalid.example/type"
    );
    assert_eq!(result["tooltip"]["value"], "Inferred type");
}

#[test]
fn accepting_edits_and_commands_is_not_part_of_inspection_output() {
    let result = normalize(&hint(), TEXT, range(), 0, false).unwrap();
    assert_eq!(result["textEditsPresent"], true);
    assert_eq!(result["labelParts"][1]["commandPresent"], true);
    assert!(result.get("textEdits").is_none());
    assert!(result.get("data").is_none());
    assert!(result["labelParts"][1].get("command").is_none());
    assert!(!result.to_string().contains("private"));
}

#[test]
fn hint_positions_reject_surrogate_interiors_and_document_overflow() {
    for position in [
        json!({"line":0,"character":5}),
        json!({"line":0,"character":999}),
        json!({"line":2,"character":0}),
        json!({"line":0,"character":-1}),
    ] {
        let mut raw = hint();
        raw["position"] = position;
        assert!(normalize(&raw, TEXT, range(), 0, false).is_err());
    }
}

#[test]
fn zero_width_anchors_can_touch_either_selected_endpoint() {
    let mut raw = hint();
    let selection = Range {
        start: Position {
            line: 0,
            character: 6,
        },
        end: Position {
            line: 0,
            character: 10,
        },
    };
    for col in [6, 10] {
        raw["position"]["character"] = json!(col);
        normalize(&raw, TEXT, selection, 0, false).unwrap();
    }
    raw["position"]["character"] = json!(11);
    assert!(normalize(&raw, TEXT, selection, 0, false).is_err());
    raw["position"]["character"] = json!(6);
    normalize(
        &raw,
        TEXT,
        Range {
            start: selection.start,
            end: selection.start,
        },
        0,
        false,
    )
    .unwrap();
}

#[test]
fn whole_file_ranges_share_exact_cr_lf_crlf_and_unicode_mapping() {
    for (text, end) in [
        (
            "a\rb\r\n😀",
            Position {
                line: 2,
                character: 2,
            },
        ),
        (
            "",
            Position {
                line: 0,
                character: 0,
            },
        ),
        (
            "a\n",
            Position {
                line: 1,
                character: 0,
            },
        ),
    ] {
        assert_eq!(hint_range(text, None).unwrap().end, end);
    }
    assert!(
        hint_range(
            TEXT,
            Some(Range {
                start: Position {
                    line: 0,
                    character: 5
                },
                end: range().end
            })
        )
        .is_err()
    );
    assert!(
        hint_range(
            TEXT,
            Some(Range {
                start: range().end,
                end: range().start
            })
        )
        .is_err()
    );
}

#[test]
fn all_supported_kinds_and_string_labels_have_explicit_output() {
    for (kind, name) in [
        (Value::Null, "unspecified"),
        (json!(1), "type"),
        (json!(2), "parameter"),
    ] {
        let mut raw = hint();
        raw["label"] = json!("value:");
        raw["kind"] = kind;
        let result = normalize(&raw, TEXT, range(), 0, false).unwrap();
        assert_eq!(result["kind"], name);
        assert_eq!(result["labelParts"], json!([]));
    }
    for kind in [json!(0), json!(3), json!("1"), json!(-1), json!(1.5)] {
        let mut raw = hint();
        raw["kind"] = kind;
        assert!(normalize(&raw, TEXT, range(), 0, false).is_err());
    }
}

#[test]
fn empty_or_malformed_labels_are_rejected_instead_of_silently_omitted() {
    for label in [
        json!(""),
        json!([]),
        json!([{"value":""}]),
        json!(["text"]),
        json!(2),
    ] {
        let mut raw = hint();
        raw["label"] = label;
        assert!(normalize(&raw, TEXT, range(), 0, false).is_err());
    }
}

#[test]
fn label_parts_and_total_utf8_label_bytes_are_bounded() {
    for label in [
        json!("x".repeat(MAX_LABEL_BYTES + 1)),
        json!(vec![json!({"value":"x"}); MAX_LABEL_PARTS + 1]),
        json!([{"value":"😀".repeat(MAX_LABEL_BYTES/4)},{"value":"x"}]),
    ] {
        let mut raw = hint();
        raw["label"] = label;
        assert!(normalize(&raw, TEXT, range(), 0, false).is_err());
    }
    let mut raw = hint();
    raw["data"] = json!("x".repeat(MAX_HINT_BYTES));
    assert!(normalize(&raw, TEXT, range(), 0, false).is_err());
}

#[test]
fn malformed_padding_tooltips_locations_and_edit_containers_are_errors() {
    for (key, value) in [
        ("paddingLeft", json!(1)),
        ("paddingRight", json!("true")),
        ("tooltip", json!({"kind":"html","value":"x"})),
        ("textEdits", json!({})),
    ] {
        let mut raw = hint();
        raw[key] = value;
        assert!(normalize(&raw, TEXT, range(), 0, false).is_err());
    }
    for loc in [
        json!({"uri":"","range":range()}),
        json!({"uri":"x","range":{"start":{"line":2,"character":0},"end":{"line":1,"character":0}}}),
    ] {
        let mut raw = hint();
        raw["label"][1]["location"] = loc;
        assert!(normalize(&raw, TEXT, range(), 0, false).is_err());
    }
}

#[test]
fn resolve_identity_allows_only_negotiated_tooltips_and_locations() {
    let raw = hint();
    let mut resolved = raw.clone();
    resolved["tooltip"] = json!("new details");
    resolved["label"][0]["tooltip"] = json!("part details");
    resolved["label"][1]["location"] = Value::Null;
    assert_eq!(identity(&raw), identity(&resolved));
    assert_eq!(raw["tooltip"]["value"], "Inferred type");
    for (key, value) in [
        ("position", json!({"line":0,"character":7})),
        ("label", json!("substitute")),
        ("kind", json!(2)),
        ("paddingLeft", json!(false)),
        ("data", json!({"new":"identity"})),
        ("textEdits", json!([])),
        ("unknown", json!(1)),
    ] {
        let mut other = resolved.clone();
        other[key] = value;
        assert_ne!(identity(&raw), identity(&other), "{key}");
    }
    resolved["label"][1]["command"] = Value::Null;
    assert_ne!(identity(&raw), identity(&resolved));
}

#[test]
fn static_and_resolvable_provider_capabilities_are_distinguished() {
    for provider in [json!(true), json!({}), json!({"resolveProvider":false})] {
        assert!(!resolve_supported(&json!({"inlayHintProvider":provider})).unwrap());
    }
    assert!(resolve_supported(&json!({"inlayHintProvider":{"resolveProvider":true}})).unwrap());
    for provider in [
        Value::Null,
        json!(false),
        json!(1),
        json!({"resolveProvider":"true"}),
    ] {
        assert!(resolve_supported(&json!({"inlayHintProvider":provider})).is_err());
    }
}
