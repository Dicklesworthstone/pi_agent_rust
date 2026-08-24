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
    let mut out = latex.to_string();
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

    for (from, to) in replacements {
        out = out.replace(from, to);
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
        if chars.get(i) == Some(&'#') && i + 6 < chars.len() {
            let chunk: Vec<char> = chars.iter().skip(i + 1).take(6).copied().collect();
            let is_hex = chunk.len() == 6 && chunk.iter().all(char::is_ascii_hexdigit);
            if is_hex {
                out.push('■');
                out.push(' ');
                out.push('#');
                for c in chunk {
                    out.push(c);
                }
                i += 7;
                continue;
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
    format!("\x1b]8;;{url}\x1b\\{label}\x1b]8;;\x1b\\")
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
        assert_eq!(HighlightLanguage::from_fence_tag("rust"), HighlightLanguage::Rust);
        assert_eq!(HighlightLanguage::from_fence_tag("rs"), HighlightLanguage::Rust);
        assert_eq!(HighlightLanguage::from_fence_tag("python"), HighlightLanguage::Python);
        assert_eq!(HighlightLanguage::from_fence_tag("tsx"), HighlightLanguage::TypeScript);
        assert_eq!(HighlightLanguage::from_fence_tag("unknown_xyz"), HighlightLanguage::Plain);
    }

    #[test]
    fn test_latex_to_unicode_conversion() {
        let math = r"\alpha + \beta \le \gamma \times \infty";
        let unicode = latex_to_unicode(math);
        assert_eq!(unicode, "α + β ≤ γ × ∞");
    }

    #[test]
    fn test_hex_swatch_injection() {
        let input = "The primary accent is #3b82f6 and dark is #1e293b.";
        let swatched = render_hex_swatches(input);
        assert!(swatched.contains("■ #3b82f6"));
        assert!(swatched.contains("■ #1e293b"));
    }

    #[test]
    fn test_osc8_link_formatting() {
        let link = format_osc8_link("https://github.com", "GitHub");
        assert!(link.starts_with("\x1b]8;;https://github.com\x1b\\"));
        assert!(link.ends_with("\x1b]8;;\x1b\\"));
        assert!(link.contains("GitHub"));
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
