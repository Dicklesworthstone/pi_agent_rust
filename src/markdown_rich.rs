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
