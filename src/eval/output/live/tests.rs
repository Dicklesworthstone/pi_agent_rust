use super::super::Capture;
use super::*;
use crate::model::TextContent;
use std::sync::{Arc, Mutex};
use std::time::Instant;

fn display(sequence: u64, frame: Value) -> Value {
    json!({"id":7,"stream":{"sequence":sequence,"name":"display","display":frame}})
}

fn text(value: &str) -> Value {
    json!({"mimeType":"text/plain","data":value})
}

#[test]
fn rich_only_progress_is_a_running_replacement_snapshot() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    let mut capture = Capture::new(
        "python",
        Some(Box::new(move |event| {
            sink.lock().unwrap().push(event);
        })),
    );
    capture
        .accept(
            &display(
                1,
                json!({"mimeType":"application/json","data":{"answer":42}}),
            ),
            7,
        )
        .unwrap();
    let now = Instant::now();
    capture.publish_at(now, false);
    capture.accept(&display(2, text("second")), 7).unwrap();
    capture.publish_at(now, false);
    assert_eq!(events.lock().unwrap().len(), 1);
    capture.publish_at(now + super::super::UPDATE_INTERVAL, false);
    let events = events.lock().unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[1].content.len(), 2);
    let details = events[1].details.as_ref().unwrap();
    assert_eq!(details["status"], "running");
    assert_eq!(details["displayStream"]["complete"], false);
    assert_eq!(details["displayStream"]["sequence"], 2);
    assert_eq!(details["jsonOutputs"], json!([{"answer":42}]));
    assert_eq!(details["output"]["sequence"], 0);
    assert_eq!(details["output"]["stdoutBytes"], 0);
}

#[test]
fn malformed_rich_frames_never_change_or_publish_the_prefix() {
    let mut capture = Capture::new("python", None);
    for bad in [
        display(2, text("out of order")),
        display(1, json!({"mimeType":"image/png","data":"aGVsbG8="})),
        display(1, json!({"mimeType":"text/plain","data":42})),
        display(
            1,
            json!({"mimeType":"text/html","data":"<b>not supported</b>"}),
        ),
        json!({"id":7,"stream":{"sequence":1,"name":"display","text":"not a MIME frame"}}),
        json!({"id":8,"stream":{"sequence":1,"name":"display","display":text("wrong cell")}}),
    ] {
        assert!(capture.accept(&bad, 7).is_err());
        assert!(capture.displays.is_empty());
        assert!(!capture.dirty);
    }
    capture.accept(&display(1, text("accepted")), 7).unwrap();
    assert!(capture.accept(&display(1, text("duplicate")), 7).is_err());
    assert_eq!(capture.displays.len(), 1);
}

#[test]
fn final_rich_values_must_match_the_stream_without_rewriting_console() {
    let frame = text("original");
    let mut capture = Capture::new("python", None);
    capture.accept(&display(1, frame.clone()), 7).unwrap();
    capture
        .accept(
            &json!({"id":7,"stream":{"sequence":1,"name":"stdout","text":"console"}}),
            7,
        )
        .unwrap();
    for frames in [
        json!([]),
        json!([text("rewritten")]),
        json!([frame.clone(), frame.clone()]),
        Value::Null,
    ] {
        let mut response = json!({"stdout":"","stderr":"","outputSequence":1,"displays":frames});
        assert!(capture.finish_response(&mut response).is_err());
        assert_eq!(response["stdout"], "");
    }
    let mut response = json!({"stdout":"","stderr":"","outputSequence":1,"displays":[frame]});
    capture.finish_response(&mut response).unwrap();
    assert_eq!(response["stdout"], "console");
    let mut output = crate::tools::ToolOutput {
        content: Vec::new(),
        details: Some(json!({})),
        is_error: false,
    };
    capture.annotate(&mut output);
    assert_eq!(output.details.unwrap()["displayStream"]["complete"], true);
    assert!(
        output.content.is_empty(),
        "final formatter, not annotate, appends displays"
    );
}

#[test]
fn live_admission_reuses_count_aggregate_and_item_limits() {
    let mut capture = Capture::new("js", None);
    for sequence in 1..=32 {
        capture
            .accept(&display(sequence, text("small")), 7)
            .unwrap();
    }
    assert!(capture.accept(&display(33, text("overflow")), 7).is_err());
    let mut capture = Capture::new("js", None);
    let large = text(&"x".repeat(1_500_000));
    capture.accept(&display(1, large.clone()), 7).unwrap();
    capture.accept(&display(2, large.clone()), 7).unwrap();
    assert!(capture.accept(&display(3, large), 7).is_err());
    assert_eq!(capture.displays.len(), 2);
    let oversized = text(&"x".repeat(2 * 1024 * 1024));
    assert!(
        Capture::new("python", None)
            .accept(&display(1, oversized), 7)
            .is_err()
    );
}

#[test]
fn preview_omission_does_not_discard_or_truncate_final_json() {
    let frame = json!({"mimeType":"application/json","data":{"large":"x".repeat(PREVIEW_BYTES)}});
    let mut capture = Capture::new("python", None);
    capture.accept(&display(1, frame.clone()), 7).unwrap();
    let mut content = Vec::new();
    let mut details = json!({});
    capture.displays.append(&mut content, &mut details, false);
    assert_eq!(details["jsonOutputs"], json!([]));
    assert_eq!(details["displays"][0]["previewOmitted"], true);
    assert_eq!(details["displayStream"]["omittedDisplays"], 1);
    assert!(matches!(&content[0], ContentBlock::Text(text) if text.text.len() < 256));
    let mut response =
        json!({"stdout":"","stderr":"","outputSequence":0,"displays":[frame.clone()]});
    capture.finish_response(&mut response).unwrap();
    assert_eq!(response["displays"], json!([frame]));
}

#[test]
fn watchdog_error_keeps_bounded_rich_progress_but_not_success() {
    let mut capture = Capture::new("js", None);
    capture.accept(&display(1, text("progress")), 7).unwrap();
    let mut output = crate::tools::ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new("EVAL_CANCELLED"))],
        details: Some(json!({"stateReset":true})),
        is_error: true,
    };
    capture.append_partial(&mut output);
    assert!(output.is_error);
    assert_eq!(output.content.len(), 2);
    assert_eq!(
        output.details.as_ref().unwrap()["displayStream"]["complete"],
        false
    );
    assert_eq!(output.details.as_ref().unwrap()["stateReset"], true);
    let failure = capture.with_error(Error::tool("eval", "EVAL_TIMEOUT"));
    assert!(failure.to_string().contains("EVAL_DISPLAY_INCOMPLETE"));
    assert!(failure.to_string().contains("EVAL_TIMEOUT"));
}
