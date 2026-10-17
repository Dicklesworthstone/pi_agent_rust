#![allow(clippy::literal_string_with_formatting_args)]

use super::*;
use crate::lsp::completion::snippet::{Expanded, Values, prepare};
use serde_json::json;

fn expand(text: &str, supplied: &[(&str, &str)]) -> Result<Expanded> {
    let values: Values = supplied
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
    prepare(
        &json!({"label":"Type", "insertTextFormat":2, "insertText":text}),
        Some(&values),
    )
}

fn rendered(text: &str, supplied: &[(&str, &str)]) -> String {
    let result = expand(text, supplied).unwrap();
    result.require_values().unwrap();
    result.item["insertText"].as_str().unwrap().to_string()
}

fn transform(text: &str) -> Transform {
    let mut offset = 0;
    let result = Transform::parse(text, &mut offset).unwrap();
    assert_eq!(offset, text.len());
    result
}

fn apply(text: &str, value: &str) -> String {
    let mut work = super::super::MAX_WORK;
    let mut scan = SCAN_BUDGET;
    transform(text).apply(value, &mut work, &mut scan).unwrap()
}

#[test]
fn transforms_use_source_values_without_changing_other_mirrors() {
    assert_eq!(
        rendered(
            "${1:my_name} ${1/(.*)/${1:/pascalcase}/} $1 ${1/(.*)/${1:/upcase}/}",
            &[]
        ),
        "my_name MyName my_name MY_NAME"
    );
}

#[test]
fn transform_before_definition_resolves_forward_reference() {
    assert_eq!(
        rendered("${2/(.*)/${1:/upcase}/} ${2:later}", &[]),
        "LATER later"
    );
}

#[test]
fn explicit_values_and_generated_text_are_never_reparsed_as_snippets() {
    assert_eq!(
        rendered(
            "${1/(.*)/$1/}|$1",
            &[("1", "${2:literal} $(not_a_command)")]
        ),
        "${2:literal} $(not_a_command)|${2:literal} $(not_a_command)"
    );
    assert_eq!(
        rendered(r"${1:word} ${1/(.*)/\$CLIPBOARD/}", &[]),
        "word $CLIPBOARD"
    );
}

#[test]
fn first_and_global_replacements_preserve_unmatched_segments() {
    assert_eq!(
        apply("[.-]/_/}", "example-123.456-TEST.js"),
        "example_123.456-TEST.js"
    );
    assert_eq!(
        apply("[.-]/_/g}", "example-123.456-TEST.js"),
        "example_123_456_TEST_js"
    );
    assert_eq!(apply("a/X/gi}", "AbA"), "XbX");
    assert_eq!(apply("x/Y/}", "unchanged"), "unchanged");
}

#[test]
fn five_case_modifiers_preserve_reference_word_semantics() {
    assert_eq!(apply("(.*)/${1:/upcase}/}", "foo_BAR"), "FOO_BAR");
    assert_eq!(apply("(.*)/${1:/downcase}/}", "foo_BAR"), "foo_bar");
    assert_eq!(apply("(.*)/${1:/capitalize}/}", "foo_BAR"), "Foo_BAR");
    assert_eq!(apply("(.*)/${1:/pascalcase}/}", "foo_BAR-baz"), "FooBARBaz");
    assert_eq!(apply("(.*)/${1:/camelcase}/}", "Foo_BAR-baz"), "fooBARBaz");
    assert_eq!(apply("(.*)/${1:/pascalcase}/}", "---"), "---");
}

#[test]
fn capture_formats_and_all_conditional_forms_are_supported() {
    assert_eq!(apply("(a)?(b)/$0-${1}-${2}-${3}/}", "b"), "b--b-");
    assert_eq!(
        apply(
            "(a)?(b)/${1:+yes}|${1:-no}|${2:?yes:no}|${3:fallback}/}",
            "b"
        ),
        "|no|yes|fallback"
    );
    assert_eq!(
        apply("(a)/${1:-no}|${1:default}|${1:?yes:no}/}", "a"),
        "a|a|yes"
    );
}

#[test]
fn no_match_can_use_explicit_nonempty_else_branch() {
    assert_eq!(apply("(x)/prefix-${1:-absent}/}", "value"), "prefix-absent");
    assert_eq!(apply("(x)/${1:?present:absent}/}", "value"), "absent");
    assert_eq!(apply("(x)/${1:+present}/}", "value"), "value");
    assert_eq!(apply("(x)/${1:-}/}", "value"), "value");
}

#[test]
fn empty_capture_is_false_and_empty_global_matches_are_not_skipped() {
    assert_eq!(apply("()/[${1:?yes:no}]/}", "a"), "[no]a");
    assert_eq!(apply("a*/X/g}", "a"), "XX");
    assert_eq!(apply("()/X/g}", "ab"), "XaXbX");
    assert_eq!(apply("$/X/g}", "ab"), "abX");
}

#[test]
fn escaped_slashes_dollars_braces_and_backslashes_remain_literal() {
    assert_eq!(apply(r"a\/b/\$1\/\}\\/}", "a/b"), "$1/}\\");
    assert_eq!(apply(r"(a)/${1:?yes\:ok:no\}ok}/}", "a"), "yes:ok");
}

#[test]
fn unsupported_regex_dialects_flags_and_value_semantics_fail_closed() {
    for text in [
        r"(a)\1/x/}",
        "(?=a)/x/}",
        "(?<=a)b/x/}",
        "(?i)a/x/}",
        "[a&&b]/x/}",
        "[a--b]/x/}",
        "[a~~b]/x/}",
        "[[a]]/x/}",
        r"\p{L}/x/}",
        r"\u0061/x/}",
        r"\A/x/}",
        "(a(b)?)+/x/}",
        "a/x/m}",
        "a/x/s}",
        "a/x/u}",
        "a/x/y}",
        "a/x/gg}",
        "a/x/ii}",
        "(.*)/${1:/unknown}/}",
        "(.*)/${1:+${2:ambiguous}}/}",
    ] {
        let mut offset = 0;
        let error = Transform::parse(text, &mut offset).unwrap_err();
        assert!(error.to_string().contains("UNSUPPORTED"), "{text}: {error}");
        assert_eq!(
            offset, 0,
            "a failed parse must not advance its caller's cursor"
        );
    }
    let parsed = transform("(.*)/${1:/upcase}/}");
    for value in ["caf\u{e9}", "a\nb", "a\rb", "\u{1f980}"] {
        let mut scan = SCAN_BUDGET;
        assert!(parsed.apply(value, &mut 1000, &mut scan).is_err());
    }
}

#[test]
fn invalid_formats_and_delimiters_are_never_inserted() {
    for text in [
        "a",
        "a/b",
        "a/b/",
        "a/$/}",
        "a/${1!/}",
        "a/${1:?missing}/}",
        r"a/\q/}",
    ] {
        let mut offset = 0;
        assert!(Transform::parse(text, &mut offset).is_err(), "{text}");
        assert_eq!(offset, 0);
    }
}

#[test]
fn numeric_transform_still_requires_missing_source_values() {
    let expanded = expand("Type(${1/(.*)/${1:/upcase}/})$0", &[]).unwrap();
    assert_eq!(expanded.missing, vec![1]);
    assert!(expanded.require_values().is_err());
    assert_eq!(
        rendered("Type(${1/(.*)/${1:/upcase}/})$0", &[("1", "value")]),
        "Type(VALUE)"
    );
}

#[test]
fn nested_transform_defaults_share_cycle_detection_and_override_rules() {
    assert_eq!(
        rendered("${1:${2/(.*)/${1:/upcase}/}} ${2:word}", &[]),
        "WORD word"
    );
    assert_eq!(
        rendered("${1:${2/(.*)/${1:/upcase}/}}", &[("1", "override")]),
        "override"
    );
    assert!(expand("${1:${1/(.*)/x/}}", &[]).is_err());
    assert!(expand("${1:${2/(.*)/x/}} ${2:$1}", &[]).is_err());
    assert!(expand("${1:${2/(?=a)/x/}}", &[("1", "override")]).is_err());
}

#[test]
fn equal_default_transforms_do_not_conflict_due_to_compiled_regex_identity() {
    assert_eq!(
        rendered("${1:${2/(.*)/x/}} ${1:${2/(.*)/x/}} ${2:a}", &[]),
        "x x a"
    );
}

#[test]
fn transforms_only_rewrite_primary_text_and_preserve_resolve_identity() {
    let raw = json!({"label":"Type", "insertTextFormat":2, "data":{"id":"opaque"},
        "textEdit":{"range":{}, "newText":"Type(${1/(.*)/${1:/upcase}/})"},
        "additionalTextEdits":[{"range":{},"newText":"// literal ${1/(.*)/$1/}\n"}]});
    let values = Values::from([("1".into(), "value".into())]);
    let expanded = prepare(&raw, Some(&values)).unwrap();
    assert_eq!(expanded.item["textEdit"]["newText"], "Type(VALUE)");
    assert_eq!(
        expanded.item["additionalTextEdits"],
        raw["additionalTextEdits"]
    );
    assert_eq!(expanded.item["data"], raw["data"]);
    assert_eq!(raw["insertTextFormat"], 2);
    assert_eq!(raw["textEdit"]["newText"], "Type(${1/(.*)/${1:/upcase}/})");
    let changed = Values::from([("1".into(), "changed".into())]);
    assert_eq!(
        prepare(&raw, Some(&changed)).unwrap().item["textEdit"]["newText"],
        "Type(CHANGED)"
    );
}

#[test]
fn pattern_transform_count_scan_work_and_output_limits_are_enforced() {
    let mut offset = 0;
    assert!(
        Transform::parse(
            &format!("{}/x/}}", "a".repeat(MAX_PATTERN + 1)),
            &mut offset
        )
        .is_err()
    );
    assert!(expand(&"${0/a/x/}".repeat(MAX_TRANSFORMS + 1), &[]).is_err());
    let parsed = transform("(a)/$1/}");
    let mut scan = SCAN_BUDGET;
    assert!(parsed.apply("a", &mut 0, &mut scan).is_err());
    assert!(parsed.apply("a", &mut 1000, &mut 0).is_err());
    let parsed = transform(&format!("a/{}/g}}", "x".repeat(100)));
    let mut work = super::super::MAX_WORK;
    let mut scan = SCAN_BUDGET;
    assert!(
        parsed
            .apply(&"a".repeat(1000), &mut work, &mut scan)
            .is_err()
    );
    let parts = "$1".repeat(MAX_FORMAT + 1);
    assert!(Transform::parse(&format!("(a)/{parts}/}}"), &mut 0).is_err());
}

#[test]
fn untransformed_unicode_and_literal_auto_imports_remain_supported() {
    assert_eq!(rendered("${1:na\u{ef}ve} $1", &[]), "na\u{ef}ve na\u{ef}ve");
    let raw = json!({"label":"Type", "insertText":"${1/(.*)/$1/}"});
    assert_eq!(prepare(&raw, None).unwrap().item, raw);
}

// Exercise the production public tool, the existing framed child peer, lazy
// resolution and the real file transaction rather than simulating insertion.
mod public_tool {
    use crate::lsp::LspTool;
    use crate::tools::Tool as _;
    use serde_json::{Value, json};
    use std::path::Path;
    use std::process::{Command, Stdio};

    const SOURCE: &str = "// header\nfn main() { Ty }\n";
    const IMPORT: &str = "use example::Type;\n// literal $0 ${1:keep}\n";
    const PEER: &str = include_str!("../../tests/server.py");

    fn fixture(root: &Path) -> Option<(LspTool, asupersync::runtime::Runtime)> {
        let python = ["python3", "python"].into_iter().find(|program| {
            Command::new(program)
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        });
        let Some(python) = python else {
            assert!(
                std::env::var_os("PI_LSP_REQUIRE_PROTOCOL").is_none(),
                "completion protocol peer requires Python"
            );
            eprintln!("SKIP completion transform protocol fixture: Python unavailable");
            return None;
        };
        let root = root.canonicalize().unwrap();
        std::fs::write(root.join("source.picomp"), SOURCE).unwrap();
        std::fs::write(root.join(".completion-root"), "").unwrap();
        let script = root.join("completion_peer.py");
        std::fs::write(&script, PEER).unwrap();
        let config = serde_json::from_value(json!({"lsp":{"servers":{"completion-fixture":{
            "command":python,"args":["-I","-u",script.display().to_string(),"snippet_transform_apply"],
            "languages":["plaintext"],"extensions":[".picomp"],"rootMarkers":[".completion-root"]
        }}}})).unwrap();
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .enable_parking(false)
            .worker_threads(1)
            .blocking_threads(1, 8)
            .build()
            .unwrap();
        Some((LspTool::new(&root, Some(&config)), runtime))
    }

    fn request(
        tool: &LspTool,
        runtime: &asupersync::runtime::Runtime,
        args: Value,
    ) -> crate::error::Result<Value> {
        let output = runtime.block_on(tool.execute("transform-test", args, None))?;
        assert!(!output.is_error);
        Ok(output.details.expect("structured completion result"))
    }

    fn list(tool: &LspTool, runtime: &asupersync::runtime::Runtime) -> String {
        let result = request(
            tool,
            runtime,
            json!({
                "action":"completion","file":"source.picomp","position":{"line":1,"character":14},
                "query":"Ty","timeout":5
            }),
        )
        .unwrap();
        assert!(result["items"][0]["blockedReason"].is_null());
        result["items"][0]["completionId"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn select(id: &str, value: &str, apply: bool) -> Value {
        json!({"action":"completion","completionId":id,"snippetValues":{"1":value},"apply":apply,"timeout":5})
    }

    fn source(root: &Path) -> String {
        std::fs::read_to_string(root.join("source.picomp")).unwrap()
    }

    #[test]
    fn transformed_preview_and_apply_share_auto_imports_without_caching_substitutions() {
        let temp = tempfile::tempdir().unwrap();
        let Some((tool, runtime)) = fixture(temp.path()) else {
            return;
        };
        let id = list(&tool, &runtime);
        let preview = request(&tool, &runtime, select(&id, "preview_name", false)).unwrap();
        assert_eq!(preview["canApply"], true);
        assert_eq!(
            preview["edits"][0]["newText"],
            "Type(preview_name, PreviewName, preview_name)"
        );
        assert_eq!(preview["edits"][1]["newText"], IMPORT);
        assert_eq!(source(temp.path()), SOURCE);
        let defaults = request(
            &tool,
            &runtime,
            json!({"action":"completion","completionId":id,"timeout":5}),
        )
        .unwrap();
        assert_eq!(
            defaults["edits"][0]["newText"],
            "Type(my_name, MyName, my_name)"
        );
        let applied = request(&tool, &runtime, select(&id, "live_name", true)).unwrap();
        assert_eq!(applied["applied"], true);
        assert_eq!(applied["commandExecuted"], false);
        assert_eq!(
            source(temp.path()),
            format!("{IMPORT}// header\nfn main() {{ Type(live_name, LiveName, live_name) }}\n")
        );
        assert!(request(&tool, &runtime, select(&id, "again", true)).is_err());
        let log = std::fs::read_to_string(temp.path().join("completion-requests.jsonl")).unwrap();
        let events: Vec<Value> = log
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let resolves: Vec<_> = events
            .iter()
            .filter(|event| event["method"] == "completionItem/resolve")
            .collect();
        assert_eq!(resolves.len(), 1);
        assert!(
            resolves[0]["params"]["textEdit"]["newText"]
                .as_str()
                .unwrap()
                .contains("${1/(.*)/")
        );
        assert!(
            !events
                .iter()
                .any(|event| event["method"] == "workspace/executeCommand")
        );
    }

    #[test]
    fn unsupported_transform_value_does_not_write_or_consume_the_selection() {
        let temp = tempfile::tempdir().unwrap();
        let Some((tool, runtime)) = fixture(temp.path()) else {
            return;
        };
        let id = list(&tool, &runtime);
        for value in ["caf\u{e9}", "first\nsecond"] {
            let preview = request(&tool, &runtime, select(&id, value, false)).unwrap();
            assert_eq!(preview["canApply"], false);
            let error = request(&tool, &runtime, select(&id, value, true)).unwrap_err();
            assert!(error.to_string().contains("LSP_COMPLETION_UNSUPPORTED"));
            assert_eq!(source(temp.path()), SOURCE);
        }
        request(&tool, &runtime, select(&id, "valid", true)).unwrap();
        assert_eq!(
            source(temp.path()),
            format!("{IMPORT}// header\nfn main() {{ Type(valid, Valid, valid) }}\n")
        );
    }

    #[test]
    fn source_drift_after_transform_preview_prevents_primary_edit_and_imports() {
        let temp = tempfile::tempdir().unwrap();
        let Some((tool, runtime)) = fixture(temp.path()) else {
            return;
        };
        let id = list(&tool, &runtime);
        request(&tool, &runtime, select(&id, "chosen", false)).unwrap();
        std::fs::write(temp.path().join("source.picomp"), "external change\n").unwrap();
        let error = request(&tool, &runtime, select(&id, "chosen", true)).unwrap_err();
        assert!(error.to_string().contains("LSP_COMPLETION_STALE"));
        assert_eq!(source(temp.path()), "external change\n");
    }
}
