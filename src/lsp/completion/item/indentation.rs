//! Apply the negotiated completion indentation policy to the primary edit only.
//!
//! The insertion point is the replacement range's start, as in VS Code's
//! snippet insertion. Copy only leading spaces/tabs preceding that point;
//! indentation being replaced is not reintroduced. Preserve relative snippet
//! indentation and exact line endings instead of guessing a tab width or EOL.

use std::borrow::Cow;

use serde_json::Value;

use crate::error::Result;
use crate::lsp::completion::{MAX_ITEM_BYTES, malformed};
use crate::lsp::text::{Position, position_to_offset_exact};
use crate::lsp::tool_err;

#[derive(Clone, Copy)]
pub(super) enum Mode {
    AsIs,
    Adjust,
}

fn limit() -> crate::error::Error {
    tool_err(
        "LSP_COMPLETION_LIMIT",
        "indented completion exceeds its insertion byte limit",
    )
}

impl Mode {
    pub(super) fn parse(item: &Value) -> Result<Self> {
        item.get("insertTextMode")
            .filter(|value| !value.is_null())
            .map_or(Ok(Self::AsIs), |value| match value.as_u64() {
                Some(1) => Ok(Self::AsIs),
                Some(2) => Ok(Self::Adjust),
                _ => Err(tool_err(
                    "LSP_COMPLETION_UNSUPPORTED",
                    "completion insertTextMode must be asIs (1) or adjustIndentation (2)",
                )),
            })
    }

    /// Input is already snippet-expanded. Neither the cached server item nor
    /// the synchronized source is changed; preview and apply use this same path.
    pub(super) fn text<'a>(
        self,
        text: &'a str,
        source: &str,
        insertion: Position,
    ) -> Result<Cow<'a, str>> {
        if matches!(self, Self::AsIs) {
            return Ok(Cow::Borrowed(text));
        }
        if text.len() > MAX_ITEM_BYTES {
            return Err(limit());
        }
        let offset = position_to_offset_exact(source, insertion)
            .ok_or_else(|| malformed("invalid completion indentation position"))?;
        let before = &source[..offset];
        let start = before.rfind(['\r', '\n']).map_or(0, |index| index + 1);
        let prefix = &source[start..offset];
        let indent_len = prefix
            .bytes()
            .take_while(|byte| matches!(*byte, b' ' | b'\t'))
            .count();
        let indent = &prefix[..indent_len];
        if indent.is_empty() || !text.contains(['\r', '\n']) {
            return Ok(Cow::Borrowed(text));
        }
        // A source line may be much larger than an individual completion.
        // Preflight the complete expansion before allocating or copying it.
        let size = break_ends(text)
            .count()
            .checked_mul(indent.len())
            .and_then(|extra| text.len().checked_add(extra))
            .filter(|size| *size <= MAX_ITEM_BYTES)
            .ok_or_else(limit)?;
        let mut output = String::new();
        output.try_reserve_exact(size).map_err(|_| limit())?;
        let mut cursor = 0;
        for end in break_ends(text) {
            output.push_str(&text[cursor..end]);
            output.push_str(indent);
            cursor = end;
        }
        output.push_str(&text[cursor..]);
        Ok(Cow::Owned(output))
    }
}

/// Byte offsets after line endings. A CRLF pair contributes one indentation,
/// while adjacent bare CRs/LFs retain each of their empty lines.
fn break_ends(text: &str) -> impl Iterator<Item = usize> + '_ {
    let mut breaks = text.match_indices(['\r', '\n']).peekable();
    std::iter::from_fn(move || {
        let (index, delimiter) = breaks.next()?;
        if delimiter == "\r"
            && breaks
                .peek()
                .is_some_and(|(next, value)| *next == index + 1 && *value == "\n")
        {
            let _ = breaks.next();
            Some(index + 2)
        } else {
            Some(index + 1)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::completion::{item, snippet};
    use crate::lsp::text::{Range, TextEdit, apply_text_edits};
    use serde_json::json;

    fn position(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    fn range(line: u32, start: u32, end: u32) -> Range {
        Range {
            start: position(line, start),
            end: position(line, end),
        }
    }

    fn apply(source: &str, values: &[Value]) -> String {
        let edits: Vec<_> = values
            .iter()
            .map(|value| TextEdit {
                range: serde_json::from_value(value["range"].clone()).unwrap(),
                new_text: value["newText"].as_str().unwrap().to_string(),
            })
            .collect();
        apply_text_edits(source, &edits).unwrap()
    }

    #[test]
    fn spaces_tabs_relative_indent_and_empty_lines_are_preserved() {
        let source = "\t  obj";
        let text = "first\n\tchild\n\nlast\n";
        let actual = Mode::Adjust.text(text, source, position(0, 3)).unwrap();
        assert_eq!(actual, "first\n\t  \tchild\n\t  \n\t  last\n\t  ");
        assert_eq!(source, "\t  obj");
        assert_eq!(text, "first\n\tchild\n\nlast\n");
    }

    #[test]
    fn crlf_and_bare_cr_are_indented_once_without_eol_conversion() {
        assert_eq!(
            Mode::Adjust
                .text("a\r\nb\rc\nd\r\r\n", "  x", position(0, 2))
                .unwrap(),
            "a\r\n  b\r  c\n  d\r  \r\n  "
        );
    }

    #[test]
    fn insertion_in_leading_whitespace_uses_only_the_prefix_before_it() {
        let source = " \t  foo";
        let item = json!({"label":"a", "insertTextMode":2,
            "textEdit":{"range":range(0, 2, 2), "newText":"a\nb"}});
        let edits = item::edits(&item, source, position(0, 2), None).unwrap();
        assert_eq!(apply(source, &edits), " \ta\n \tb  foo");
        // A replacement that removes the existing indent must not re-add it
        // only to its later lines.
        let item = json!({"label":"a", "insertTextMode":2,
            "textEdit":{"range":range(0, 0, 7), "newText":"a\nb"}});
        let edits = item::edits(&item, source, position(0, 7), None).unwrap();
        assert_eq!(apply(source, &edits), "a\nb");
    }

    #[test]
    fn unicode_positions_and_non_whitespace_prefixes_do_not_become_indent() {
        let source = "outside\r\n\t🦀.foo";
        assert_eq!(
            Mode::Adjust.text("x\ny", source, position(1, 4)).unwrap(),
            "x\n\ty"
        );
        assert!(Mode::Adjust.text("x\ny", source, position(1, 2)).is_err());
        assert!(Mode::Adjust.text("x\ny", source, position(2, 0)).is_err());
        assert_eq!(
            Mode::Adjust
                .text("x\ny", "\u{a0}foo", position(0, 1))
                .unwrap(),
            "x\ny"
        );
    }

    #[test]
    fn default_as_is_and_single_line_text_remain_borrowed() {
        for item in [
            json!({}),
            json!({"insertTextMode":null}),
            json!({"insertTextMode":1}),
        ] {
            let actual = Mode::parse(&item)
                .unwrap()
                .text("a\nb", "  x", position(0, 2))
                .unwrap();
            assert!(matches!(actual, Cow::Borrowed("a\nb")));
        }
        assert!(matches!(
            Mode::Adjust
                .text("one line", "  x", position(0, 2))
                .unwrap(),
            Cow::Borrowed("one line")
        ));
    }

    #[test]
    fn expansion_checks_the_exact_budget_before_copying_indent() {
        let text = format!("{}\n", "x".repeat(MAX_ITEM_BYTES - 3));
        let actual = Mode::Adjust.text(&text, "  x", position(0, 2)).unwrap();
        assert_eq!(actual.len(), MAX_ITEM_BYTES);
        assert!(actual.ends_with("\n  "));
        let too_large = format!("x{text}");
        let error = Mode::Adjust
            .text(&too_large, "  x", position(0, 2))
            .unwrap_err();
        assert!(error.to_string().contains("LSP_COMPLETION_LIMIT"));
        let source = format!("{}x", " ".repeat(1_000_000));
        assert!(
            Mode::Adjust
                .text("x\ny", &source, position(0, 1_000_000))
                .is_err()
        );
    }

    #[test]
    #[allow(clippy::literal_string_with_formatting_args)] // LSP snippet tabstops, not format strings
    fn default_range_snippet_and_auto_import_follow_the_real_preparation_path() {
        let source = "header\n\tfoo\n";
        let defaults =
            json!({"editRange":range(1, 1, 4), "insertTextMode":2, "insertTextFormat":2});
        let raw = json!({"label":"if", "textEditText":"if (${1:ok}) {\n\t${2:call()};\n}$0",
            "additionalTextEdits":[{"range":range(0, 0, 0),"newText":"import x\n$literal\n"}]});
        let cached = item::materialize(&raw, Some(&defaults)).unwrap();
        let saved = cached.clone();
        let expanded = snippet::prepare(&cached, None).unwrap();
        expanded.require_values().unwrap();
        let edits = item::edits(&expanded.item, source, position(1, 4), None).unwrap();
        assert_eq!(edits[0]["newText"], "if (ok) {\n\t\tcall();\n\t}");
        assert_eq!(edits[1], raw["additionalTextEdits"][0]);
        assert_eq!(
            apply(source, &edits),
            "import x\n$literal\nheader\n\tif (ok) {\n\t\tcall();\n\t}\n"
        );
        assert_eq!(
            cached, saved,
            "preview must not rewrite the retained server item"
        );
        let values = snippet::Values::from([("2".into(), "literal$3\nmore".into())]);
        let expanded = snippet::prepare(&cached, Some(&values)).unwrap();
        let edits = item::edits(&expanded.item, source, position(1, 4), None).unwrap();
        assert_eq!(
            edits[0]["newText"],
            "if (ok) {\n\t\tliteral$3\n\tmore;\n\t}"
        );
        assert_eq!(cached, saved);
    }

    #[test]
    fn explicit_as_is_overrides_adjustment_defaults() {
        let raw = json!({"label":"x", "insertTextMode":1,"textEditText":"x\ny"});
        let defaults = json!({"editRange":range(0, 2, 3), "insertTextMode":2});
        let item = item::materialize(&raw, Some(&defaults)).unwrap();
        let edits = item::edits(&item, "  x", position(0, 3), None).unwrap();
        assert_eq!(apply("  x", &edits), "  x\ny");
    }

    #[test]
    fn insert_replace_and_explicit_fallback_use_the_same_indentation() {
        let source = "  partial";
        let replace = range(0, 2, 9);
        let item = json!({"label":"x", "insertTextMode":2,
            "textEdit":{"insert":range(0, 2, 5),"replace":replace,"newText":"x\ny"}});
        let edits = item::edits(&item, source, position(0, 5), None).unwrap();
        assert_eq!(apply(source, &edits), "  x\n  y");
        let item = json!({"label":"x", "insertTextMode":2,"insertText":"x\ny"});
        let fallback = item::edits(&item, source, position(0, 5), Some(replace)).unwrap();
        assert_eq!(fallback, edits);
    }

    #[test]
    fn unknown_modes_commands_and_overlaps_are_still_refused() {
        let source = "  foo";
        let mut item = json!({"label":"x", "insertTextMode":2,
            "textEdit":{"range":range(0, 2, 5), "newText":"x\ny"}});
        for invalid in [
            json!(0),
            json!(3),
            json!(-1),
            json!(2.0),
            json!("2"),
            json!(true),
            json!({}),
            json!([]),
        ] {
            item["insertTextMode"] = invalid;
            assert!(item::edits(&item, source, position(0, 5), None).is_err());
        }
        item["insertTextMode"] = json!(2);
        item["command"] = json!({"command":"execute-me"});
        assert!(item::edits(&item, source, position(0, 5), None).is_err());
        item.as_object_mut().unwrap().remove("command");
        item["additionalTextEdits"] = json!([{"range":range(0, 2, 2),"newText":"overlap"}]);
        assert!(item::edits(&item, source, position(0, 5), None).is_err());
    }

    #[test]
    fn lazy_resolve_cannot_change_the_retained_indentation_policy() {
        let before = json!({"label":"x", "insertTextMode":1});
        let after = json!({"label":"x", "insertTextMode":2});
        assert!(item::resolved(&before, &after).is_err());
    }
}
