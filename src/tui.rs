//! Terminal UI components using rich_rust.
//!
//! This module provides the interactive terminal interface for Pi,
//! built on rich_rust for beautiful markup-based output.

use std::io::{self, IsTerminal, Write};

use rich_rust::Theme;
use rich_rust::prelude::*;

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

    /// Create a new Pi console with an optional rich_rust theme.
    pub fn new_with_theme(theme: Option<Theme>) -> Self {
        let is_tty = io::stdout().is_terminal();
        let mut builder = Console::builder().markup(is_tty).emoji(is_tty);
        if let Some(theme) = theme {
            builder = builder.theme(theme);
        }
        let console = builder.build();

        Self { console, is_tty }
    }

    /// Create a console with forced color output (for testing).
    pub fn with_color() -> Self {
        Self {
            console: Console::builder().markup(true).emoji(true).build(),
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

/// Strip rich markup tags from text.
fn strip_markup(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut in_tag = false;

    for c in text.chars() {
        match c {
            '[' => in_tag = true,
            ']' if in_tag => in_tag = false,
            _ if !in_tag => result.push(c),
            _ => {}
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

    #[test]
    fn test_strip_markup() {
        assert_eq!(strip_markup("[bold]Hello[/]"), "Hello");
        assert_eq!(strip_markup("[red]A[/] [blue]B[/]"), "A B");
        assert_eq!(strip_markup("No markup"), "No markup");
        assert_eq!(strip_markup("[bold red on blue]Text[/]"), "Text");
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
}
