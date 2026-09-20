use super::*;

fn position(character: u32) -> Position {
    Position { line: 0, character }
}
fn range(start: u32, end: u32) -> Value {
    json!({"start":position(start),"end":position(end)})
}
fn edit(start: u32, end: u32, text: &str) -> Value {
    json!({"range":range(start, end),"newText":text})
}
fn completion() -> Value {
    json!({"label":"alpha","textEdit":edit(0, 2, "alpha")})
}

#[test]
fn plain_primary_edit_uses_server_range_not_caller_fallback() {
    let edits = edits(
        &completion(),
        "al rest",
        position(2),
        Some(Range {
            start: position(2),
            end: position(2),
        }),
    )
    .unwrap();
    assert_eq!(edits, vec![edit(0, 2, "alpha")]);
}

#[test]
fn omitted_text_edit_requires_explicit_language_independent_replacement() {
    let item = json!({"label":"label","insertText":"alpha"});
    assert!(
        edits(&item, "al", position(2), None)
            .unwrap_err()
            .to_string()
            .contains("RANGE_REQUIRED")
    );
    let fallback = Range {
        start: position(0),
        end: position(2),
    };
    assert_eq!(
        edits(&item, "al", position(2), Some(fallback)).unwrap(),
        vec![edit(0, 2, "alpha")]
    );
}

#[test]
fn default_edit_range_uses_text_edit_text_or_label_not_insert_text() {
    let defaults = json!({"editRange":range(0,2),"data":{"opaque":[1,"token"]}});
    let item = materialize(
        &json!({"label":"alpha","insertText":"wrong","textEditText":"right"}),
        Some(&defaults),
    )
    .unwrap();
    assert_eq!(item["textEdit"], edit(0, 2, "right"));
    assert_eq!(item["data"], defaults["data"]);
    let item = materialize(
        &json!({"label":"alpha","insertText":"wrong"}),
        Some(&defaults),
    )
    .unwrap();
    assert_eq!(item["textEdit"], edit(0, 2, "alpha"));
}

#[test]
fn explicit_item_properties_override_defaults_and_preserve_opaque_data() {
    let item = materialize(
        &json!({"label":"x","data":{"id":"item"},"insertTextFormat":1,"textEdit":edit(0,1,"x")}),
        Some(&json!({"editRange":range(0,2),"insertTextFormat":2,"data":{"id":"default"}})),
    )
    .unwrap();
    assert_eq!(item["data"], json!({"id":"item"}));
    assert_eq!(item["insertTextFormat"], 1);
    assert_eq!(item["textEdit"], edit(0, 1, "x"));
}

#[test]
fn insert_replace_edit_defaults_select_the_complete_replace_span() {
    let item = materialize(
        &json!({"label":"alpha"}),
        Some(&json!({"editRange":{"insert":range(0,2),"replace":range(0,4)}})),
    )
    .unwrap();
    assert_eq!(
        edits(&item, "alxx", position(2), None).unwrap(),
        vec![edit(0, 4, "alpha")]
    );
}

#[test]
fn invalid_insert_replace_relations_cannot_choose_an_unrelated_span() {
    for (insert, replace) in [
        (range(1, 2), range(0, 4)),
        (range(0, 4), range(0, 2)),
        (range(0, 1), range(0, 4)),
    ] {
        let item = json!({"label":"alpha","textEdit":{"insert":insert,"replace":replace,"newText":"alpha"}});
        assert!(edits(&item, "alxx", position(2), None).is_err());
    }
}

#[test]
fn main_edit_must_contain_cursor_and_stay_on_one_line() {
    assert!(edits(&completion(), "al rest", position(5), None).is_err());
    let item = json!({"label":"x","textEdit":{"range":{"start":position(0),"end":{"line":1,"character":1}},"newText":"x"}});
    assert!(edits(&item, "al\nx", position(1), None).is_err());
}

#[test]
fn unicode_and_crlf_boundaries_are_exact_not_clamped() {
    for invalid in [range(0, 1), range(0, 99), range(3, 2)] {
        let item = json!({"label":"x","textEdit":{"range":invalid,"newText":"x"}});
        assert!(edits(&item, "😀a\r\nb", position(2), None).is_err());
    }
    let valid = json!({"label":"x","textEdit":edit(2,3,"x")});
    assert!(edits(&valid, "😀a\r\nb", position(3), None).is_ok());
}

#[test]
fn auto_import_edits_are_preserved_in_original_document_coordinates() {
    let item = json!({"label":"Type","textEdit":{"range":{"start":{"line":1,"character":0},"end":{"line":1,"character":2}},"newText":"Type"},
        "additionalTextEdits":[edit(0,0,"use module::Type;\n")]});
    let got = edits(
        &item,
        "// comment\nTy",
        Position {
            line: 1,
            character: 2,
        },
        None,
    )
    .unwrap();
    assert_eq!(got.len(), 2);
    assert_eq!(got[1], edit(0, 0, "use module::Type;\n"));
}

#[test]
fn overlaps_including_duplicate_insert_points_are_rejected() {
    for extra in [
        vec![edit(1, 3, "bad")],
        vec![edit(0, 0, "bad")],
        vec![edit(4, 4, "one"), edit(4, 4, "two")],
    ] {
        let mut item = completion();
        item["additionalTextEdits"] = json!(extra);
        assert!(edits(&item, "al rest", position(2), None).is_err());
    }
}

#[test]
fn adjacent_nonoverlapping_edits_remain_supported() {
    let mut item = completion();
    item["additionalTextEdits"] = json!([edit(2, 3, "_")]);
    assert_eq!(edits(&item, "al rest", position(2), None).unwrap().len(), 2);
}

#[test]
fn annotated_or_resource_shaped_additional_edits_are_not_accepted() {
    for extra in [
        json!({"range":range(4,4),"newText":"x","uri":"file:///outside"}),
        json!({"kind":"delete","uri":"file:///outside"}),
        json!({"range":range(4,4),"newText":"x","annotationId":"confirm"}),
    ] {
        let mut item = completion();
        item["additionalTextEdits"] = json!([extra]);
        assert!(edits(&item, "al rest", position(2), None).is_err());
    }
}

#[test]
fn snippet_command_and_indentation_modes_fail_before_application() {
    for extra in [
        json!({"insertTextFormat":2}),
        json!({"insertTextMode":2}),
        json!({"command":{"command":"server.exec"}}),
    ] {
        let mut item = completion();
        item.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert!(
            edits(&item, "al", position(2), None)
                .unwrap_err()
                .to_string()
                .contains("UNSUPPORTED")
        );
    }
}

#[test]
fn lazy_resolve_can_add_auto_imports_and_documentation() {
    let original = completion();
    let mut after = original.clone();
    after["detail"] = json!("resolved type");
    after["documentation"] = json!({"kind":"markdown","value":"help"});
    after["additionalTextEdits"] = json!([edit(4, 4, "import")]);
    assert_eq!(resolved(&original, &after).unwrap(), after);
    assert_eq!(original, completion());
}

#[test]
fn lazy_resolve_cannot_substitute_insertion_identity_or_commands() {
    let original = completion();
    for (property, value) in [
        ("label", json!("other")),
        ("textEdit", edit(0, 2, "other")),
        ("filterText", json!("other")),
        ("data", json!({"id":"other"})),
        ("command", json!({"command":"server.exec"})),
    ] {
        let mut after = original.clone();
        after[property] = value;
        assert!(resolved(&original, &after).is_err(), "{property}");
    }
    let mut after = original.clone();
    after.as_object_mut().unwrap().remove("textEdit");
    assert!(resolved(&original, &after).is_err());
}

#[test]
fn additional_edits_and_wire_items_have_admission_bounds() {
    let mut item = completion();
    item["additionalTextEdits"] = json!(vec![edit(4, 4, "x"); 129]);
    assert!(
        edits(&item, "al rest", position(2), None)
            .unwrap_err()
            .to_string()
            .contains("LIMIT")
    );
    assert!(
        materialize(
            &json!({"label":"x","data":"x".repeat(MAX_ITEM_BYTES)}),
            None
        )
        .is_err()
    );
    assert!(
        materialize(
            &json!({"label":"x"}),
            Some(&json!({"commitCharacters":["."]}))
        )
        .is_err()
    );
}
