use super::*;
use serde_json::json;

#[allow(clippy::needless_pass_by_value)] // call sites pass inline json! values
fn notebook(cell: Value) -> Vec<u8> {
    serde_json::to_vec(&json!({"nbformat":4,"cells":[cell]})).unwrap()
}

#[test]
fn multiline_fields_are_concatenated_without_inventing_separators() {
    assert_eq!(
        multiline(&json!(["one", "two\n", "界"])).unwrap(),
        "onetwo\n界"
    );
    for value in [json!(["keep", 3]), json!(null), json!({})] {
        assert!(multiline(&value).is_err());
    }
}

#[test]
fn saved_streams_errors_and_outputs_remain_in_order() {
    let raw = notebook(
        json!({"cell_type":"code","source":"work()","execution_count":3,"outputs":[
            {"output_type":"stream","name":"stderr","text":["first", "\n"]},
            {"output_type":"error","ename":"ValueError","evalue":"second","traceback":["frame one", "frame two"]},
            {"output_type":"stream","name":"stdout","text":"third"}
        ]}),
    );
    let text = render(&raw).unwrap();
    assert!(text.find("first").unwrap() < text.find("ValueError: second").unwrap());
    assert!(text.find("frame one").unwrap() < text.find("frame two").unwrap());
    assert!(text.find("frame two").unwrap() < text.find("third").unwrap());
    assert!(text.contains("execution_count: 3"));
    assert!(text.contains("outputs: 3 present"));
}

#[test]
fn json_representation_preserves_types_and_omits_alternates() {
    let raw = notebook(json!({"cell_type":"code","source":"data","outputs":[
        {"output_type":"display_data","data":{"application/json":{"a":null,"b":true,"c":[42]},
            "text/plain":"alternate secret","image/png":"binary secret"}}
    ]}));
    let text = render(&raw).unwrap();
    assert!(text.contains("\"a\": null"));
    assert!(text.contains("\"b\": true"));
    assert!(text.contains("42"));
    assert!(!text.contains("alternate secret"));
    assert!(!text.contains("binary secret"));
    assert!(text.contains("image/png"));
}

#[test]
fn html_only_outputs_use_static_reader_not_executable_markup() {
    let raw = notebook(json!({"cell_type":"code","source":"report","outputs":[
        {"output_type":"display_data","data":{"text/html":["<h1>Useful report</h1>","<script>never_run()</script><p>Result</p>"]}}
    ]}));
    let text = render(&raw).unwrap();
    assert!(text.contains("Useful report"));
    assert!(text.contains("Result"));
    assert!(!text.contains("never_run"));
}

#[test]
fn binary_widget_and_attachment_payloads_are_not_echoed() {
    let raw = notebook(
        json!({"cell_type":"markdown","source":"diagram","attachments":{
            "inline.png":{"image/png":"attachment secret"}},"outputs":[
            {"output_type":"display_data","data":{"image/png":"binary secret","application/vnd.jupyter.widget-view+json":{"model_id":"widget secret"}}}
        ]}),
    );
    let text = render(&raw).unwrap();
    assert!(text.contains("1 saved attachments"));
    assert!(text.contains("binary/widget output was not rendered"));
    for secret in ["attachment secret", "binary secret", "widget secret"] {
        assert!(!text.contains(secret));
    }
}

#[test]
fn raw_code_fences_and_terminal_controls_are_rendered_as_data() {
    let raw = notebook(json!({"cell_type":"raw","source":"```\ntext\n```\n\u{1b}[31mred\u{0}"}));
    let text = render(&raw).unwrap();
    assert!(text.contains("~~~text\n```"));
    assert!(text.contains("\\u{001b}[31mred\\u{0000}"));
    assert!(!text.contains('\u{1b}'));
    assert!(!text.contains('\0'));
}

#[test]
fn unknown_types_are_explicit_and_malformed_known_fields_fail() {
    let raw = notebook(
        json!({"cell_type":"future_cell","source":"kept source","outputs":[
            {"output_type":"future_output","payload":"not interpreted"}
        ]}),
    );
    let text = render(&raw).unwrap();
    assert!(text.contains("kept source"));
    assert!(text.contains("Unsupported saved output type"));
    assert!(!text.contains("not interpreted"));
    for output in [
        json!({"output_type":"stream","text":7}),
        json!({"output_type":"display_data","data":null}),
        json!({"output_type":"error","ename":"Bad","evalue":"error","traceback":[3]}),
    ] {
        assert!(
            render(&notebook(
                json!({"cell_type":"code","source":"x","outputs":[output]})
            ))
            .is_err()
        );
    }
    assert!(
        render(&notebook(
            json!({"cell_type":"code","source":"x","execution_count":true})
        ))
        .is_err()
    );
}

#[test]
fn cell_output_and_render_limits_refuse_instead_of_truncating() {
    assert!(
        render(
            &serde_json::to_vec(&json!({"nbformat":4,"cells":vec![json!({}); MAX_CELLS+1]}))
                .unwrap()
        )
        .is_err()
    );
    let raw =
        notebook(json!({"cell_type":"code","source":"x","outputs":vec![json!({}); MAX_OUTPUTS+1]}));
    assert!(render(&raw).unwrap_err().to_string().contains("LIMIT"));
    let mut out = Render {
        bytes: Vec::new(),
        limit: 10,
    };
    out.push("1234567890").unwrap();
    assert!(out.push("x").is_err());
    assert_eq!(out.bytes.len(), 10);
    let mut out = Render {
        bytes: Vec::new(),
        limit: 20,
    };
    assert!(out.json(&json!({"large":"x".repeat(50)})).is_err());
    assert!(out.bytes.len() <= 20);
}

#[test]
fn recognition_checks_top_level_structure_not_a_word_in_a_string() {
    let raw = b"\xef\xbb\xbf \n{\"nbformat\":4,\"cells\":[]}";
    assert!(recognizes(raw));
    assert!(render(raw).unwrap().contains("code was not executed"));
    assert!(!recognizes(br#"{"text":"nbformat", "cells":[]}"#));
    assert!(!recognizes(br#"{"nested":{"nbformat":4,"cells":[]}}"#));
    assert!(render(br#"{"nbformat":5,"cells":[]}"#).is_err());
    assert!(render(b"not JSON").is_err());
}

#[test]
fn malformed_mime_value_is_not_silently_replaced_by_an_alternate() {
    let raw = notebook(json!({"cell_type":"code","source":"x","outputs":[
        {"output_type":"display_data","data":{"text/plain":["good",false],"text/html":"<p>fallback</p>"}}
    ]}));
    assert!(render(&raw).is_err());
}
