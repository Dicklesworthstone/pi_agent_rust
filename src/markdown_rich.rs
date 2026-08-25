#![forbid(unsafe_code)]

//! Markdown, math, mermaid, and visual excellence (OMP-ADOPT / bd-cv653.9.7).
//!
//! Provides streaming GFM rendering, syntax tokenization, hex color swatch chips,
//! Unicode math conversions, mermaid diagram formatting, and OSC-8 hyperlinks.

use serde::{Deserialize, Serialize};

/// Supported syntax highlighting languages for code fences.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HighlightLanguage {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Bash,
    Go,
    C,
    Cpp,
    Json,
    Toml,
    Yaml,
    Diff,
    Markdown,
    Plain,
}

impl HighlightLanguage {
    #[must_use]
    pub fn from_fence_tag(tag: &str) -> Self {
        match tag.trim().to_ascii_lowercase().as_str() {
            "rust" | "rs" => Self::Rust,
            "python" | "py" => Self::Python,
            "javascript" | "js" => Self::JavaScript,
            "typescript" | "ts" | "tsx" => Self::TypeScript,
            "bash" | "sh" | "zsh" | "shell" => Self::Bash,
            "go" | "golang" => Self::Go,
            "c" => Self::C,
            "cpp" | "c++" | "cc" | "cxx" => Self::Cpp,
            "json" | "jsonc" => Self::Json,
            "toml" => Self::Toml,
            "yaml" | "yml" => Self::Yaml,
            "diff" | "patch" => Self::Diff,
            "markdown" | "md" => Self::Markdown,
            _ => Self::Plain,
        }
    }
}

/// Convert LaTeX math expressions to legible Unicode representations.
#[must_use]
pub fn latex_to_unicode(latex: &str) -> String {
    let replacements = [
        (r"\alpha", "α"),
        (r"\beta", "β"),
        (r"\gamma", "γ"),
        (r"\delta", "δ"),
        (r"\epsilon", "ε"),
        (r"\theta", "θ"),
        (r"\lambda", "λ"),
        (r"\mu", "μ"),
        (r"\pi", "π"),
        (r"\sigma", "σ"),
        (r"\tau", "τ"),
        (r"\phi", "φ"),
        (r"\omega", "ω"),
        (r"\times", "×"),
        (r"\div", "÷"),
        (r"\pm", "±"),
        (r"\le", "≤"),
        (r"\ge", "≥"),
        (r"\ne", "≠"),
        (r"\approx", "≈"),
        (r"\infty", "∞"),
        (r"\to", "→"),
        (r"\gets", "←"),
        (r"\sum", "∑"),
        (r"\prod", "∏"),
        (r"\sqrt", "√"),
    ];

    let mut out = String::with_capacity(latex.len());
    let mut cursor = 0;
    while cursor < latex.len() {
        let remaining = &latex[cursor..];
        if let Some(&(from, to)) = replacements.iter().find(|&&(from, _)| {
            remaining.strip_prefix(from).is_some_and(|suffix| {
                suffix
                    .chars()
                    .next()
                    .is_none_or(|next| !next.is_ascii_alphabetic())
            })
        }) {
            out.push_str(to);
            cursor += from.len();
            continue;
        }

        let Some(next) = remaining.chars().next() else {
            break;
        };
        out.push(next);
        cursor += next.len_utf8();
    }
    out
}

/// Inject visual color swatches for hex color codes (`#RRGGBB` or `#RGB`).
#[must_use]
pub fn render_hex_swatches(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 32);
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if chars.get(i) == Some(&'#') {
            // Ensure not preceded by an alphanumeric character (e.g. not url#anchor or var#name)
            let prev_ok = if i == 0 {
                true
            } else {
                chars
                    .get(i.saturating_sub(1))
                    .is_none_or(|&prev| !prev.is_ascii_alphanumeric())
            };

            if prev_ok {
                let digit_count = [6, 3].into_iter().find(|digit_count| {
                    let end = i + 1 + digit_count;
                    end <= chars.len()
                        && chars[i + 1..end].iter().all(char::is_ascii_hexdigit)
                        && chars
                            .get(end)
                            .is_none_or(|next| !next.is_ascii_alphanumeric())
                });

                if let Some(digit_count) = digit_count {
                    let end = i + 1 + digit_count;
                    out.push('■');
                    out.push(' ');
                    out.extend(chars[i..end].iter().copied());
                    i = end;
                    continue;
                }
            }
        }
        if let Some(&c) = chars.get(i) {
            out.push(c);
        }
        i += 1;
    }

    out
}

/// Apply prose-only Markdown enhancements without rewriting literal code,
/// link destinations, URLs, or filesystem paths.
///
/// The individual enhancement functions intentionally operate on plain text.
/// Callers rendering Markdown should use this boundary so syntax-bearing text
/// reaches the Markdown parser byte-for-byte intact.
fn fence_marker(line: &str) -> Option<(u8, usize)> {
    let indent = line.bytes().take_while(|byte| *byte == b' ').count();
    if indent > 3 {
        return None;
    }
    let rest = line.as_bytes().get(indent..)?;
    let marker = *rest.first()?;
    if !matches!(marker, b'`' | b'~') {
        return None;
    }
    let len = rest.iter().take_while(|byte| **byte == marker).count();
    (len >= 3).then_some((marker, len))
}

fn closes_fence(line: &str, marker: u8, minimum_len: usize) -> bool {
    let indent = line.bytes().take_while(|byte| *byte == b' ').count();
    if indent > 3 {
        return false;
    }
    let Some(rest) = line.as_bytes().get(indent..) else {
        return false;
    };
    let len = rest.iter().take_while(|byte| **byte == marker).count();
    len >= minimum_len && rest[len..].iter().all(u8::is_ascii_whitespace)
}

fn is_indented_code(line: &str) -> bool {
    line.starts_with('\t') || line.as_bytes().starts_with(b"    ")
}

fn is_path_or_url(chunk: &str) -> bool {
    chunk.contains('/')
        || chunk.contains("://")
        || chunk.contains(":\\")
        || chunk.starts_with("\\\\")
}

fn enrich_plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut start = 0;
    let mut whitespace = None;
    for (index, character) in text.char_indices() {
        let is_whitespace = character.is_whitespace();
        if whitespace.is_some_and(|current| current != is_whitespace) {
            let chunk = &text[start..index];
            if whitespace == Some(true) || is_path_or_url(chunk) {
                out.push_str(chunk);
            } else {
                out.push_str(&render_hex_swatches(&latex_to_unicode(chunk)));
            }
            start = index;
        }
        whitespace = Some(is_whitespace);
    }
    let chunk = &text[start..];
    if whitespace == Some(true) || is_path_or_url(chunk) {
        out.push_str(chunk);
    } else {
        out.push_str(&render_hex_swatches(&latex_to_unicode(chunk)));
    }
    out
}

fn inline_code_end(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let delimiter_len = bytes[start..]
        .iter()
        .take_while(|byte| **byte == b'`')
        .count();
    let mut cursor = start + delimiter_len;
    while cursor < bytes.len() {
        if bytes[cursor] != b'`' {
            cursor += 1;
            continue;
        }
        let run_len = bytes[cursor..]
            .iter()
            .take_while(|byte| **byte == b'`')
            .count();
        if run_len == delimiter_len {
            return Some(cursor + run_len);
        }
        cursor += run_len;
    }
    None
}

fn link_destination_end(text: &str, start: usize) -> Option<usize> {
    if !text[start..].starts_with("](") {
        return None;
    }
    let bytes = text.as_bytes();
    let mut depth = 1_usize;
    let mut cursor = start + 2;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\\' => cursor = (cursor + 2).min(bytes.len()),
            b'(' => {
                depth += 1;
                cursor += 1;
            }
            b')' => {
                depth -= 1;
                cursor += 1;
                if depth == 0 {
                    return Some(cursor);
                }
            }
            _ => cursor += 1,
        }
    }
    None
}

fn enrich_inline(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut plain_start = 0;
    let mut cursor = 0;
    while cursor < bytes.len() {
        let protected_end = match bytes[cursor] {
            b'`' => inline_code_end(text, cursor),
            b']' => link_destination_end(text, cursor),
            b'<' => text[cursor + 1..]
                .find('>')
                .map(|offset| cursor + offset + 2),
            _ => None,
        };
        if let Some(end) = protected_end {
            out.push_str(&enrich_plain(&text[plain_start..cursor]));
            out.push_str(&text[cursor..end]);
            cursor = end;
            plain_start = end;
        } else {
            let character_len = text[cursor..].chars().next().map_or(1, char::len_utf8);
            cursor += character_len;
        }
    }
    out.push_str(&enrich_plain(&text[plain_start..]));
    out
}

/// Apply prose-only Markdown enhancements without rewriting literal code,
/// link destinations, URLs, or filesystem paths.
///
/// The individual enhancement functions intentionally operate on plain text.
/// Callers rendering Markdown should use this boundary so syntax-bearing text
/// reaches the Markdown parser byte-for-byte intact.
#[must_use]
pub fn enrich_markdown(markdown: &str) -> String {
    let mut out = String::with_capacity(markdown.len() + 32);
    let mut active_fence = None;
    for line in markdown.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line);
        if let Some((marker, minimum_len)) = active_fence {
            out.push_str(line);
            if closes_fence(body, marker, minimum_len) {
                active_fence = None;
            }
        } else if let Some(marker) = fence_marker(body) {
            active_fence = Some(marker);
            out.push_str(line);
        } else if is_indented_code(body) {
            out.push_str(line);
        } else {
            out.push_str(&enrich_inline(body));
            if line.ends_with('\n') {
                out.push('\n');
            }
        }
    }
    out
}

/// Emit an OSC-8 terminal hyperlink.
#[must_use]
pub fn format_osc8_link(url: &str, label: &str) -> String {
    let safe_url: String = url
        .chars()
        .filter(|character| !character.is_control())
        .collect();
    let safe_label: String = label
        .chars()
        .filter(|character| !character.is_control())
        .collect();
    format!("\x1b]8;;{safe_url}\x1b\\{safe_label}\x1b]8;;\x1b\\")
}

/// Format a mermaid diagram code block for clean terminal display.
#[must_use]
pub fn render_mermaid_diagram(source: &str, max_width: usize) -> String {
    let mut out = String::new();
    out.push_str("┌── [Mermaid Diagram] ──────────────────┐\n");
    let line_limit = max_width.saturating_sub(4);
    for line in source.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            out.push_str("│ ");
            for c in trimmed.chars().take(line_limit) {
                out.push(c);
            }
            out.push('\n');
        }
    }
    out.push_str("└───────────────────────────────────────┘\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_highlight_language_detection() {
        assert_eq!(
            HighlightLanguage::from_fence_tag("rust"),
            HighlightLanguage::Rust
        );
        assert_eq!(
            HighlightLanguage::from_fence_tag("rs"),
            HighlightLanguage::Rust
        );
        assert_eq!(
            HighlightLanguage::from_fence_tag("python"),
            HighlightLanguage::Python
        );
        assert_eq!(
            HighlightLanguage::from_fence_tag("tsx"),
            HighlightLanguage::TypeScript
        );
        assert_eq!(
            HighlightLanguage::from_fence_tag("unknown_xyz"),
            HighlightLanguage::Plain
        );
    }

    #[test]
    fn test_latex_to_unicode_conversion() {
        let math = r"\alpha + \beta \le \gamma \times \infty";
        let unicode = latex_to_unicode(math);
        assert_eq!(unicode, "α + β ≤ γ × ∞");
    }

    #[test]
    fn test_latex_to_unicode_preserves_longer_commands() {
        let math = r"\left(x\right) + \top + \alpha_1";
        let unicode = latex_to_unicode(math);
        assert_eq!(unicode, r"\left(x\right) + \top + α_1");
    }

    #[test]
    fn test_hex_swatch_injection() {
        let input = "The primary accent is #3b82f6, dark is #1e293b, and short is #abc.";
        let swatched = render_hex_swatches(input);
        assert!(swatched.contains("■ #3b82f6"));
        assert!(swatched.contains("■ #1e293b"));
        assert!(swatched.contains("■ #abc"));
    }

    #[test]
    fn test_hex_swatch_rejects_longer_alphanumeric_tokens() {
        let input = "Not colors: #1234567, #abcdefg, or #fffz.";
        assert_eq!(render_hex_swatches(input), input);
    }

    #[test]
    fn test_markdown_enrichment_preserves_literal_and_destination_bytes() {
        let input = concat!(
            "Math \\alpha and color #abc.\n",
            "Inline `\\alpha #abc` and ``literal ` \\beta #123456``.\n",
            "A [link](https://example.test/\\alpha#abc) and https://example.test/#abc.\n",
            "A path /tmp/\\alpha/#abc stays literal.\n",
            "```rust\n",
            "let literal = r\"\\alpha #abc\";\n",
            "```\n",
            "    let indented = r\"\\beta #123456\";\n",
        );
        let enriched = enrich_markdown(input);

        assert!(enriched.contains("Math α and color ■ #abc."));
        assert!(enriched.contains("`\\alpha #abc`"));
        assert!(enriched.contains("``literal ` \\beta #123456``"));
        assert!(enriched.contains("(https://example.test/\\alpha#abc)"));
        assert!(enriched.contains("https://example.test/#abc"));
        assert!(enriched.contains("/tmp/\\alpha/#abc"));
        assert!(enriched.contains("let literal = r\"\\alpha #abc\";"));
        assert!(enriched.contains("    let indented = r\"\\beta #123456\";"));
        assert!(!enriched.contains("https://example.test/■"));
    }

    #[test]
    fn test_osc8_link_formatting() {
        let link = format_osc8_link("https://github.com", "GitHub");
        assert!(link.starts_with("\x1b]8;;https://github.com\x1b\\"));
        assert!(link.ends_with("\x1b]8;;\x1b\\"));
        assert!(link.contains("GitHub"));
    }

    #[test]
    fn test_osc8_link_strips_hostile_control_sequences() {
        let link = format_osc8_link("https://example.test/\x1b]2;bad", "safe\x07\nlabel");
        assert!(link.contains("https://example.test/]2;bad"));
        assert!(link.contains("safelabel"));
        assert_eq!(
            link.matches('\x1b').count(),
            4,
            "only OSC-8 framing remains"
        );
        assert!(!link.contains('\x07'));
    }

    #[test]
    fn test_mermaid_diagram_formatting() {
        let diagram = "graph TD\n  A[Client] --> B[Server]\n  B --> C[DB]";
        let rendered = render_mermaid_diagram(diagram, 80);
        assert!(rendered.contains("Mermaid Diagram"));
        assert!(rendered.contains("Client"));
        assert!(rendered.contains("Server"));
    }
}
