#![allow(clippy::literal_string_with_formatting_args, clippy::format_collect)]

use super::*;

fn item(text: &str) -> Value {
    json!({"label":"Type","insertTextFormat":2,"insertText":text})
}

fn values(pairs: &[(&str, &str)]) -> Values {
    pairs
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

fn expansion(text: &str, supplied: &[(&str, &str)]) -> Expanded {
    prepare(&item(text), Some(&values(supplied))).unwrap()
}

#[test]
fn numbered_defaults_mirrors_and_final_cursor_expand_without_markers() {
    let expanded = expansion("call(${1:argument}, $1)$0", &[]);
    assert_eq!(expanded.item["insertText"], "call(argument, argument)");
    assert_eq!(expanded.item["insertTextFormat"], 1);
    assert!(expanded.missing.is_empty());
    expanded.require_values().unwrap();
}

#[test]
fn substitutions_are_literal_including_snippet_and_shell_syntax() {
    let literal = "${2:no} \\ $CLIPBOARD $(touch nope) 😀\nline";
    let expanded = expansion("call(${1:default}, $1)$0", &[("1", literal)]);
    assert_eq!(
        expanded.item["insertText"],
        format!("call({literal}, {literal})")
    );
    assert!(expanded.missing.is_empty());
}

#[test]
fn forward_references_use_the_later_definition() {
    assert_eq!(
        expansion("$2 ${2:after} ${1:$2}", &[]).item["insertText"],
        "after after after"
    );
}

#[test]
fn nested_defaults_accept_inner_or_outer_overrides() {
    let text = "${1:outer ${2:inner}} / $2";
    assert_eq!(
        expansion(text, &[]).item["insertText"],
        "outer inner / inner"
    );
    assert_eq!(
        expansion(text, &[("2", "chosen")]).item["insertText"],
        "outer chosen / chosen"
    );
    assert_eq!(
        expansion(text, &[("1", "whole"), ("2", "chosen")]).item["insertText"],
        "whole / chosen"
    );
}

#[test]
fn suppressed_nested_fields_are_not_required_or_silently_consumed() {
    let text = "${1:outer $2}";
    let expanded = expansion(text, &[("1", "whole")]);
    assert!(expanded.missing.is_empty());
    assert_eq!(expanded.fields.as_array().unwrap().len(), 1);
    assert!(
        prepare(
            &item(text),
            Some(&values(&[("1", "whole"), ("2", "unused")]))
        )
        .is_err()
    );
}

#[test]
fn bare_positive_tabstops_require_explicit_values_but_zero_does_not() {
    let expanded = expansion("call($2, ${1})$0", &[]);
    assert_eq!(expanded.missing, vec![1, 2]);
    assert_eq!(expanded.item["insertText"], "call(, )");
    assert!(
        expanded
            .require_values()
            .unwrap_err()
            .to_string()
            .contains("VALUES_REQUIRED")
    );
    let filled = expansion("call($2, ${1})$0", &[("1", ""), ("2", "value")]);
    filled.require_values().unwrap();
    assert_eq!(filled.item["insertText"], "call(value, )");
}

#[test]
fn empty_defaults_and_final_placeholder_defaults_are_preserved() {
    let expanded = expansion("${1:} ${0:body}", &[]);
    assert!(expanded.missing.is_empty());
    assert_eq!(expanded.item["insertText"], " body");
    assert_eq!(
        expansion("${0:body}", &[("0", "changed")]).item["insertText"],
        "changed"
    );
}

#[test]
fn choices_expose_options_and_default_to_first_but_allow_literal_edits() {
    let expanded = expansion("${1|red,green,blue|}-$1", &[]);
    assert_eq!(expanded.item["insertText"], "red-red");
    assert_eq!(
        expanded.fields[0]["choices"],
        json!(["red", "green", "blue"])
    );
    assert_eq!(
        expansion("${1|red,green|}", &[("1", "custom")]).item["insertText"],
        "custom"
    );
}

#[test]
fn context_sensitive_escapes_preserve_unicode_and_literal_dollars() {
    assert_eq!(
        expansion(r"\$1 \} \\ 😀", &[]).item["insertText"],
        "$1 } \\ 😀"
    );
    let expanded = expansion(r"${1|a\,b,c\|d,e\\f,$0|}", &[]);
    assert_eq!(
        expanded.fields[0]["choices"],
        json!(["a,b", "c|d", "e\\f", "$0"])
    );
    assert_eq!(expanded.item["insertText"], "a,b");
}

#[test]
fn ordinary_code_braces_and_multiline_text_survive() {
    assert_eq!(
        expansion("fn x() {\r\n  ${1:body}\r\n}$0", &[]).item["insertText"],
        "fn x() {\r\n  body\r\n}"
    );
}

#[test]
fn contradictory_defaults_and_cycles_fail_closed() {
    for text in [
        "${1:a}${1:b}",
        "${1:$1}",
        "${1:$2}${2:$1}",
        "${1|a,b|}${1:a}",
    ] {
        assert!(prepare(&item(text), None).is_err(), "{text}");
    }
    assert_eq!(expansion("${1:a}${1:a}", &[]).item["insertText"], "aa");
}

#[test]
fn unsupported_transforms_are_rejected_even_in_overridden_defaults() {
    for text in [
        "${1/(?=a)/$1/}",
        "${1:${TM_FILENAME/(?=a)/$1/}}",
        "${1:${2/(?=a)/$1/}}",
    ] {
        let error = prepare(&item(text), Some(&values(&[("1", "override")])))
            .err()
            .unwrap();
        assert!(error.to_string().contains("UNSUPPORTED"), "{text}: {error}");
    }
}

#[test]
fn malformed_delimiters_and_invalid_escapes_are_not_inserted() {
    for text in [
        "${1:unfinished",
        "${1|a,b",
        "${1|a|x",
        "${1!}",
        r"\q",
        r"${1|\$|}",
        "$",
        "${}",
    ] {
        assert!(prepare(&item(text), None).is_err(), "{text}");
    }
}

#[test]
fn default_range_materialization_keeps_snippet_format_and_data() {
    let raw = json!({"label":"Type","textEditText":"Type(${1:arg})$0"});
    let range = json!({"start":{"line":0,"character":0},"end":{"line":0,"character":2}});
    let normalized = crate::lsp::completion::item::materialize(
        &raw,
        Some(&json!({
            "editRange":range,"insertTextFormat":2,"data":{"opaque":[1,"id"]}
        })),
    )
    .unwrap();
    let expanded = prepare(&normalized, None).unwrap();
    assert_eq!(expanded.item["textEdit"]["newText"], "Type(arg)");
    assert_eq!(expanded.item["textEdit"]["range"], range);
    assert_eq!(expanded.item["data"], normalized["data"]);
    assert_eq!(normalized["insertTextFormat"], 2);
}

#[test]
fn only_primary_text_is_expanded_and_the_cached_item_is_unchanged() {
    let raw = json!({"label":"Type","insertTextFormat":2,
        "textEdit":{"insert":{},"replace":{},"newText":"Type(${1:arg})$0"},
        "additionalTextEdits":[{"range":{},"newText":"// literal $0 ${1:keep}\n"}]});
    let expanded = prepare(&raw, Some(&values(&[("1", "value")]))).unwrap();
    assert_eq!(expanded.item["textEdit"]["newText"], "Type(value)");
    assert_eq!(
        expanded.item["additionalTextEdits"],
        raw["additionalTextEdits"]
    );
    assert_eq!(raw["textEdit"]["newText"], "Type(${1:arg})$0");
    assert_eq!(
        prepare(&raw, None).unwrap().item["textEdit"]["newText"],
        "Type(arg)"
    );
}

#[test]
fn caller_keys_and_sizes_are_validated_before_expansion() {
    for key in ["01", "-1", "1.0", "variable.name", "65536", ""] {
        assert!(validate_values(&values(&[(key, "x")])).is_err());
    }
    assert!(prepare(&item("$1"), Some(&values(&[("2", "unknown")]))).is_err());
    assert!(validate_values(&values(&[("1", &"x".repeat(16 * 1024 + 1))])).is_err());
    assert!(validate_values(&(0..65).map(|id| (id.to_string(), String::new())).collect()).is_err());
    assert!(
        validate_values(
            &(0..5)
                .map(|id| (id.to_string(), "x".repeat(16 * 1024)))
                .collect()
        )
        .is_err()
    );
}

#[test]
fn nesting_nodes_choices_and_expansion_are_all_bounded() {
    let nested = format!(
        "{}x{}",
        "${1:".repeat(MAX_DEPTH + 1),
        "}".repeat(MAX_DEPTH + 1)
    );
    let fields = (0..65).map(|id| format!("${id} ")).collect::<String>();
    let choices = format!("${{1|{}|}}", vec!["x"; 33].join(","));
    for text in [
        nested,
        fields,
        choices,
        "$0 ".repeat(MAX_NODES),
        "x".repeat(MAX_TEXT + 1),
    ] {
        assert!(prepare(&item(&text), None).is_err());
    }
    let text = "$1".repeat(5);
    assert!(
        prepare(
            &item(&text),
            Some(&values(&[("1", &"x".repeat(16 * 1024))]))
        )
        .is_err()
    );
}

#[test]
fn substitutions_cannot_bypass_command_or_indentation_rejection() {
    use crate::lsp::text::{Position, Range};
    let cursor = Position {
        line: 0,
        character: 0,
    };
    for extra in [
        json!({"command":{"command":"unsafe.run"}}),
        json!({"insertTextMode":2}),
    ] {
        let mut raw = item("${1:value}");
        raw.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let expanded = prepare(&raw, None).unwrap();
        assert!(
            crate::lsp::completion::item::edits(
                &expanded.item,
                "",
                cursor,
                Some(Range {
                    start: cursor,
                    end: cursor
                })
            )
            .is_err()
        );
    }
}

#[test]
fn plain_items_reject_values_and_never_interpret_their_text() {
    let raw = json!({"label":"$CLIPBOARD", "insertText":"${1:literal}"});
    let expanded = prepare(&raw, None).unwrap();
    assert_eq!(expanded.item, raw);
    assert!(expanded.fields.is_null());
    assert!(prepare(&raw, Some(&Values::new())).is_err());
}

#[test]
fn named_values_mirror_exactly_and_never_evaluate_their_contents() {
    let literal = "${OTHER:ignored} $CLIPBOARD $(not_a_command) \\ 😀\nline";
    let expanded = expansion("$NAME / ${NAME} / ${1:$NAME}", &[("NAME", literal)]);
    assert_eq!(expanded.item["insertText"], format!("{literal} / {literal} / {literal}"));
    assert_eq!(expanded.variables, json!([{"name":"NAME","missing":false}]));
    expanded.require_values().unwrap();
}

#[test]
fn variable_defaults_are_occurrence_local_and_may_contain_numbered_fields() {
    let expanded = expansion("${NAME:a} ${NAME:b} ${OTHER:${1:inner}} $1", &[]);
    assert_eq!(expanded.item["insertText"], "a b inner inner");
    expanded.require_values().unwrap();
    let expanded = expansion("${NAME:a} ${NAME:b}", &[("NAME", "")]);
    assert_eq!(expanded.item["insertText"], " ");
    expanded.require_values().unwrap();
    // A later variable default is not a definition for an earlier occurrence.
    let expanded = expansion("$NAME ${NAME:later}", &[]);
    assert_eq!(expanded.item["insertText"], "NAME later");
    assert_eq!(expanded.missing_variables, vec!["NAME"]);
    assert!(expanded.require_values().is_err());
}

#[test]
fn unknown_variables_are_required_fields_not_environment_or_clipboard_lookups() {
    // PATH exists on ordinary test hosts; it still must not be consulted.
    // No environment mutation is needed to make this a privacy negative control.
    for name in ["PATH", "HOME", "CLIPBOARD", "CURRENT_YEAR", "CUSTOM_SECRET"] {
        let raw = item(&format!("${name}"));
        let expanded = prepare(&raw, None).unwrap();
        assert_eq!(expanded.item["insertText"], name);
        assert_eq!(expanded.missing_variables, vec![name]);
        assert!(expanded.require_values().unwrap_err().to_string().contains("VALUES_REQUIRED"));
        let filled = prepare(&raw, Some(&values(&[(name, "")]))).unwrap();
        filled.require_values().unwrap();
        assert_eq!(filled.item["insertText"], "");
    }
    assert_eq!(expansion("${CLIPBOARD:local default}", &[]).item["insertText"], "local default");
}

#[test]
fn numbered_and_named_missing_fields_both_block_apply() {
    let expanded = expansion("call($1, $Z, $A, $Z)$0", &[]);
    assert_eq!(expanded.missing, vec![1]);
    assert_eq!(expanded.missing_variables, vec!["A", "Z"]);
    assert!(expanded.require_values().is_err());
    let still_missing = expansion("call($1, $Z)", &[("1", "first")]);
    assert!(still_missing.missing.is_empty());
    assert!(still_missing.require_values().is_err());
    expansion("call($1, $Z)", &[("1", "first"), ("Z", "last")])
        .require_values()
        .unwrap();
}

#[test]
fn variable_overrides_suppress_nested_fields_without_consuming_extra_values() {
    let text = "${OUTER:${INNER:$1}}";
    let expanded = expansion(text, &[("OUTER", "whole")]);
    expanded.require_values().unwrap();
    assert!(expanded.fields.as_array().unwrap().is_empty());
    assert_eq!(expanded.variables, json!([{"name":"OUTER","missing":false}]));
    for extra in [("INNER", "unused"), ("1", "unused")] {
        assert!(prepare(&item(text), Some(&values(&[("OUTER", "whole"), extra]))).is_err());
    }
    let expanded = expansion("${1:$NAME}", &[("1", "whole")]);
    expanded.require_values().unwrap();
    assert!(expanded.variables.as_array().unwrap().is_empty());
}

#[test]
fn variable_transforms_share_placeholder_semantics_without_mutating_mirrors() {
    let text = r"${NAME/(.*)\..+$/$1/} / $NAME / ${NAME/(.*)/${1:/upcase}/}";
    let expanded = expansion(text, &[("NAME", "file.rs")]);
    assert_eq!(expanded.item["insertText"], "file / file.rs / FILE.RS");
    expanded.require_values().unwrap();
    let missing = expansion("${NAME/(.*)/constant/}", &[]);
    assert_eq!(missing.item["insertText"], "constant");
    assert!(missing.require_values().is_err(), "constant replacement must not hide an unresolved source");
    assert!(prepare(&item(text), Some(&values(&[("NAME", "😀.rs")]))).is_err());
}

#[test]
fn variable_and_placeholder_admission_share_limits() {
    let exact = (0..32).map(|id| format!("${id} $V{id} ")).collect::<String>();
    assert!(prepare(&item(&exact), None).is_ok());
    assert!(prepare(&item(&format!("{exact}$EXTRA")), None).is_err());
    assert!(prepare(&item(&format!("${}", "N".repeat(MAX_VARIABLE_NAME + 1))), None).is_err());
    let transforms = "${V/x/y/}".repeat(transform::MAX_TRANSFORMS);
    assert!(prepare(&item(&transforms), Some(&values(&[("V", "x")]))).is_ok());
    assert!(prepare(&item(&format!("{transforms}${{1/x/y/}}")), None).is_err());
    let value = "x".repeat(16 * 1024);
    assert!(prepare(&item("$NAME$NAME$NAME$NAME$NAME"), Some(&values(&[("NAME", &value)]))).is_err());
}

#[test]
fn variable_names_and_delimiters_are_validated_even_when_defaults_are_overridden() {
    for key in ["name", "_", "_A9", "TM_FILENAME", "0", "65535"] {
        validate_values(&values(&[(key, "value")])).unwrap();
    }
    for key in ["a.b", "a-b", "9name", "é", " name", "${NAME}"] {
        assert!(validate_values(&values(&[(key, "value")])).is_err(), "{key}");
    }
    for text in ["${NAME|a,b|}", "${NAME", "${NAME!}", "${1:${NAME!}}"] {
        assert!(prepare(&item(text), Some(&values(&[("1", "override")]))).is_err(), "{text}");
    }
    assert!(prepare(&item("$NAME"), Some(&values(&[("name", "wrong case")]))).is_err());
    assert!(prepare(&item("$1"), Some(&values(&[("NAME", "unknown")]))).is_err());
}

#[test]
fn named_values_do_not_mutate_cached_defaults_or_expand_auto_imports() {
    let raw = json!({"label":"Type","insertTextFormat":2,
        "textEdit":{"range":{},"newText":"${NAME:default}"},
        "additionalTextEdits":[{"range":{},"newText":"// $NAME ${NAME:literal}\n"}]});
    let original = raw.clone();
    let expanded = prepare(&raw, Some(&values(&[("NAME", "chosen")]))).unwrap();
    assert_eq!(expanded.item["textEdit"]["newText"], "chosen");
    assert_eq!(expanded.item["additionalTextEdits"], raw["additionalTextEdits"]);
    assert_eq!(raw, original);
    assert_eq!(prepare(&raw, None).unwrap().item["textEdit"]["newText"], "default");
}
