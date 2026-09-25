use std::path::{Path, PathBuf};

use url::Url;

pub(super) fn next_non_whitespace_token(text: &str, start: usize) -> (&str, usize) {
    if start >= text.len() {
        return ("", text.len());
    }
    let mut end = text.len();
    for (offset, ch) in text[start..].char_indices() {
        if ch.is_whitespace() {
            end = start + offset;
            break;
        }
    }
    (&text[start..end], end)
}

pub(super) fn parse_quoted_file_ref(text: &str, start: usize) -> Option<(String, String, usize)> {
    let mut chars = text[start..].chars();
    let quote = chars.next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }

    let mut path = String::new();
    let mut escaped = false;
    let mut end = None;
    let after_quote = start + quote.len_utf8();

    for (offset, ch) in text[after_quote..].char_indices() {
        if escaped {
            if ch != quote && ch != '\\' {
                path.push('\\');
            }
            path.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == quote {
            end = Some(after_quote + offset);
            break;
        }
        path.push(ch);
    }

    let end = end?;
    let mut trailing = String::new();
    let mut token_end = end + quote.len_utf8();
    for ch in text[token_end..].chars() {
        if is_trailing_punct(ch) {
            trailing.push(ch);
            token_end += ch.len_utf8();
        } else {
            break;
        }
    }

    Some((path, trailing, token_end))
}

pub(super) fn strip_wrapping_quotes(input: &str) -> &str {
    let bytes = input.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &input[1..bytes.len() - 1];
        }
    }
    input
}

pub(super) fn looks_like_windows_path(input: &str) -> bool {
    let bytes = input.as_bytes();
    (bytes.len() >= 2 && bytes[1] == b':') || input.starts_with("\\\\")
}

pub(super) fn unescape_dragged_path(input: &str) -> String {
    if looks_like_windows_path(input) {
        return input.to_string();
    }

    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\'
            && let Some(next) = chars.peek().copied()
            && matches!(
                next,
                ' ' | '(' | ')' | '[' | ']' | '{' | '}' | '"' | '\'' | '\\'
            )
        {
            out.push(next);
            let _ = chars.next();
            continue;
        }
        out.push(ch);
    }
    out
}

pub(super) fn file_url_to_path(input: &str) -> Option<PathBuf> {
    if !input.starts_with("file://") {
        return None;
    }
    Url::parse(input).ok()?.to_file_path().ok()
}

pub(super) fn path_for_display(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd).map_or_else(
        |_| path.to_string_lossy().to_string(),
        |p| p.to_string_lossy().to_string(),
    )
}

pub(super) fn format_file_ref(path: &str) -> String {
    let needs_quotes =
        path.chars().any(char::is_whitespace) || path.chars().last().is_some_and(is_trailing_punct);

    if needs_quotes {
        if !path.contains('"') {
            format!("@\"{}\"", escape_quoted_file_ref(path, '"'))
        } else if !path.contains('\'') {
            format!("@'{}'", escape_quoted_file_ref(path, '\''))
        } else {
            format!("@\"{}\"", escape_quoted_file_ref(path, '"'))
        }
    } else {
        format!("@{path}")
    }
}

fn escape_quoted_file_ref(path: &str, quote: char) -> String {
    let mut escaped = String::with_capacity(path.len());
    for ch in path.chars() {
        if ch == '\\' || ch == quote {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

pub(super) fn split_trailing_punct(token: &str) -> (&str, &str) {
    let mut split = token.len();
    for (idx, ch) in token.char_indices().rev() {
        if is_trailing_punct(ch) {
            split = idx;
        } else {
            break;
        }
    }
    token.split_at(split)
}

const fn is_trailing_punct(ch: char) -> bool {
    matches!(
        ch,
        ',' | '.' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '"' | '\''
    )
}

pub(super) fn is_file_ref_boundary(text: &str, at: usize) -> bool {
    if at == 0 {
        return true;
    }
    let prev = text[..at].chars().last().unwrap_or(' ');
    prev.is_whitespace() || matches!(prev, '(' | '[' | '{' | '<' | '"' | '\'')
}

const fn is_linebreak(ch: char) -> bool {
    matches!(ch, '\n' | '\r')
}

const fn is_horizontal_whitespace(ch: char) -> bool {
    matches!(ch, ' ' | '\t')
}

fn trim_trailing_horizontal_whitespace(text: &mut String) {
    while text.chars().last().is_some_and(is_horizontal_whitespace) {
        text.pop();
    }
}

fn trailing_line_is_blank(text: &str) -> bool {
    if let Some((line_start, linebreak)) =
        text.char_indices().rev().find(|(_, ch)| is_linebreak(*ch))
    {
        let start = line_start + linebreak.len_utf8();
        return text[start..].chars().all(is_horizontal_whitespace);
    }

    text.chars().all(is_horizontal_whitespace)
}

fn consume_single_linebreak(text: &str, start: usize) -> usize {
    if start >= text.len() {
        return 0;
    }

    let Some(first) = text[start..].chars().next() else {
        return 0;
    };
    if !is_linebreak(first) {
        return 0;
    }

    let first_len = first.len_utf8();
    if first == '\r' && text[start + first_len..].starts_with('\n') {
        return first_len + '\n'.len_utf8();
    }

    first_len
}

/// Split `@file` references out of a message: returns the message with each
/// resolved reference removed (and its surrounding whitespace tidied) plus
/// the resolved paths, in order. `resolve` maps a written path to the path
/// to attach, or `None` to leave the `@token` in the text untouched. Shared
/// by both interactive stacks.
pub fn extract_file_references(
    message: &str,
    mut resolve: impl FnMut(&str) -> Option<String>,
) -> (String, Vec<String>) {
    let mut cleaned = String::with_capacity(message.len());
    let mut file_args = Vec::new();
    let mut idx = 0usize;

    while idx < message.len() {
        let ch = message[idx..].chars().next().unwrap_or(' ');
        if ch == '@' && is_file_ref_boundary(message, idx) {
            let token_start = idx + ch.len_utf8();
            let parsed = parse_quoted_file_ref(message, token_start);
            let (path, trailing, token_end) = parsed.unwrap_or_else(|| {
                let (token, token_end) = next_non_whitespace_token(message, token_start);
                let (path, trailing) = split_trailing_punct(token);
                (path.to_string(), trailing.to_string(), token_end)
            });

            if !path.is_empty()
                && let Some(resolved) = resolve(&path)
            {
                file_args.push(resolved);
                let mut next_idx = token_end;
                if !trailing.is_empty() {
                    trim_trailing_horizontal_whitespace(&mut cleaned);
                } else if message[next_idx..]
                    .chars()
                    .next()
                    .is_some_and(is_horizontal_whitespace)
                {
                    while message[next_idx..]
                        .chars()
                        .next()
                        .is_some_and(is_horizontal_whitespace)
                    {
                        next_idx += message[next_idx..].chars().next().map_or(0, char::len_utf8);
                    }
                } else if trailing_line_is_blank(&cleaned)
                    && message[next_idx..].chars().next().is_some_and(is_linebreak)
                {
                    trim_trailing_horizontal_whitespace(&mut cleaned);
                    next_idx += consume_single_linebreak(message, next_idx);
                }
                cleaned.push_str(&trailing);
                idx = next_idx;
                continue;
            }
        }

        cleaned.push(ch);
        idx += ch.len_utf8();
    }

    (cleaned, file_args)
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    // Only `file_url_to_path_valid` names a `PathBuf`, and its `file:///tmp/…`
    // fixture is Unix-shaped, so it is gated and this import with it.
    #[cfg(unix)]
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn parse_quoted_file_ref_preserves_windows_path_separators() {
        let text = "\"C:\\Program Files\\Pi\\agent.rs\".";
        let parsed = parse_quoted_file_ref(text, 0);
        assert_eq!(
            parsed,
            Some((
                "C:\\Program Files\\Pi\\agent.rs".to_string(),
                ".".to_string(),
                text.len()
            ))
        );
    }

    #[test]
    fn parse_quoted_file_ref_unescapes_only_quote_and_backslash() {
        let text = "\"foo\\\\bar\\\"baz.txt\"";
        let parsed = parse_quoted_file_ref(text, 0);
        assert_eq!(
            parsed,
            Some(("foo\\bar\"baz.txt".to_string(), String::new(), text.len()))
        );
    }

    #[test]
    fn strip_wrapping_quotes_double() {
        assert_eq!(strip_wrapping_quotes("\"hello\""), "hello");
    }

    #[test]
    fn strip_wrapping_quotes_single() {
        assert_eq!(strip_wrapping_quotes("'hello'"), "hello");
    }

    #[test]
    fn strip_wrapping_quotes_mismatched() {
        assert_eq!(strip_wrapping_quotes("\"hello'"), "\"hello'");
    }

    #[test]
    fn strip_wrapping_quotes_no_quotes() {
        assert_eq!(strip_wrapping_quotes("hello"), "hello");
    }

    #[test]
    fn strip_wrapping_quotes_empty() {
        assert_eq!(strip_wrapping_quotes(""), "");
    }

    #[test]
    fn strip_wrapping_quotes_single_char() {
        assert_eq!(strip_wrapping_quotes("\""), "\"");
    }

    #[test]
    fn windows_path_drive_letter() {
        assert!(looks_like_windows_path("C:\\Users\\foo"));
        assert!(looks_like_windows_path("D:file.txt"));
    }

    #[test]
    fn windows_path_unc() {
        assert!(looks_like_windows_path("\\\\server\\share"));
    }

    #[test]
    fn unix_path_not_windows() {
        assert!(!looks_like_windows_path("/home/user/file"));
        assert!(!looks_like_windows_path("relative/path"));
    }

    #[test]
    fn unescape_dragged_path_backslash_space() {
        assert_eq!(unescape_dragged_path("my\\ file.txt"), "my file.txt");
    }

    #[test]
    fn unescape_dragged_path_backslash_parens() {
        assert_eq!(unescape_dragged_path("file\\(1\\).txt"), "file(1).txt");
    }

    #[test]
    fn unescape_dragged_path_windows_preserved() {
        assert_eq!(unescape_dragged_path("C:\\Users\\foo"), "C:\\Users\\foo");
    }

    #[test]
    fn unescape_dragged_path_no_escapes() {
        assert_eq!(unescape_dragged_path("simple.txt"), "simple.txt");
    }

    #[test]
    #[cfg(unix)]
    fn file_url_to_path_valid() {
        let result = file_url_to_path("file:///tmp/test.txt");
        assert_eq!(result, Some(PathBuf::from("/tmp/test.txt")));
    }

    #[test]
    fn file_url_to_path_not_file_url() {
        assert!(file_url_to_path("https://example.com").is_none());
        assert!(file_url_to_path("/tmp/test.txt").is_none());
    }

    #[test]
    fn format_file_ref_simple() {
        assert_eq!(format_file_ref("src/main.rs"), "@src/main.rs");
    }

    #[test]
    fn format_file_ref_with_spaces() {
        assert_eq!(format_file_ref("my file.rs"), "@\"my file.rs\"");
    }

    #[test]
    fn format_file_ref_with_double_quotes_in_path() {
        assert_eq!(format_file_ref("my \"file\".rs"), "@'my \"file\".rs'");
    }

    #[test]
    fn format_file_ref_with_both_quotes() {
        assert_eq!(
            format_file_ref("it's a \"file\" name.rs"),
            "@\"it's a \\\"file\\\" name.rs\""
        );
    }

    #[test]
    fn format_file_ref_escapes_backslashes_in_quoted_paths() {
        assert_eq!(
            format_file_ref("C:\\Program Files\\Pi\\"),
            "@\"C:\\\\Program Files\\\\Pi\\\\\""
        );
    }

    #[test]
    fn quoted_file_ref_formatter_round_trips_repeated_and_trailing_backslashes() {
        let path = "\\\\server\\share name\\";
        let formatted = format_file_ref(path);
        let parsed = parse_quoted_file_ref(&formatted, 1);
        assert_eq!(
            parsed,
            Some((path.to_string(), String::new(), formatted.len()))
        );
    }

    #[test]
    fn split_trailing_punct_period() {
        assert_eq!(split_trailing_punct("file.rs."), ("file.rs", "."));
    }

    #[test]
    fn split_trailing_punct_comma() {
        assert_eq!(split_trailing_punct("word,"), ("word", ","));
    }

    #[test]
    fn split_trailing_punct_no_trailing() {
        assert_eq!(split_trailing_punct("word"), ("word", ""));
    }

    #[test]
    fn split_trailing_punct_all_punct() {
        assert_eq!(split_trailing_punct("!?"), ("", "!?"));
    }

    #[test]
    fn split_trailing_punct_empty() {
        assert_eq!(split_trailing_punct(""), ("", ""));
    }

    #[test]
    fn file_ref_boundary_at_start() {
        assert!(is_file_ref_boundary("@file", 0));
    }

    #[test]
    fn file_ref_boundary_after_space() {
        assert!(is_file_ref_boundary("see @file", 4));
    }

    #[test]
    fn file_ref_boundary_after_paren() {
        assert!(is_file_ref_boundary("(@file)", 1));
    }

    #[test]
    fn file_ref_boundary_mid_word() {
        assert!(!is_file_ref_boundary("foo@bar", 3));
    }

    #[test]
    fn path_for_display_within_cwd() {
        let cwd = Path::new("/home/user/project");
        let path = Path::new("/home/user/project/src/main.rs");
        assert_eq!(path_for_display(path, cwd), "src/main.rs");
    }

    #[test]
    fn path_for_display_outside_cwd() {
        let cwd = Path::new("/home/user/project");
        let path = Path::new("/tmp/file.txt");
        assert_eq!(path_for_display(path, cwd), "/tmp/file.txt");
    }

    #[test]
    fn path_for_display_same_as_cwd() {
        let cwd = Path::new("/home/user");
        let path = Path::new("/home/user");
        assert_eq!(path_for_display(path, cwd), "");
    }

    #[test]
    fn next_token_basic() {
        let (token, end) = next_non_whitespace_token("hello world", 0);
        assert_eq!(token, "hello");
        assert_eq!(end, 5);
    }

    #[test]
    fn next_token_past_end() {
        let (token, end) = next_non_whitespace_token("abc", 10);
        assert_eq!(token, "");
        assert_eq!(end, 3);
    }

    #[test]
    fn next_token_last_word() {
        let (token, end) = next_non_whitespace_token("a bc", 2);
        assert_eq!(token, "bc");
        assert_eq!(end, 4);
    }
}
