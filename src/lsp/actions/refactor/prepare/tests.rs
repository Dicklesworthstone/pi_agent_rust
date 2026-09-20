use super::*;

fn position(line: u32, character: u32) -> Position {
    Position { line, character }
}

fn range(start: u32, end: u32) -> Value {
    json!({"start":{"line":0,"character":start},"end":{"line":0,"character":end}})
}

#[test]
fn concrete_ranges_preserve_unicode_text_and_cursor_at_token_end() {
    for cursor in [2, 4, 7] {
        let target = parse(&range(2, 7), "😀alpha\r\nbeta", position(0, cursor))
            .unwrap()
            .unwrap();
        assert_eq!(target.source_text, "alpha");
        assert_eq!(target.placeholder, "alpha");
        assert_eq!(target.range.start, position(0, 2));
    }
}

#[test]
fn server_placeholder_is_distinct_from_the_original_source_spelling() {
    let raw = json!({"range":range(0, 6),"placeholder":"name"});
    let target = parse(&raw, "`name`", position(0, 3)).unwrap().unwrap();
    assert_eq!(target.source_text, "`name`");
    assert_eq!(target.placeholder, "name");
}

#[test]
fn multiline_ranges_keep_original_line_terminators() {
    let raw = json!({"start":{"line":0,"character":2},"end":{"line":1,"character":4}});
    let target = parse(&raw, "😀alpha\r\nbeta\n", position(1, 2))
        .unwrap()
        .unwrap();
    assert_eq!(target.source_text, "alpha\r\nbeta");
}

#[test]
fn null_preparation_is_an_explicit_refusal() {
    assert!(
        parse(&Value::Null, "alpha", position(0, 2))
            .unwrap()
            .is_none()
    );
}

#[test]
fn malformed_or_unnegotiated_preparation_never_becomes_a_range() {
    for raw in [
        json!(false),
        json!([]),
        json!({}),
        json!({"defaultBehavior":true}),
        json!({"defaultBehavior":false}),
        json!({"range":range(0, 5)}),
        json!({"range":range(0, 5),"placeholder":7}),
        json!({"placeholder":"alpha","start":{"line":0,"character":0},"end":{"line":0,"character":5}}),
        json!({"range":range(0, 5),"placeholder":"alpha","start":{"line":0,"character":0}}),
    ] {
        assert!(parse(&raw, "alpha", position(0, 2)).is_err(), "{raw}");
    }
}

#[test]
fn nonexact_empty_reversed_and_unrelated_ranges_are_rejected() {
    for raw in [
        range(1, 7),
        range(2, 99),
        range(4, 4),
        range(7, 2),
        range(5, 7),
        json!({"start":{"line":0,"character":2},"end":{"line":9,"character":0}}),
    ] {
        assert!(parse(&raw, "😀alpha\n", position(0, 3)).is_err(), "{raw}");
    }
}

#[test]
fn preparation_bounds_both_placeholder_and_original_text() {
    let raw = json!({"range":range(0, 5),"placeholder":"x".repeat(MAX_TARGET_BYTES + 1)});
    assert!(
        parse(&raw, "alpha", position(0, 3))
            .unwrap_err()
            .to_string()
            .contains("LSP_RENAME_LIMIT")
    );
    let text = "x".repeat(MAX_TARGET_BYTES + 1);
    assert!(
        parse(
            &range(0, u32::try_from(text.len()).unwrap()),
            &text,
            position(0, 0)
        )
        .is_err()
    );
    let raw = json!({"extra":"x".repeat(super::super::MAX_ACTION_BYTES + 1)});
    assert!(
        parse(&raw, "alpha", position(0, 1))
            .unwrap_err()
            .to_string()
            .contains("LSP_EDIT_LIMIT")
    );
}

#[test]
fn preparation_requires_an_explicit_valid_capability() {
    assert!(supports_prepare(&json!({"renameProvider":{"prepareProvider":true}})).unwrap());
    for caps in [
        json!({}),
        json!({"renameProvider":true}),
        json!({"renameProvider":{}}),
        json!({"renameProvider":{"prepareProvider":false}}),
    ] {
        assert!(!supports_prepare(&caps).unwrap());
    }
    for caps in [
        json!({"renameProvider":false}),
        json!({"renameProvider":1}),
        json!({"renameProvider":{"prepareProvider":"true"}}),
        json!({"renameProvider":{"prepareProvider":true},"positionEncoding":"utf-8"}),
    ] {
        assert!(supports_prepare(&caps).is_err());
    }
}

#[test]
fn targeting_accepts_one_position_selector_without_guessing() {
    for action in ["rename", "prepare_rename"] {
        for selector in [
            json!({"position":{"line":0,"character":5}}),
            json!({"symbol":"alpha#2"}),
            json!({"symbol":"alpha","line":1}),
        ] {
            let mut raw = json!({"action":action,"file":"a.rs"});
            raw.as_object_mut()
                .unwrap()
                .extend(selector.as_object().unwrap().clone());
            if action == "rename" {
                raw["newName"] = json!("renamed");
            }
            validate_input(&serde_json::from_value(raw).unwrap()).unwrap();
        }
    }
    for extra in [
        json!({}),
        json!({"line":1}),
        json!({"symbol":""}),
        json!({"position":{"line":0,"character":1},"symbol":"alpha"}),
        json!({"position":{"line":0,"character":1},"line":1}),
    ] {
        let mut raw = json!({"action":"prepare_rename","file":"a.rs"});
        raw.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert!(validate_input(&serde_json::from_value(raw).unwrap()).is_err());
    }
}

#[test]
fn inspection_cannot_accept_mutation_or_unrelated_selectors() {
    for (key, value) in [
        ("apply", json!(false)),
        ("apply", json!(true)),
        ("newName", json!("renamed")),
        ("query", json!("1")),
        ("actionId", json!("a")),
        ("refactorId", json!("r")),
        ("newFile", json!("b.rs")),
        ("method", json!("workspace/executeCommand")),
        ("payload", json!({})),
        ("range", range(0, 5)),
        ("only", json!(["refactor"])),
        ("completionId", json!("c")),
        ("hierarchyId", json!("h")),
        ("snippetValues", json!({})),
        ("resolve", json!(false)),
        ("limit", json!(1)),
        ("after", json!("x")),
        ("formatOptions", json!({})),
    ] {
        let mut raw = json!({"action":"prepare_rename","file":"a.rs","symbol":"alpha"});
        raw[key] = value;
        assert!(
            validate_input(&serde_json::from_value(raw).unwrap()).is_err(),
            "{key}"
        );
    }
}

#[test]
fn sources_are_bounded_before_reading_and_special_files_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    assert!(read_source(temp.path()).is_err());
    let large = temp.path().join("large.rs");
    std::fs::File::create(&large)
        .unwrap()
        .set_len(MAX_SOURCE_BYTES as u64 + 1)
        .unwrap();
    assert!(
        read_source(&large)
            .unwrap_err()
            .to_string()
            .contains("LSP_RENAME_LIMIT")
    );
    let source = temp.path().join("a.rs");
    std::fs::write(&source, "😀alpha\r\n").unwrap();
    assert_eq!(read_source(&source).unwrap(), "😀alpha\r\n");
    #[cfg(unix)]
    {
        let link = temp.path().join("link.rs");
        std::os::unix::fs::symlink(&source, &link).unwrap();
        assert!(read_source(&link).is_err());
    }
}

#[test]
fn request_budget_and_cancellation_do_not_reset_between_phases() {
    let mut budget = Budget::new(Duration::from_secs(2));
    budget.started = Instant::now().checked_sub(Duration::from_secs(3)).unwrap();
    assert!(
        budget
            .remaining()
            .unwrap_err()
            .to_string()
            .contains("LSP_TIMEOUT")
    );
    let mut budget = Budget::new(Duration::from_secs(20));
    budget.owner = AgentCx::for_request();
    budget
        .owner
        .cancel_with(asupersync::types::CancelKind::User, Some("cancel rename"));
    assert!(
        budget
            .remaining()
            .unwrap_err()
            .to_string()
            .contains("LSP_CANCELLED")
    );
}

mod protocol;
