//! Server-suggested enclosing selections for the existing refactor workflow.
//!
//! The returned ranges are observations of the synchronized source, not edit
//! authorization or proof that an extraction is semantically available.

use super::{Budget, MAX_PAYLOAD_BYTES, MAX_RESPONSE_BYTES, Position, Range, Result};
use super::{bounded_size, json, position_to_offset_exact, source_range, tool_err, Value};

pub(super) const MAX_LEVELS: usize = 64;
const MAX_TEXT_PREVIEW_BYTES: usize = 4096;

fn malformed(message: &str) -> crate::error::Error {
    tool_err("LSP_SELECTION_MALFORMED", message)
}

/// A limit bounds returned levels, not validation. An invalid outer range
/// must not disappear behind a useful-looking inner range and limit:1.
pub(super) fn output(
    raw: &Value,
    source: &str,
    position: Position,
    limit: usize,
    mut payload: Value,
    budget: &Budget,
) -> Result<Value> {
    budget.remaining()?;
    bounded_size(raw, MAX_RESPONSE_BYTES)?;
    let mut node = match raw {
        Value::Null => None,
        Value::Array(items) if items.len() == 1 => Some(&items[0]),
        _ => return Err(malformed("selectionRange must return one selection for the requested position, or null")),
    };
    payload["ranges"] = json!([]);
    payload["order"] = json!("inner_to_outer");
    payload["total"] = json!(0);
    payload["count"] = json!(0);
    payload["truncated"] = json!(false);
    payload["responseComplete"] = json!(true);
    payload["returnedNull"] = json!(raw.is_null());
    payload["note"] = json!(
        "Suggested source selections only, not an approval handle or an extraction guarantee. Pass an exact range to a fresh code_actions request; text previews may be shortened."
    );
    let overhead = bounded_size(&payload, MAX_PAYLOAD_BYTES)?;
    let mut remaining = MAX_PAYLOAD_BYTES.saturating_sub(overhead + 256);
    let mut ranges = Vec::new();
    let mut child: Option<Range> = None;
    let mut total = 0;
    let mut full = false;
    while let Some(value) = node {
        budget.remaining()?;
        total += 1;
        if total > MAX_LEVELS {
            return Err(tool_err("LSP_SEMANTIC_LIMIT", "selection hierarchy exceeds 64 levels"));
        }
        if !value.is_object() {
            return Err(malformed("a selection and its parent must be objects"));
        }
        let selected = source_range(&value["range"], source)?;
        if position < selected.start || position > selected.end {
            return Err(malformed("selection range does not contain the requested cursor"));
        }
        if child.is_some_and(|child| selected.start > child.start || selected.end < child.end) {
            return Err(malformed("parent selection must contain its child range"));
        }
        // Equal ranges are valid containment and retain server order. An empty
        // range at the cursor is the protocol's explicit no-selection result.
        child = Some(selected);
        if !full && ranges.len() < limit.min(MAX_LEVELS) {
            let start = position_to_offset_exact(source, selected.start)
                .ok_or_else(|| malformed("selection start is not an exact source boundary"))?;
            let end = position_to_offset_exact(source, selected.end)
                .ok_or_else(|| malformed("selection end is not an exact source boundary"))?;
            let text = &source[start..end];
            let mut preview_end = text.len().min(MAX_TEXT_PREVIEW_BYTES);
            while !text.is_char_boundary(preview_end) {
                preview_end -= 1;
            }
            let entry = json!({
                "index":total,"range":selected,"byteLength":text.len(),
                "empty":start == end,"text":&text[..preview_end],
                "textTruncated":preview_end != text.len()
            });
            let size = bounded_size(&entry, MAX_PAYLOAD_BYTES)? + 1;
            if size > remaining {
                full = true;
            } else {
                remaining -= size;
                ranges.push(entry);
            }
        }
        node = match value.get("parent") {
            None => None,
            Some(parent) if parent.is_object() => Some(parent),
            Some(_) => return Err(malformed("selection parent must be an object when present")),
        };
    }
    payload["total"] = json!(total);
    payload["count"] = json!(ranges.len());
    payload["truncated"] = json!(ranges.len() != total);
    payload["responseComplete"] = json!(ranges.len() == total);
    payload["ranges"] = json!(ranges);
    bounded_size(&payload, MAX_PAYLOAD_BYTES)?;
    budget.remaining()?;
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn span(start: u32, end: u32) -> Value {
        json!({"start":{"line":0,"character":start},"end":{"line":0,"character":end}})
    }

    fn chain(spans: &[Value]) -> Value {
        let mut next = None;
        for range in spans.iter().rev() {
            let mut node = json!({"range":range});
            if let Some(parent) = next { node["parent"] = parent; }
            next = Some(node);
        }
        json!([next.expect("nonempty test chain")])
    }

    fn render(raw: &Value, source: &str, cursor: u32, limit: usize) -> Result<Value> {
        output(raw, source, Position { line:0, character:cursor }, limit,
            json!({"action":"selection_ranges"}), &Budget::new(Duration::from_secs(10)))
    }

    #[test]
    fn selections_preserve_exact_source_ranges_and_inner_to_outer_order() {
        let raw = chain(&[span(5,6), span(0,7), span(0,8)]);
        let out = render(&raw, "call(x);", 5, 16).unwrap();
        assert_eq!(out["total"], 3);
        assert_eq!(out["count"], 3);
        assert_eq!(out["ranges"][0]["text"], "x");
        assert_eq!(out["ranges"][1]["text"], "call(x)");
        assert_eq!(out["ranges"][2]["range"], span(0,8));
        assert_eq!(out["ranges"][2]["index"], 3);
        assert_eq!(out["responseComplete"], true);
        assert_eq!(out["truncated"], false);
    }

    #[test]
    fn equal_parent_ranges_and_empty_cursor_selections_remain_distinct() {
        let out = render(&chain(&[span(1,1),span(0,2),span(0,2)]), "xy", 1, 16).unwrap();
        assert_eq!(out["count"], 3);
        assert_eq!(out["ranges"][0]["empty"], true);
        assert_eq!(out["ranges"][0]["byteLength"], 0);
        assert_eq!(out["ranges"][1]["text"], "xy");
        assert_eq!(out["ranges"][2]["text"], "xy");
        assert!(render(&chain(&[span(2,2)]), "xy", 2, 16).is_ok());
    }

    #[test]
    fn null_is_no_report_but_wrong_response_cardinality_is_an_error() {
        let out = render(&Value::Null, "x", 0, 16).unwrap();
        assert_eq!(out["returnedNull"], true);
        assert_eq!(out["count"], 0);
        assert_eq!(out["ranges"], json!([]));
        for raw in [json!([]), json!([null]), json!([{},{}]), json!({}), json!(false)] {
            assert!(render(&raw, "x", 0, 16).is_err(), "{raw}");
        }
    }

    #[test]
    fn every_range_must_contain_the_cursor_and_be_within_its_parent() {
        for raw in [chain(&[span(0,1)]),chain(&[span(1,4),span(0,3)]),
            chain(&[span(1,4),span(2,5)])] {
            assert!(render(&raw, "abcde", 2, 16).is_err());
        }
        assert!(render(&chain(&[span(0,2)]), "ab", 2, 16).is_ok());
    }

    #[test]
    fn limits_cannot_hide_a_malformed_parent() {
        for parent in [json!(null), json!(false), json!([]), json!({"range":span(0,99)}),
            json!({"range":span(2,3)}), json!({"range":span(3,0)})] {
            let raw = json!([{"range":span(1,2),"parent":parent}]);
            assert!(render(&raw, "abc", 1, 1).is_err(), "{raw}");
        }
    }

    #[test]
    fn unicode_and_line_endings_map_to_the_original_bytes() {
        let range = json!({"start":{"line":0,"character":2},"end":{"line":1,"character":1}});
        let out = render(&chain(&[range.clone()]), "😀x\r\ny\rz", 2, 16).unwrap();
        assert_eq!(out["ranges"][0]["range"], range);
        assert_eq!(out["ranges"][0]["text"], "x\r\ny");
        assert_eq!(out["ranges"][0]["byteLength"], 4);
        assert!(render(&chain(&[span(1,3)]), "😀x", 2, 16).is_err());
    }

    #[test]
    fn malformed_unsigned_coordinates_do_not_become_clamped_ranges() {
        for number in [json!(-1), json!(1.5), json!("0"), json!(2147483648_u64)] {
            let mut raw = chain(&[span(0,1)]);
            raw[0]["range"]["start"]["character"] = number;
            assert!(render(&raw, "x", 0, 16).is_err());
        }
    }

    #[test]
    fn omitted_valid_levels_are_counted_and_never_claimed_returned() {
        let out = render(&chain(&[span(1,2),span(0,3)]), "abc", 1, 1).unwrap();
        assert_eq!(out["total"], 2);
        assert_eq!(out["count"], 1);
        assert_eq!(out["truncated"], true);
        assert_eq!(out["responseComplete"], false);
        assert_eq!(out["ranges"][0]["text"], "b");
    }

    #[test]
    fn preview_clipping_preserves_unicode_and_the_complete_range() {
        let text = "界".repeat(2000);
        let out = render(&chain(&[span(0,2000)]), &text, 0, 16).unwrap();
        let shown = out["ranges"][0]["text"].as_str().unwrap();
        assert_eq!(shown.len(), 4095);
        assert_eq!(out["ranges"][0]["byteLength"], 6000);
        assert_eq!(out["ranges"][0]["range"], span(0,2000));
        assert_eq!(out["ranges"][0]["textTruncated"], true);
        assert_eq!(out["responseComplete"], true);
    }

    #[test]
    fn escaping_is_charged_to_output_without_hiding_outer_validation() {
        let source = "\u{0001}".repeat(4096);
        let raw = chain(&vec![span(0,4096); MAX_LEVELS]);
        let out = render(&raw, &source, 0, MAX_LEVELS).unwrap();
        assert!(out.to_string().len() <= MAX_PAYLOAD_BYTES);
        assert!(out["count"].as_u64().unwrap() < MAX_LEVELS as u64);
        assert_eq!(out["total"], MAX_LEVELS);
        assert_eq!(out["truncated"], true);
        let mut spans = vec![span(0,4096); MAX_LEVELS - 1];
        spans.push(span(0,4097));
        assert!(render(&chain(&spans), &source, 0, MAX_LEVELS).is_err());
    }

    #[test]
    fn excessive_depth_and_expired_budgets_fail_explicitly() {
        assert!(render(&chain(&vec![span(0,1); MAX_LEVELS]), "x", 0, 1).is_ok());
        let error = render(&chain(&vec![span(0,1); MAX_LEVELS + 1]), "x", 0, 1).unwrap_err();
        assert!(error.to_string().contains("LSP_SEMANTIC_LIMIT"));
        let error = output(&Value::Null, "x", Position {line:0,character:0}, 16,
            json!({}), &Budget::new(Duration::ZERO)).unwrap_err();
        assert!(error.to_string().contains("LSP_TIMEOUT"));
    }
}
