//! Terminal UI components using rich_rust.
//!
//! This module provides the interactive terminal interface for Pi,
//! built on rich_rust for beautiful markup-based output.

use std::io::{self, IsTerminal, Write};

use rich_rust::prelude::*;
use rich_rust::renderables::{Markdown, Syntax};
use rich_rust::segment::Segment;

/// Pi's console wrapper providing styled terminal output.
pub struct PiConsole {
    console: Console,
    is_tty: bool,
}

impl PiConsole {
    /// Create a new Pi console with auto-detected terminal capabilities.
    pub fn new() -> Self {
        Self::new_with_theme(None)
    }

    /// Create a new Pi console with an optional theme.
    pub fn new_with_theme(_theme: Option<crate::theme::Theme>) -> Self {
        let is_tty = io::stdout().is_terminal();
        let console = Console::builder().markup(is_tty).emoji(is_tty).build();

        Self { console, is_tty }
    }

    /// Create a console with forced color output (for testing).
    pub fn with_color() -> Self {
        Self {
            console: Console::builder()
                .markup(true)
                .emoji(true)
                .file(Box::new(io::sink()))
                .build(),
            is_tty: true,
        }
    }

    /// Check if we're running in a terminal.
    pub const fn is_terminal(&self) -> bool {
        self.is_tty
    }

    /// Get the terminal width.
    pub fn width(&self) -> usize {
        self.console.width()
    }

    // -------------------------------------------------------------------------
    // Text Output
    // -------------------------------------------------------------------------

    /// Print plain text without any styling.
    pub fn print_plain(&self, text: &str) {
        print!("{text}");
        let _ = io::stdout().flush();
    }

    /// Print text with rich markup (if TTY).
    pub fn print_markup(&self, markup: &str) {
        if self.is_tty {
            self.console.print(markup);
        } else {
            // Strip markup for non-TTY
            print!("{}", strip_markup(markup));
            let _ = io::stdout().flush();
        }
    }

    /// Print a newline.
    pub fn newline(&self) {
        println!();
    }

    /// Render Markdown (TTY → styled output; non-TTY → raw Markdown).
    pub fn render_markdown(&self, markdown: &str) {
        if self.is_tty {
            let mut segments = render_markdown_with_syntax(markdown, self.width());
            let mut ends_with_newline = false;
            for segment in segments.iter().rev() {
                let text = segment.text.as_ref();
                if text.is_empty() {
                    continue;
                }
                ends_with_newline = text.ends_with('\n');
                break;
            }
            if !ends_with_newline {
                segments.push(Segment::plain("\n"));
            }
            self.console.print_segments(&segments);
        } else {
            print!("{markdown}");
            if !markdown.ends_with('\n') {
                println!();
            }
            let _ = io::stdout().flush();
        }
    }

    // -------------------------------------------------------------------------
    // Agent Event Rendering
    // -------------------------------------------------------------------------

    /// Render streaming text from the assistant.
    pub fn render_text_delta(&self, text: &str) {
        print!("{text}");
        let _ = io::stdout().flush();
    }

    /// Render streaming thinking text (dimmed).
    pub fn render_thinking_delta(&self, text: &str) {
        if self.is_tty {
            // Dim style for thinking
            print!("\x1b[2m{text}\x1b[0m");
        } else {
            print!("{text}");
        }
        let _ = io::stdout().flush();
    }

    /// Render the start of a thinking block.
    pub fn render_thinking_start(&self) {
        if self.is_tty {
            self.print_markup("\n[dim italic]Thinking...[/]\n");
        }
    }

    /// Render the end of a thinking block.
    pub fn render_thinking_end(&self) {
        if self.is_tty {
            self.print_markup("[/dim]\n");
        }
    }

    /// Render tool execution start.
    pub fn render_tool_start(&self, name: &str, _input: &str) {
        if self.is_tty {
            self.print_markup(&format!("\n[bold yellow][[Running {name}...]][/]\n"));
        }
    }

    /// Render tool execution end.
    pub fn render_tool_end(&self, name: &str, is_error: bool) {
        if self.is_tty {
            if is_error {
                self.print_markup(&format!("[bold red][[{name} failed]][/]\n\n"));
            } else {
                self.print_markup(&format!("[bold green][[{name} done]][/]\n\n"));
            }
        }
    }

    /// Render an error message.
    pub fn render_error(&self, error: &str) {
        if self.is_tty {
            self.print_markup(&format!("\n[bold red]Error:[/] {error}\n"));
        } else {
            eprintln!("\nError: {error}");
        }
    }

    /// Render a warning message.
    pub fn render_warning(&self, warning: &str) {
        if self.is_tty {
            self.print_markup(&format!("[bold yellow]Warning:[/] {warning}\n"));
        } else {
            eprintln!("Warning: {warning}");
        }
    }

    /// Render a success message.
    pub fn render_success(&self, message: &str) {
        if self.is_tty {
            self.print_markup(&format!("[bold green]{message}[/]\n"));
        } else {
            println!("{message}");
        }
    }

    /// Render an info message.
    pub fn render_info(&self, message: &str) {
        if self.is_tty {
            self.print_markup(&format!("[bold blue]{message}[/]\n"));
        } else {
            println!("{message}");
        }
    }

    // -------------------------------------------------------------------------
    // Structured Output
    // -------------------------------------------------------------------------

    /// Render a panel with a title.
    pub fn render_panel(&self, content: &str, title: &str) {
        if self.is_tty {
            let panel = Panel::from_text(content)
                .title(title)
                .border_style(Style::parse("cyan").unwrap_or_default());
            self.console.print_renderable(&panel);
        } else {
            println!("--- {title} ---");
            println!("{content}");
            println!("---");
        }
    }

    /// Render a table.
    pub fn render_table(&self, headers: &[&str], rows: &[Vec<&str>]) {
        if self.is_tty {
            let mut table = Table::new().header_style(Style::parse("bold").unwrap_or_default());
            for header in headers {
                table = table.with_column(Column::new(*header));
            }
            for row in rows {
                table.add_row_cells(row.iter().copied());
            }
            self.console.print_renderable(&table);
        } else {
            // Simple text table for non-TTY
            println!("{}", headers.join("\t"));
            for row in rows {
                println!("{}", row.join("\t"));
            }
        }
    }

    /// Render a horizontal rule.
    pub fn render_rule(&self, title: Option<&str>) {
        if self.is_tty {
            let rule = title.map_or_else(Rule::new, Rule::with_title);
            self.console.print_renderable(&rule);
        } else if let Some(t) = title {
            println!("--- {t} ---");
        } else {
            println!("---");
        }
    }

    // -------------------------------------------------------------------------
    // Usage/Status Display
    // -------------------------------------------------------------------------

    /// Render token usage statistics.
    pub fn render_usage(&self, input_tokens: u32, output_tokens: u32, cost_usd: Option<f64>) {
        if self.is_tty {
            let cost_str = cost_usd
                .map(|c| format!(" [dim](${c:.4})[/]"))
                .unwrap_or_default();
            self.print_markup(&format!(
                "[dim]Tokens: {input_tokens} in / {output_tokens} out{cost_str}[/]\n"
            ));
        }
    }

    /// Render session info.
    pub fn render_session_info(&self, session_path: &str, message_count: usize) {
        if self.is_tty {
            self.print_markup(&format!(
                "[dim]Session: {session_path} ({message_count} messages)[/]\n"
            ));
        }
    }

    /// Render model info.
    pub fn render_model_info(&self, model: &str, thinking_level: Option<&str>) {
        if self.is_tty {
            let thinking_str = thinking_level
                .map(|t| format!(" [dim](thinking: {t})[/]"))
                .unwrap_or_default();
            self.print_markup(&format!("[dim]Model: {model}{thinking_str}[/]\n"));
        }
    }

    // -------------------------------------------------------------------------
    // Interactive Mode Helpers
    // -------------------------------------------------------------------------

    /// Render the input prompt.
    pub fn render_prompt(&self) {
        if self.is_tty {
            self.print_markup("[bold cyan]>[/] ");
        } else {
            print!("> ");
        }
        let _ = io::stdout().flush();
    }

    /// Render a user message echo.
    pub fn render_user_message(&self, message: &str) {
        if self.is_tty {
            self.print_markup(&format!("[bold]You:[/] {message}\n\n"));
        } else {
            println!("You: {message}\n");
        }
    }

    /// Render assistant message start.
    pub fn render_assistant_start(&self) {
        if self.is_tty {
            self.print_markup("[bold]Assistant:[/] ");
        } else {
            print!("Assistant: ");
        }
        let _ = io::stdout().flush();
    }

    /// Clear the current line (for progress updates).
    pub fn clear_line(&self) {
        if self.is_tty {
            print!("\r\x1b[K");
            let _ = io::stdout().flush();
        }
    }

    /// Move cursor up N lines.
    pub fn cursor_up(&self, n: usize) {
        if self.is_tty {
            print!("\x1b[{n}A");
            let _ = io::stdout().flush();
        }
    }
}

impl Default for PiConsole {
    fn default() -> Self {
        Self::new()
    }
}

// Thread-safe console for use across async tasks
impl Clone for PiConsole {
    fn clone(&self) -> Self {
        Self {
            console: Console::builder()
                .markup(self.is_tty)
                .emoji(self.is_tty)
                .build(),
            is_tty: self.is_tty,
        }
    }
}

#[derive(Debug, Clone)]
enum MarkdownChunk {
    Text(String),
    CodeBlock {
        language: Option<String>,
        code: String,
    },
}

fn parse_fenced_code_language(info: &str) -> Option<String> {
    let language_tag = info
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .split(',')
        .next()
        .unwrap_or_default()
        .trim();
    if language_tag.is_empty() {
        None
    } else {
        Some(language_tag.to_ascii_lowercase())
    }
}

fn split_markdown_fenced_code_blocks(markdown: &str) -> Vec<MarkdownChunk> {
    let mut chunks = Vec::new();

    let mut text_buf = String::new();
    let mut code_buf = String::new();
    let mut in_code_block = false;
    let mut fence_len = 0usize;
    let mut code_language: Option<String> = None;

    for line in markdown.split_inclusive('\n') {
        let trimmed_start = line.trim_start();
        let trimmed_line = trimmed_start.trim_end_matches(['\r', '\n']);

        let backtick_count = trimmed_line.chars().take_while(|ch| *ch == '`').count();
        let is_fence = backtick_count >= 3 && trimmed_line.starts_with("```");

        if !in_code_block {
            if is_fence {
                if !text_buf.is_empty() {
                    chunks.push(MarkdownChunk::Text(std::mem::take(&mut text_buf)));
                }

                fence_len = backtick_count;
                let info = trimmed_line.get(fence_len..).unwrap_or_default();
                code_language = parse_fenced_code_language(info);
                in_code_block = true;
                code_buf.clear();
                continue;
            }

            text_buf.push_str(line);
            continue;
        }

        if is_fence
            && backtick_count >= fence_len
            && trimmed_line[backtick_count..].trim().is_empty()
        {
            chunks.push(MarkdownChunk::CodeBlock {
                language: code_language.take(),
                code: std::mem::take(&mut code_buf),
            });
            in_code_block = false;
            fence_len = 0;
            continue;
        }

        code_buf.push_str(line);
    }

    if in_code_block {
        // Unterminated fence; fall back to treating the entire input as plain markdown.
        return vec![MarkdownChunk::Text(markdown.to_string())];
    }

    if !text_buf.is_empty() {
        chunks.push(MarkdownChunk::Text(text_buf));
    }

    chunks
}

fn has_multiple_non_none_styles(segments: &[Segment<'_>]) -> bool {
    use std::collections::HashSet;

    let mut seen = HashSet::new();
    for segment in segments {
        let Some(style) = &segment.style else {
            continue;
        };
        if segment.text.as_ref().trim().is_empty() {
            continue;
        }

        seen.insert(style.clone());
        if seen.len() > 1 {
            return true;
        }
    }

    false
}

fn render_syntax_line_by_line(
    code: &str,
    language: &str,
    width: usize,
) -> Option<Vec<Segment<'static>>> {
    let mut rendered: Vec<Segment<'static>> = Vec::new();
    for line in code.split_inclusive('\n') {
        let syntax = Syntax::new(line, language);
        let items = syntax.render(Some(width)).ok()?;
        rendered.extend(items.into_iter().map(Segment::into_owned));
    }
    Some(rendered)
}

fn render_markdown_with_syntax(markdown: &str, width: usize) -> Vec<Segment<'static>> {
    if !markdown.contains("```") {
        return Markdown::new(markdown)
            .render(width)
            .into_iter()
            .map(Segment::into_owned)
            .collect();
    }

    let chunks = split_markdown_fenced_code_blocks(markdown);
    let mut segments: Vec<Segment<'static>> = Vec::new();

    for chunk in chunks {
        match chunk {
            MarkdownChunk::Text(text) => {
                if text.is_empty() {
                    continue;
                }
                segments.extend(
                    Markdown::new(text)
                        .render(width)
                        .into_iter()
                        .map(Segment::into_owned),
                );
            }
            MarkdownChunk::CodeBlock { language, mut code } => {
                if !code.ends_with('\n') {
                    code.push('\n');
                }

                let language = language.unwrap_or_else(|| "text".to_string());
                let require_variation = matches!(language.as_str(), "typescript" | "ts" | "tsx");
                let mut candidates: Vec<&str> = Vec::new();
                match language.as_str() {
                    // syntect's built-in set doesn't always include TypeScript; prefer `ts` if
                    // available, otherwise fall back to JavaScript highlighting.
                    "typescript" | "ts" | "tsx" => candidates.extend(["ts", "javascript"]),
                    _ => candidates.push(language.as_str()),
                }
                candidates.push("text");

                let mut rendered_items: Option<Vec<Segment<'static>>> = None;
                for candidate in candidates {
                    let syntax = Syntax::new(code.as_str(), candidate);
                    if let Ok(items) = syntax.render(Some(width)) {
                        if require_variation
                            && candidate != "text"
                            && !has_multiple_non_none_styles(&items)
                        {
                            if candidate == "javascript" {
                                if let Some(line_items) =
                                    render_syntax_line_by_line(code.as_str(), candidate, width)
                                {
                                    if has_multiple_non_none_styles(&line_items) {
                                        rendered_items = Some(line_items);
                                        break;
                                    }
                                }
                            }
                            continue;
                        }
                        rendered_items = Some(items.into_iter().map(Segment::into_owned).collect());
                        break;
                    }
                }

                if let Some(items) = rendered_items {
                    segments.extend(items);
                } else {
                    segments.extend(
                        Markdown::new(format!("```\n{code}```\n"))
                            .render(width)
                            .into_iter()
                            .map(Segment::into_owned),
                    );
                }
            }
        }
    }

    segments
}

/// Strip rich markup tags from text.
fn strip_markup(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars();

    while let Some(c) = chars.next() {
        if c == '[' {
            // Potential tag
            let mut buffer = String::new();
            let mut is_tag = true;

            for next_c in chars.by_ref() {
                if next_c == ']' {
                    break;
                }
                buffer.push(next_c);
                // Heuristic: rich_rust tags usually contain alpha, space, slash, comma.
                // If we see digits or other symbols, assume it's not a tag (e.g. array[0]).
                if !next_c.is_ascii_alphabetic() && !matches!(next_c, ' ' | '/' | ',') {
                    is_tag = false;
                }
            }

            if is_tag && !buffer.is_empty() {
                // It was a tag, discard buffer (already consumed)
            } else {
                // Not a tag, append literal
                result.push('[');
                result.push_str(&buffer);
                result.push(']');
            }
        } else {
            result.push(c);
        }
    }

    result
}

/// Spinner styles for different operations.
pub enum SpinnerStyle {
    /// Default dots spinner for general operations.
    Dots,
    /// Line spinner for file operations.
    Line,
    /// Simple ASCII spinner for compatibility.
    Simple,
}

impl SpinnerStyle {
    /// Get the spinner frames for this style.
    pub const fn frames(&self) -> &'static [&'static str] {
        match self {
            Self::Dots => &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"],
            Self::Line => &["⎺", "⎻", "⎼", "⎽", "⎼", "⎻"],
            Self::Simple => &["|", "/", "-", "\\"],
        }
    }

    /// Get the frame interval in milliseconds.
    pub const fn interval_ms(&self) -> u64 {
        match self {
            Self::Dots => 80,
            Self::Line | Self::Simple => 100,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    fn capture_markdown_segments(markdown: &str) -> Vec<Segment<'static>> {
        let mut console = PiConsole::with_color();
        console.console.begin_capture();
        console.render_markdown(markdown);
        console.console.end_capture()
    }

    fn segments_text(segments: &[Segment<'static>]) -> String {
        segments.iter().map(|s| s.text.as_ref()).collect()
    }

    fn unique_style_debug_for_tokens(
        segments: &[Segment<'static>],
        tokens: &[&str],
    ) -> HashSet<String> {
        segments
            .iter()
            .filter(|segment| {
                let text = segment.text.as_ref();
                tokens.iter().any(|token| text.contains(token))
            })
            .map(|segment| format!("{:?}", segment.style))
            .collect()
    }

    #[derive(Clone)]
    struct SharedBufferWriter {
        buffer: Arc<Mutex<Vec<u8>>>,
    }

    impl io::Write for SharedBufferWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.buffer
                .lock()
                .expect("lock buffer")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn test_strip_markup() {
        assert_eq!(strip_markup("[bold]Hello[/]"), "Hello");
        assert_eq!(strip_markup("[red]A[/] [blue]B[/]"), "A B");
        assert_eq!(strip_markup("No markup"), "No markup");
        assert_eq!(strip_markup("[bold red on blue]Text[/]"), "Text");
        assert_eq!(strip_markup("array[0]"), "array[0]");
    }

    #[test]
    fn render_markdown_emits_ansi_when_tty() {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let writer = SharedBufferWriter {
            buffer: Arc::clone(&buffer),
        };
        let console = Console::builder()
            .markup(true)
            .emoji(false)
            .force_terminal(true)
            .color_system(ColorSystem::TrueColor)
            .file(Box::new(writer))
            .build();

        let pi_console = PiConsole {
            console,
            is_tty: true,
        };

        pi_console.render_markdown("# Title\n\n- Item 1\n- Item 2\n\n**bold**");

        let output = String::from_utf8(buffer.lock().expect("lock buffer").clone()).expect("utf-8");

        assert!(
            output.contains("\u{1b}["),
            "expected ANSI escape codes, got: {output:?}"
        );
        assert!(!output.contains("**bold**"));
        assert!(output.contains("bold"));
    }

    #[test]
    fn test_spinner_frames() {
        let dots = SpinnerStyle::Dots;
        assert_eq!(dots.frames().len(), 10);
        assert_eq!(dots.interval_ms(), 80);

        let simple = SpinnerStyle::Simple;
        assert_eq!(simple.frames().len(), 4);
    }

    #[test]
    fn test_console_creation() {
        let console = PiConsole::with_color();
        assert!(console.width() > 0);
    }

    #[test]
    fn render_markdown_produces_styled_segments() {
        let mut console = PiConsole::with_color();

        console.console.begin_capture();
        console.render_markdown("# Title\n\nThis is **bold**.\n\n- Item 1\n- Item 2");
        let segments = console.console.end_capture();

        let captured: String = segments.iter().map(|s| s.text.as_ref()).collect();
        assert!(captured.contains("Title"));
        assert!(captured.contains("bold"));
        assert!(segments.iter().any(|s| s.style.is_some()));
    }

    #[test]
    fn render_markdown_code_fence_uses_syntax_highlighting_when_language_present() {
        let mut console = PiConsole::with_color();

        console.console.begin_capture();
        console.render_markdown("```rust\nfn main() {\n    println!(\"hi\");\n}\n```");
        let segments = console.console.end_capture();

        let code_styles = unique_style_debug_for_tokens(&segments, &["fn", "println"]);

        assert!(
            code_styles.len() > 1,
            "expected multiple token styles from syntax highlighting, got {code_styles:?}"
        );
    }

    #[test]
    fn parse_fenced_code_language_extracts_first_tag() {
        assert_eq!(parse_fenced_code_language("rust"), Some("rust".to_string()));
        assert_eq!(
            parse_fenced_code_language(" RuSt "),
            Some("rust".to_string())
        );
        assert_eq!(
            parse_fenced_code_language("rust,ignore"),
            Some("rust".to_string())
        );
        assert_eq!(parse_fenced_code_language(""), None);
        assert_eq!(parse_fenced_code_language("   "), None);
    }

    #[test]
    fn split_markdown_fenced_code_blocks_splits_text_and_code() {
        let input = "Intro\n\n```rust\nfn main() {}\n```\n\nTail\n";
        let chunks = split_markdown_fenced_code_blocks(input);

        assert_eq!(chunks.len(), 3);
        assert!(matches!(chunks[0], MarkdownChunk::Text(_)));
        assert!(
            matches!(
                &chunks[1],
                MarkdownChunk::CodeBlock { language, code }
                    if language.as_deref() == Some("rust") && code.contains("fn main")
            ),
            "expected rust code block, got {chunks:?}"
        );
        assert!(matches!(chunks[2], MarkdownChunk::Text(_)));
    }

    #[test]
    fn split_markdown_fenced_code_blocks_unterminated_fence_falls_back_to_text() {
        let input = "Intro\n\n```rust\nfn main() {}\n";
        let chunks = split_markdown_fenced_code_blocks(input);

        assert_eq!(
            chunks.len(),
            1,
            "expected a single Text chunk, got {chunks:?}"
        );
        let MarkdownChunk::Text(text) = &chunks[0] else {
            unreachable!("expected text fallback, got {chunks:?}");
        };
        assert!(text.contains("```rust"));
        assert!(text.contains("fn main"));
    }

    #[test]
    fn render_markdown_strips_inline_markers_and_renders_headings_lists_links() {
        let segments = capture_markdown_segments(
            r"
# H1
## H2
### H3
#### H4
##### H5
###### H6

This is **bold**, *italic*, ~~strike~~, `code`, and [link](https://example.com).

- Bullet 1
1. Numbered 1

Nested: **bold and *italic*** and ~~**strike bold**~~.
",
        );

        let captured = segments_text(&segments);
        for needle in [
            "H1",
            "H2",
            "H3",
            "H4",
            "H5",
            "H6",
            "bold",
            "italic",
            "strike",
            "code",
            "link",
            "Bullet 1",
            "Numbered 1",
            "Nested",
        ] {
            assert!(
                captured.contains(needle),
                "expected output to contain {needle:?}, got: {captured:?}"
            );
        }

        assert!(
            !captured.contains("**"),
            "expected bold markers to be stripped, got: {captured:?}"
        );
        assert!(
            !captured.contains("~~"),
            "expected strikethrough markers to be stripped, got: {captured:?}"
        );
        assert!(
            !captured.contains('`'),
            "expected inline code markers to be stripped, got: {captured:?}"
        );
        assert!(
            !captured.contains("]("),
            "expected link markers to be stripped, got: {captured:?}"
        );

        assert!(
            segments.iter().any(|s| s.style.is_some()),
            "expected styled segments, got: {segments:?}"
        );
    }

    #[test]
    fn render_markdown_code_fences_highlight_multiple_languages_and_fallback_unknown() {
        let segments = capture_markdown_segments(
            r#"
```rust
fn main() { println!("hi"); }
```

```python
def foo():
    print("hi")
```

```javascript
function foo() { console.log("hi"); }
```

```typescript
interface Foo { x: number }
const foo: Foo = { x: 1 };
const greeting = "hi";
```

```notalanguage
some_code_here();
```
"#,
        );

        for (language, tokens) in [
            ("rust", vec!["fn", "println", "\"hi\""]),
            ("python", vec!["def", "print", "\"hi\""]),
            ("javascript", vec!["function", "console", "\"hi\""]),
            ("typescript", vec!["interface", "const", "\"hi\""]),
        ] {
            let styles = unique_style_debug_for_tokens(&segments, &tokens);
            assert!(
                styles.len() > 1,
                "expected multiple styles for {language} tokens {tokens:?}, got {styles:?}"
            );
        }

        let captured = segments_text(&segments);
        assert!(
            captured.contains("some_code_here"),
            "expected unknown language fence to still render code, got: {captured:?}"
        );
    }
}
