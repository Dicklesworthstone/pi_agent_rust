use super::*;

mod protocol;

#[test]
fn wire_arrays_null_and_completion_lists_are_distinct_valid_results() {
    assert_eq!(list_parts(&Value::Null).unwrap().0.len(),0);
    assert_eq!(list_parts(&json!([{"label":"x"}])).unwrap().0.len(),1);
    let raw=json!({"isIncomplete":true,"items":[{"label":"x"}],"itemDefaults":{"data":null}});
    let (items,defaults,incomplete)=list_parts(&raw).unwrap();
    assert_eq!(items.len(),1);
    assert!(defaults.is_some());
    assert!(incomplete);
}

#[test]
fn malformed_and_oversized_results_are_not_successful_empty_lists() {
    for raw in [json!(1),json!({}),json!({"items":[]}),json!({"isIncomplete":false,"items":null}),
        json!([{}]),json!([{"label":""}]),json!([{"label":"x","sortText":42}]),
        json!({"isIncomplete":false,"items":[],"itemDefaults":false}),
        json!({"isIncomplete":false,"items":[],"applyKind":{}})] {
        assert!(list_parts(&raw).is_err(),"{raw}");
    }
    assert!(list_parts(&json!(vec![json!({"label":"x"});MAX_SERVER_ITEMS+1])).is_err());
    assert!(list_parts(&json!({"isIncomplete":false,"items":[],"junk":"x".repeat(MAX_RESPONSE_BYTES)})).is_err());
}

#[test]
fn cache_generations_never_reuse_an_old_listing_identity() {
    let cache=CompletionCache::default();
    assert_eq!(cache.start().unwrap(),1);
    cache.clear();
    assert_eq!(cache.start().unwrap(),2);
    lock(&cache.0).serial=u64::MAX;
    assert!(cache.start().is_err());
    assert!(lock(&cache.0).items.is_empty());
}

#[test]
fn listing_and_selection_inputs_do_not_silently_ignore_conflicting_selectors() {
    for raw in [json!({"action":"completion"}),
        json!({"action":"completion","file":"a.rs","position":{"line":0,"character":0},"apply":true}),
        json!({"action":"completion","file":"a.rs","position":{"line":0,"character":0},"limit":0}),
        json!({"action":"completion","completionId":"id","file":"a.rs"}),
        json!({"action":"completion","completionId":"id","range":{"start":{"line":0,"character":0},"end":{"line":0,"character":0}}}),
        json!({"action":"completion","completionId":"id","symbol":"x"}),
        json!({"action":"completion","completionId":"id","actionId":"action"})] {
        assert!(validate_input(&serde_json::from_value(raw).unwrap()).is_err());
    }
    for raw in [json!({"action":"completion","file":"a.rs","position":{"line":0,"character":0}}),
        json!({"action":"completion","completionId":"id","apply":true})] {
        validate_input(&serde_json::from_value(raw).unwrap()).unwrap();
    }
}

#[test]
fn source_admission_is_bounded_and_preserves_exact_utf8() {
    let temp=tempfile::tempdir().unwrap();
    let path=temp.path().join("source");
    std::fs::write(&path,"😀\r\nhello").unwrap();
    assert_eq!(read_source(&path).unwrap(),"😀\r\nhello");
    std::fs::write(&path,[255,254]).unwrap();
    assert!(read_source(&path).is_err());
    std::fs::write(&path,vec![b'x';MAX_SOURCE_BYTES+1]).unwrap();
    assert!(read_source(&path).is_err());
    assert!(read_source(temp.path()).is_err());
}
