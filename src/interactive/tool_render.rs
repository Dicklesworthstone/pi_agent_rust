use crate::model::ContentBlock;
use crate::theme::TuiStyles;
use serde_json::Value;

use super::conversation::tool_content_blocks_to_text;

pub(super) fn format_tool_output(
    content: &[ContentBlock],
    details: Option<&Value>,
    show_images: bool,
) -> Option<String> {
    let mut output = tool_content_blocks_to_text(content, show_images);
    if let Some(details) = details {
        // `edit` includes a unified diff-like view in `details.diff`. Surface it in the TUI
        // even when the primary content is a short "success" message.
        if let Some(diff) = details.get("diff").and_then(Value::as_str) {
            let diff = diff.trim();
            if !diff.is_empty() {
                if !output.trim().is_empty() {
                    output.push_str("\n\n");
                }
                output.push_str("Diff:\n");
                output.push_str(diff);
            }
        } else if output.trim().is_empty() {
            output = pretty_json(details);
        }
    } else if output.trim().is_empty() {
        // No primary content and no details payload.
    }
    if output.trim().is_empty() {
        None
    } else {
        Some(output)
    }
}

/// Maximum number of diff lines to show before truncating.
const DIFF_TRUNCATE_THRESHOLD: usize = 50;
/// Lines to show at the beginning of a truncated diff.
const DIFF_TRUNCATE_HEAD: usize = 20;
/// Lines to show at the end of a truncated diff.
const DIFF_TRUNCATE_TAIL: usize = 10;

pub(super) fn render_tool_message(text: &str, styles: &TuiStyles) -> String {
    let mut out = String::new();
    let mut diff_lines: Vec<&str> = Vec::new();

    // First pass: separate pre-diff text from diff lines.
    let mut pre_diff_lines: Vec<&str> = Vec::new();
    let mut found_diff_header = false;
    for line in text.lines() {
        if found_diff_header {
            diff_lines.push(line);
        } else if line.trim() == "Diff:" {
            found_diff_header = true;
        } else {
            pre_diff_lines.push(line);
        }
    }

    // Render pre-diff content (tool name, success message, etc.)
    for (idx, line) in pre_diff_lines.iter().enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        out.push_str(&styles.muted.render(line));
    }

    if !found_diff_header {
        return out;
    }

    // Extract file path from "Successfully replaced text in {path}." pattern.
    let file_path = pre_diff_lines.iter().find_map(|line| {
        line.strip_prefix("Successfully replaced text in ")
            .and_then(|rest| rest.strip_suffix('.'))
    });

    // Render diff header.
    if !out.is_empty() {
        out.push('\n');
    }
    if let Some(path) = file_path {
        out.push_str(&styles.muted_bold.render(&format!("@@ {path} @@")));
    } else {
        out.push_str(&styles.muted_bold.render("Diff:"));
    }

    // Truncate large diffs.
    let total_changed = diff_lines
        .iter()
        .filter(|l| l.starts_with('+') || l.starts_with('-'))
        .count();
    let truncated = total_changed > DIFF_TRUNCATE_THRESHOLD;
    let visible_lines = if truncated {
        // Show head + tail with separator.
        let mut visible = Vec::with_capacity(DIFF_TRUNCATE_HEAD + DIFF_TRUNCATE_TAIL + 1);
        visible.extend_from_slice(&diff_lines[..DIFF_TRUNCATE_HEAD.min(diff_lines.len())]);
        let omitted = diff_lines
            .len()
            .saturating_sub(DIFF_TRUNCATE_HEAD + DIFF_TRUNCATE_TAIL);
        if omitted > 0 {
            // We'll render a separator inline.
            visible.push(""); // placeholder for separator
            let tail_start = diff_lines.len().saturating_sub(DIFF_TRUNCATE_TAIL);
            visible.extend_from_slice(&diff_lines[tail_start..]);
        }
        visible
    } else {
        diff_lines
    };

    // Collect diff lines for word-level highlighting.
    render_diff_lines(&visible_lines, truncated, styles, &mut out);

    out
}

/// Render diff lines with word-level highlighting for paired -/+ lines.
fn render_diff_lines(lines: &[&str], truncated: bool, styles: &TuiStyles, out: &mut String) {
    let mut i = 0;
    let mut rendered_separator = false;
    while i < lines.len() {
        let line = lines[i];

        // Handle truncation separator placeholder.
        if truncated && !rendered_separator && line.is_empty() && i > 0 {
            out.push('\n');
            out.push_str(&styles.muted.render("  ... (diff truncated) ..."));
            rendered_separator = true;
            i += 1;
            continue;
        }

        out.push('\n');

        // Check for paired -/+ lines for word-level highlighting.
        if line.starts_with('-') {
            // Look ahead for a matching + line.
            if i + 1 < lines.len() && lines[i + 1].starts_with('+') {
                let removed = line;
                let added = lines[i + 1];
                render_word_diff_pair(removed, added, styles, out);
                i += 2;
                continue;
            }
            out.push_str(&styles.error_bold.render(line));
        } else if line.starts_with('+') {
            out.push_str(&styles.success_bold.render(line));
        } else {
            out.push_str(&styles.muted.render(line));
        }

        i += 1;
    }
}

/// Render a paired removed/added line with word-level change highlighting.
///
/// The line format from `generate_diff_string` is: `-NN content` / `+NN content`.
/// We diff the content portions and bold just the changed segments.
fn render_word_diff_pair(removed: &str, added: &str, styles: &TuiStyles, out: &mut String) {
    // Extract the prefix (e.g. "-  3 ") and the content after it.
    let (rem_prefix, rem_content) = split_diff_prefix(removed);
    let (add_prefix, add_content) = split_diff_prefix(added);

    // If either line has no content (just a prefix), fall back to simple coloring.
    if rem_content.is_empty() || add_content.is_empty() {
        out.push_str(&styles.error_bold.render(removed));
        out.push('\n');
        out.push_str(&styles.success_bold.render(added));
        return;
    }

    // Compute word-level diff.
    let diff = similar::TextDiff::from_words(rem_content, add_content);

    // Render removed line with deletions highlighted.
    out.push_str(&styles.error_bold.render(rem_prefix));
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Delete => {
                // Bold + underline for the specific changed text.
                let styled = styles.error_bold.clone().underline();
                out.push_str(&styled.render(change.value()));
            }
            similar::ChangeTag::Equal => {
                out.push_str(&styles.error_bold.render(change.value()));
            }
            similar::ChangeTag::Insert => {} // skip insertions on removed line
        }
    }

    // Render added line with insertions highlighted.
    out.push('\n');
    out.push_str(&styles.success_bold.render(add_prefix));
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => {
                let styled = styles.success_bold.clone().underline();
                out.push_str(&styled.render(change.value()));
            }
            similar::ChangeTag::Equal => {
                out.push_str(&styles.success_bold.render(change.value()));
            }
            similar::ChangeTag::Delete => {} // skip deletions on added line
        }
    }
}

/// Split a diff line like `"-  3 content here"` into prefix `"-  3 "` and content `"content here"`.
pub(super) const fn split_diff_prefix(line: &str) -> (&str, &str) {
    // Format: [+-] then line number with spaces, then a space, then content.
    // E.g., "+  3 let x = 1;" => prefix "+  3 ", content "let x = 1;"
    // Or "- 12 old text"    => prefix "- 12 ", content "old text"
    let bytes = line.as_bytes();
    if bytes.len() < 3 || bytes[1] != b' ' {
        return (line, "");
    }

    let mut i = 2;
    // Skip padding spaces before line number
    while i < bytes.len() && bytes[i] == b' ' {
        i += 1;
    }

    let digits_start = i;
    // Skip digits of the line number
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }

    // Must have found digits, and the next character must be a single space separator
    if i > digits_start && i < bytes.len() && bytes[i] == b' ' {
        let prefix_end = i + 1;
        return line.split_at(prefix_end);
    }

    (line, "")
}

pub(super) fn pretty_json(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TextContent;

    // ── split_diff_prefix ───────────────────────────────────────────────

    #[test]
    fn split_diff_prefix_removal_line() {
        let (prefix, content) = split_diff_prefix("-  3 let x = 1;");
        assert_eq!(prefix, "-  3 ");
        assert_eq!(content, "let x = 1;");
    }

    #[test]
    fn split_diff_prefix_addition_line() {
        let (prefix, content) = split_diff_prefix("+  3 let x = 2;");
        assert_eq!(prefix, "+  3 ");
        assert_eq!(content, "let x = 2;");
    }

    #[test]
    fn split_diff_prefix_double_digit_line_number() {
        let (prefix, content) = split_diff_prefix("- 12 old text");
        assert_eq!(prefix, "- 12 ");
        assert_eq!(content, "old text");
    }

    #[test]
    fn split_diff_prefix_short_line() {
        let (prefix, content) = split_diff_prefix("+");
        assert_eq!(prefix, "+");
        assert_eq!(content, "");
    }

    #[test]
    fn split_diff_prefix_empty() {
        let (prefix, content) = split_diff_prefix("");
        assert_eq!(prefix, "");
        assert_eq!(content, "");
    }

    #[test]
    fn split_diff_prefix_context_line() {
        let (prefix, content) = split_diff_prefix("  5 unchanged");
        assert_eq!(prefix, "  5 ");
        assert_eq!(content, "unchanged");
    }

    // ── pretty_json ─────────────────────────────────────────────────────

    #[test]
    fn pretty_json_object() {
        let value = serde_json::json!({"key": "value"});
        let output = pretty_json(&value);
        assert!(output.contains("\"key\""));
        assert!(output.contains("\"value\""));
        assert!(output.contains('\n'));
    }

    #[test]
    fn pretty_json_string() {
        let value = serde_json::json!("hello");
        assert_eq!(pretty_json(&value), "\"hello\"");
    }

    #[test]
    fn pretty_json_null() {
        let value = serde_json::json!(null);
        assert_eq!(pretty_json(&value), "null");
    }

    // ── format_tool_output ──────────────────────────────────────────────

    #[test]
    fn format_tool_output_text_only() {
        let blocks = vec![ContentBlock::Text(TextContent::new("Success".to_string()))];
        let result = format_tool_output(&blocks, None, true);
        assert_eq!(result, Some("Success".to_string()));
    }

    #[test]
    fn format_tool_output_empty_returns_none() {
        let blocks: Vec<ContentBlock> = Vec::new();
        assert!(format_tool_output(&blocks, None, true).is_none());
    }

    #[test]
    fn format_tool_output_with_diff_in_details() {
        let blocks = vec![ContentBlock::Text(TextContent::new(
            "Successfully replaced text in foo.rs.".to_string(),
        ))];
        let details = serde_json::json!({"diff": "-old\n+new"});
        let result = format_tool_output(&blocks, Some(&details), true).unwrap();
        assert!(result.contains("Diff:"));
        assert!(result.contains("-old"));
        assert!(result.contains("+new"));
    }

    #[test]
    fn format_tool_output_empty_content_shows_details_json() {
        let blocks: Vec<ContentBlock> = Vec::new();
        let details = serde_json::json!({"status": "ok"});
        let result = format_tool_output(&blocks, Some(&details), true).unwrap();
        assert!(result.contains("status"));
        assert!(result.contains("ok"));
    }

    #[test]
    fn format_tool_output_empty_diff_ignored() {
        let blocks = vec![ContentBlock::Text(TextContent::new("Done".to_string()))];
        let details = serde_json::json!({"diff": "  "});
        let result = format_tool_output(&blocks, Some(&details), true).unwrap();
        assert!(!result.contains("Diff:"));
        assert_eq!(result, "Done");
    }
}
