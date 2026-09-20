use super::*;

fn signature() -> Value {
    json!({"signatures":[{"label":"call(😀: Text, flag: bool)","parameters":[
        {"label":[5,13],"documentation":{"kind":"markdown","value":"Unicode argument"}},
        {"label":"flag: bool"}
    ],"activeParameter":1}],"activeSignature":0,"activeParameter":0})
}

#[test]
fn utf16_parameter_offsets_are_decoded_without_byte_or_scalar_confusion() {
    let result = parse(&signature(), 16).unwrap();
    assert_eq!(result["signatures"][0]["parameters"][0]["label"], "😀: Text");
    assert_eq!(result["signatures"][0]["parameters"][0]["labelOffsets"], json!([5,13]));
    assert_eq!(result["signatures"][0]["parameters"][1]["label"], "flag: bool");
    assert_eq!(result["activeParameter"], 1);
}

#[test]
fn offset_mapping_counts_whole_multiline_labels_and_exact_boundaries() {
    assert_eq!(label_offset("a\r\n😀z", 3), Some(3));
    assert_eq!(label_offset("a\r\n😀z", 4), None);
    assert_eq!(label_offset("a\r\n😀z", 5), Some(7));
    assert_eq!(label_offset("a\r\n😀z", 6), Some(8));
    assert_eq!(label_offset("a\r\n😀z", 7), None);
}

#[test]
fn malformed_parameter_offsets_are_not_clamped() {
    for pair in [json!([5,6]),json!([9,8]),json!([0,999]),json!([-1,1]),json!([0]),json!([0,1,2]),json!([0,1.5])] {
        let mut raw = signature(); raw["signatures"][0]["parameters"][0]["label"] = pair;
        assert!(parse(&raw, 16).is_err(), "{raw}");
    }
}

#[test]
fn documented_active_signature_and_parameter_defaults_are_observed() {
    let raw = json!({"signatures":[{"label":"f(a)","parameters":[{"label":"a"}]}],"activeSignature":999,"activeParameter":999});
    let result = parse(&raw, 16).unwrap();
    assert_eq!(result["activeSignature"], 0);
    assert_eq!(result["activeParameter"], 0);
    let raw = json!({"signatures":[{"label":"f()"}],"activeParameter":999});
    assert_eq!(parse(&raw, 16).unwrap()["activeParameter"], Value::Null);
}

#[test]
fn overload_parameter_override_takes_precedence_even_when_out_of_range() {
    let mut raw = signature(); raw["signatures"][0]["activeParameter"] = json!(9); raw["activeParameter"] = json!(1);
    assert_eq!(parse(&raw,16).unwrap()["activeParameter"],0);
    raw["signatures"][0].as_object_mut().unwrap().remove("activeParameter");
    assert_eq!(parse(&raw,16).unwrap()["activeParameter"],1);
}

#[test]
fn truncation_retains_the_active_overload_and_original_indices() {
    let raw = json!({"signatures":[{"label":"a()"},{"label":"b()"},{"label":"c()"}],"activeSignature":2});
    let result = parse(&raw,2).unwrap();
    assert_eq!(result["total"],3); assert_eq!(result["count"],2); assert_eq!(result["truncated"],true);
    assert_eq!(result["activeSignature"],2); assert_eq!(result["signatures"][1]["index"],2);
    assert_eq!(parse(&raw,1).unwrap()["signatures"][0]["label"],"c()");
}

#[test]
fn no_signatures_is_distinct_from_a_malformed_response() {
    for raw in [Value::Null,json!({"signatures":[]})] {
        let result = parse(&raw,16).unwrap();
        assert_eq!(result["count"],0); assert!(result["activeSignature"].is_null());
        assert!(result["activeParameter"].is_null());
    }
    for raw in [json!({}),json!(false),json!([]),json!({"signatures":null}),json!({"signatures":[{}]})] {
        assert!(parse(&raw,16).is_err());
    }
}

#[test]
fn invalid_active_index_types_are_not_silently_defaulted() {
    for value in [json!("0"),json!(-1),json!(1.5),json!(2147483648_u64)] {
        for key in ["activeSignature","activeParameter"] {
            let mut raw = signature(); raw[key] = value.clone();
            assert!(parse(&raw,16).is_err());
        }
    }
}

#[test]
fn documentation_is_preserved_as_data_and_not_executed_or_fetched() {
    let mut raw = signature(); raw["signatures"][0]["documentation"] = json!({"kind":"markdown","value":"[external](https://invalid.example) $(not-a-command)"});
    assert_eq!(parse(&raw,16).unwrap()["signatures"][0]["documentation"],raw["signatures"][0]["documentation"]);
    for value in [json!(5),json!({"kind":"html","value":"x"}),json!({"kind":"markdown","value":5})] {
        raw["signatures"][0]["documentation"] = value;
        assert!(parse(&raw,16).is_err());
    }
}

#[test]
fn discarded_overloads_are_validated_before_returning_a_partial_list() {
    let raw = json!({"signatures":[{"label":"valid()"},{"label":"broken","parameters":[{"label":[0,999]}]}]});
    assert!(parse(&raw,1).is_err());
}

#[test]
fn report_signature_and_parameter_working_sets_are_bounded() {
    assert!(parse(&json!({"signatures":vec![json!({"label":"f()"});MAX_SIGNATURES+1]}),1).is_err());
    assert!(parse(&json!({"signatures":[{"label":"f()","parameters":vec![json!({"label":"x"});MAX_PARAMETERS+1]}]}),1).is_err());
    assert!(parse(&json!({"signatures":[{"label":"x".repeat(MAX_LABEL_BYTES+1)}]}),1).is_err());
    assert!(parse(&json!({"signatures":[],"unknown":"x".repeat(MAX_RESPONSE_BYTES)}),1).is_err());
}
