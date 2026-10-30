//! Pi's content-sized FrankenTUI input editor.
//!
//! [`TextArea`] wraps [`Editor`] for text manipulation and
//! provides Frame-based rendering with viewport scrolling and cursor display.
//!
//! Adapted from `ftui-widgets` 0.9.0 `src/textarea.rs` and the private drawing
//! helpers in its `src/lib.rs`, upstream revision
//! `befbd2471a1ec4a6b8ce0a0df32116c317cca9db`. Copyright (c) 2026 Jeffrey
//! Emanuel; the unchanged upstream license is in `textarea.LICENSE` alongside
//! this file. Published archive SHA-256:
//! `7ba05f7d99b86ad2a58752f4ad7393baf3785b488d8df51952dd21b64f57d706`.
//!
//! The buffer, selection, undo, and word/grapheme segmentation
//! remain FrankenTUI's implementation. Pi adds visual row measurement,
//! reconciles the viewport after growth, deletion, or reflow, and gives a
//! full-width insertion caret a continuation row. Cursor movement and clicks
//! share those wrapped-row boundaries (issue #276).
//! Mouse gestures are cancelled on draft replacement, focus or input
//! ownership loss, and rendered geometry changes, so stale presses cannot
//! select another draft.
//! Keeping this component in the source package also delivers the fix to
//! `cargo install`; Cargo discards registry patches when publishing a crate.
//!
//! Logical lines, including a trailing empty line, remain in the editor
//! independently of the width used to render them.

use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ftui::{a11y as ftui_a11y, core as ftui_core, render as ftui_render};
use ftui::{style as ftui_style, text as ftui_text};
use ftui_core::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, Modifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ftui_core::geometry::Rect;
use ftui_core::gesture::GestureConfig;
use ftui_render::frame::Frame;
use ftui_style::Style;
use ftui_text::editor::{Editor, Selection};
use ftui_text::wrap::display_width;
use ftui_text::{CursorNavigator, CursorPosition, graphemes, word_segments};

use ftui::widgets::{StatefulWidget, Widget};

// FrankenTUI's textarea uses these private widget helpers. Keep their cell
// styling, grapheme pooling, and accessibility identity semantics here too.
fn apply_style(cell: &mut ftui::Cell, style: Style) {
    if let Some(fg) = style.fg {
        cell.fg = fg;
    }
    if let Some(bg) = style.bg {
        match bg.a() {
            0 => {}
            255 => cell.bg = bg,
            _ => cell.bg = bg.over(cell.bg),
        }
    }
    if let Some(attrs) = style.attrs {
        let cell_flags: ftui_render::cell::StyleFlags = attrs.into();
        cell.attrs = cell.attrs.merged_flags(cell_flags);
    }
}

fn clear_text_area(frame: &mut Frame, area: Rect, style: Style) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let mut cell = ftui::Cell::from_char(' ');
    apply_style(&mut cell, style);
    frame.buffer.fill(area, cell);
}

fn draw_text_span(
    frame: &mut Frame,
    mut x: u16,
    y: u16,
    content: &str,
    style: Style,
    max_x: u16,
) -> u16 {
    for grapheme in graphemes(content) {
        if x >= max_x {
            break;
        }
        let width = ftui_text::grapheme_width(grapheme);
        if width == 0 {
            continue;
        }
        let width_u16 = u16::try_from(width).unwrap_or(u16::MAX);
        if width_u16 > max_x.saturating_sub(x) {
            break;
        }
        let content = if width > 1 || grapheme.chars().count() > 1 {
            let id = frame.intern_with_width(grapheme, u8::try_from(width).unwrap_or(u8::MAX));
            ftui_render::cell::CellContent::from_grapheme(id)
        } else if let Some(ch) = grapheme.chars().next() {
            ftui_render::cell::CellContent::from_char(ch)
        } else {
            continue;
        };
        let mut cell = frame.buffer.get(x, y).copied().unwrap_or_default();
        cell.content = content;
        cell.attrs = ftui_render::cell::CellAttrs::new(cell.attrs.flags(), 0);
        apply_style(&mut cell, style);
        frame.buffer.set_fast(x, y, cell);
        x = x.saturating_add(width_u16);
    }
    x
}

fn a11y_node_id(area: Rect) -> u64 {
    const FNV_OFFSET: u64 = 14_695_981_039_346_656_037;
    const FNV_PRIME: u64 = 1_099_511_628_211;
    let mut hash = FNV_OFFSET;
    for byte in area
        .x
        .to_le_bytes()
        .iter()
        .chain(&area.y.to_le_bytes())
        .chain(&area.width.to_le_bytes())
        .chain(&area.height.to_le_bytes())
    {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Multi-line text editor widget.
/// A per-line syntax style provider for [`TextArea::with_highlighter`].
///
/// Called once per visible logical line per render with `(line_index,
/// line_text)`, where `line_text` has no trailing newline. It returns byte
/// ranges into `line_text` paired with a [`Style`] to merge over the base
/// style (before the selection style, so selection still wins). Ranges may
/// overlap — a later range wins for a byte it covers. Ranges that fall outside
/// the line or that do not sit on grapheme boundaries are ignored; an empty
/// vector means "no highlight". The hook lives entirely in `ftui-widgets` and
/// knows nothing about any tokenizer.
#[allow(clippy::type_complexity)] // Upstream callback contract, kept intact.
pub type LineHighlighter = Arc<dyn Fn(usize, &str) -> Vec<(Range<usize>, Style)> + Send + Sync>;

#[derive(Clone)]
// Independent upstream display/input options, not one state machine.
#[allow(clippy::struct_excessive_bools)]
pub struct TextArea {
    editor: Editor,
    /// Full-buffer replacements, distinct from edits within the same draft.
    replacement_generation: u64,
    /// Placeholder text shown when empty.
    placeholder: String,
    /// Whether the widget has input focus.
    focused: bool,
    /// Show line numbers in gutter.
    show_line_numbers: bool,
    /// Base style.
    style: Style,
    /// Cursor line highlight style.
    cursor_line_style: Option<Style>,
    /// Selection highlight style.
    selection_style: Style,
    /// Placeholder style.
    placeholder_style: Style,
    /// Line number style.
    line_number_style: Style,
    /// Soft-wrap long lines.
    ///
    /// Visual wrapping preserves whitespace and breaks on word boundaries with
    /// a grapheme fallback for long segments.
    soft_wrap: bool,
    /// Maximum height in lines (0 = unlimited / fill area).
    max_height: usize,
    /// Viewport scroll anchor (logical_line_idx, visual_wrap_offset).
    ///
    /// In soft-wrap mode, this tracks the first visible visual line.
    /// In no-wrap mode, the second element is ignored (always 0).
    scroll_anchor: std::cell::Cell<(usize, usize)>,
    /// Horizontal scroll offset (visual columns).
    scroll_left: std::cell::Cell<usize>,
    /// Last viewport height for page movement and visibility checks.
    #[allow(dead_code)]
    last_viewport_height: std::cell::Cell<usize>,
    /// Last viewport width for visibility checks.
    last_viewport_width: std::cell::Cell<usize>,
    /// Optional per-line syntax style provider (see [`LineHighlighter`]).
    highlighter: Option<LineHighlighter>,
    /// Origin for event timestamps; explicit replay timestamps bypass this clock.
    undo_clock: Instant,
    /// Area of the last render, for mapping mouse positions to text.
    last_area: std::cell::Cell<Rect>,
    /// Whether the last rendered area still belongs to this input flow.
    mouse_area_valid: std::cell::Cell<bool>,
    /// The previous left press, for double and triple clicks.
    last_click: std::cell::Cell<Option<LastClick>>,
    /// A left press inside the area started a drag selection.
    mouse_selecting: std::cell::Cell<bool>,
}

/// A left press, remembered to count multi-clicks.
#[derive(Debug, Clone, Copy)]
struct LastClick {
    at_ms: u64,
    x: u16,
    y: u16,
    /// 1, 2 or 3: single, double or triple.
    count: u8,
}

impl std::fmt::Debug for TextArea {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextArea")
            .field("editor", &self.editor)
            .field("replacement_generation", &self.replacement_generation)
            .field("placeholder", &self.placeholder)
            .field("focused", &self.focused)
            .field("show_line_numbers", &self.show_line_numbers)
            .field("style", &self.style)
            .field("cursor_line_style", &self.cursor_line_style)
            .field("selection_style", &self.selection_style)
            .field("placeholder_style", &self.placeholder_style)
            .field("line_number_style", &self.line_number_style)
            .field("soft_wrap", &self.soft_wrap)
            .field("max_height", &self.max_height)
            .field("scroll_anchor", &self.scroll_anchor)
            .field("scroll_left", &self.scroll_left)
            .field("last_viewport_height", &self.last_viewport_height)
            .field("last_viewport_width", &self.last_viewport_width)
            .field("highlighter", &self.highlighter.as_ref().map(|_| "<fn>"))
            .field("undo_clock", &self.undo_clock)
            .field("last_area", &self.last_area)
            .field("mouse_area_valid", &self.mouse_area_valid)
            .field("last_click", &self.last_click)
            .field("mouse_selecting", &self.mouse_selecting)
            .finish()
    }
}

/// The style of the last range in `spans` that covers `byte`, or `None`.
///
/// Ranges are byte ranges into a single line; a later range wins on overlap.
/// Never panics: offsets outside every range simply return `None`.
fn style_at(spans: &[(Range<usize>, Style)], byte: usize) -> Option<Style> {
    spans
        .iter()
        .rev()
        .find(|(range, _)| range.contains(&byte))
        .map(|(_, style)| *style)
}

impl Default for TextArea {
    fn default() -> Self {
        Self::new()
    }
}

/// Render state tracked across frames.
#[derive(Debug, Clone, Default)]
pub struct TextAreaState {
    /// Viewport height from last render.
    pub last_viewport_height: u16,
    /// Viewport width from last render.
    pub last_viewport_width: u16,
}

#[derive(Debug, Clone)]
struct WrappedSlice<'a> {
    text: &'a str,
    start_byte: usize,
    start_col: usize,
    width: usize,
}

impl WrappedSlice<'_> {
    /// A wrapped boundary belongs to the next slice. A click or vertical
    /// movement targeting this row must remain before that boundary.
    fn clamp_cursor_column(&self, column: usize, continues: bool) -> usize {
        column.min(if continues {
            self.width.saturating_sub(1)
        } else {
            self.width
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct WrapMetrics {
    rows: usize,
    last_width: usize,
    total_width: usize,
}

impl TextArea {
    /// Create a new empty text area.
    #[must_use]
    pub fn new() -> Self {
        Self {
            editor: Editor::new(),
            replacement_generation: 0,
            placeholder: String::new(),
            focused: false,
            show_line_numbers: false,
            style: Style::default(),
            cursor_line_style: None,
            selection_style: Style::new().reverse(),
            placeholder_style: Style::new().dim(),
            line_number_style: Style::new().dim(),
            soft_wrap: false,
            max_height: 0,
            scroll_anchor: std::cell::Cell::new((usize::MAX, 0)), // sentinel
            scroll_left: std::cell::Cell::new(0),
            last_viewport_height: std::cell::Cell::new(0),
            last_viewport_width: std::cell::Cell::new(0),
            highlighter: None,
            undo_clock: Instant::now(),
            last_area: std::cell::Cell::new(Rect::default()),
            mouse_area_valid: std::cell::Cell::new(false),
            last_click: std::cell::Cell::new(None),
            mouse_selecting: std::cell::Cell::new(false),
        }
    }

    // ── Event Handling ─────────────────────────────────────────────

    /// Handle a terminal event.
    ///
    /// Returns `true` if the state changed.
    pub fn handle_event(&mut self, event: &Event) -> bool {
        let now_ms = u64::try_from(self.undo_clock.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.handle_event_at(event, now_ms)
    }

    /// Handle an event with a caller-supplied monotonic millisecond timestamp.
    /// Use this instead of [`Self::handle_event`] for deterministic replay.
    /// Switching to an earlier clock value starts a new undo group.
    pub fn handle_event_at(&mut self, event: &Event, now_ms: u64) -> bool {
        self.editor.tick(now_ms);
        match event {
            Event::Key(key)
                if key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat =>
            {
                self.handle_key(key)
            }
            Event::Paste(paste) => {
                self.insert_text(&paste.text);
                true
            }
            Event::Focus(false) => {
                self.reset_mouse_interaction();
                self.editor.break_undo_group();
                false
            }
            Event::Mouse(mouse) => self.handle_mouse(mouse, now_ms),
            _ => false,
        }
    }

    /// Left press places the cursor, a second press selects the word under
    /// it (Unicode word boundaries) and a third the line. Presses count as
    /// one multi-click within [`GestureConfig`]'s window and tolerance (300
    /// ms, 1 cell), so timing matches the runtime's gesture recognizer.
    /// Dragging after a single press extends the selection. Presses outside
    /// the area last rendered are ignored, and so is a drag that started
    /// outside it, so a host can forward every mouse event.
    fn handle_mouse(&mut self, mouse: &MouseEvent, now_ms: u64) -> bool {
        if !self.mouse_area_valid.get() {
            return false;
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let Some(pos) = self.position_at(mouse.x, mouse.y) else {
                    self.mouse_selecting.set(false);
                    self.last_click.set(None);
                    return false;
                };
                let config = GestureConfig::default();
                let window =
                    u64::try_from(config.multi_click_timeout.as_millis()).unwrap_or(u64::MAX);
                let count = match self.last_click.get() {
                    Some(last)
                        if now_ms >= last.at_ms
                            && now_ms - last.at_ms <= window
                            && last.x.abs_diff(mouse.x) <= config.click_tolerance
                            && last.y.abs_diff(mouse.y) <= config.click_tolerance =>
                    {
                        last.count % 3 + 1
                    }
                    _ => 1,
                };
                self.last_click.set(Some(LastClick {
                    at_ms: now_ms,
                    x: mouse.x,
                    y: mouse.y,
                    count,
                }));
                self.mouse_selecting.set(count == 1);
                match count {
                    1 => self.editor.set_cursor(pos),
                    2 => self.select_word_at(pos),
                    _ => self.select_line_at(pos),
                }
                true
            }
            MouseEventKind::Drag(MouseButton::Left) if self.mouse_selecting.get() => {
                let area = self.last_area.get();
                if area.is_empty() {
                    return false;
                }
                let x = mouse.x.clamp(area.x, area.right() - 1);
                let y = mouse.y.clamp(area.y, area.bottom() - 1);
                let pos = self.position_in_area(area, x, y);
                self.editor.extend_selection_to(pos);
                true
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.mouse_selecting.set(false);
                false
            }
            _ => false,
        }
    }

    /// The text position under a cell of the last rendered area, or `None`
    /// outside it (or before the first render).
    fn position_at(&self, x: u16, y: u16) -> Option<CursorPosition> {
        let area = self.last_area.get();
        (!area.is_empty() && area.contains(x, y)).then(|| self.position_in_area(area, x, y))
    }

    /// Invert the render's layout: rows from the scroll anchor (wrapped
    /// slices when soft-wrapping), columns past the gutter plus the
    /// horizontal scroll. Below the text maps to the last line; the gutter
    /// maps to column 0.
    fn position_in_area(&self, area: Rect, x: u16, y: u16) -> CursorPosition {
        let col = usize::from(x.saturating_sub(area.x.saturating_add(self.gutter_width())));
        let row = usize::from(y.saturating_sub(area.y));
        let rope = self.editor.rope();
        let nav = CursorNavigator::new(rope);
        let (top_line, top_vrow) = self.scroll_anchor.get();
        let top_line = if top_line == usize::MAX { 0 } else { top_line };
        let line_count = self.editor.line_count();

        if !self.soft_wrap {
            return nav.from_visual_col(top_line + row, self.scroll_left.get() + col);
        }

        let width = self.last_viewport_width.get();
        let mut remaining = row;
        for line_idx in top_line..line_count {
            let line_text = rope
                .line(line_idx)
                .unwrap_or(std::borrow::Cow::Borrowed(""));
            let line_text = line_text.trim_end_matches(['\n', '\r']);
            let slices = self.line_wrap_slices(line_idx, line_text, width);
            let skip = if line_idx == top_line { top_vrow } else { 0 };
            let visible = slices.len().saturating_sub(skip);
            if remaining < visible {
                let index = skip + remaining;
                let slice = &slices[index];
                // Past the end of a slice that wraps, stay on that row
                // rather than landing at the start of the next one.
                let column = slice.clamp_cursor_column(col, index + 1 < slices.len());
                return nav.from_visual_col(line_idx, slice.start_col + column);
            }
            remaining -= visible;
        }
        nav.from_visual_col(line_count.saturating_sub(1), usize::MAX)
    }

    /// Select the Unicode word segment (UAX #29) under `pos`: a word, a run
    /// of spaces, or one punctuation mark.
    fn select_word_at(&mut self, pos: CursorPosition) {
        let rope = self.editor.rope();
        let nav = CursorNavigator::new(rope);
        let line_text = rope
            .line(pos.line)
            .unwrap_or(std::borrow::Cow::Borrowed(""));
        let line_text = line_text.trim_end_matches(['\n', '\r']);
        let line_start = nav.to_byte_index(nav.from_line_grapheme(pos.line, 0));
        let offset = nav.to_byte_index(pos) - line_start;
        let mut segment = None;
        let mut start = 0;
        for word in word_segments(line_text) {
            segment = Some((start, word));
            start += word.len();
            if offset < start {
                break;
            }
        }
        let Some((start, word)) = segment else {
            self.editor.set_cursor(pos);
            return;
        };
        let from = nav.from_byte_index(line_start + start);
        let to = nav.from_byte_index(line_start + start + word.len());
        self.editor.set_cursor(from);
        self.editor.extend_selection_to(to);
    }

    /// Select the whole logical line under `pos`, without its newline.
    fn select_line_at(&mut self, pos: CursorPosition) {
        let nav = CursorNavigator::new(self.editor.rope());
        let from = nav.from_line_grapheme(pos.line, 0);
        let to = nav.from_line_grapheme(pos.line, usize::MAX);
        self.editor.set_cursor(from);
        self.editor.extend_selection_to(to);
    }

    // Keep the upstream key dispatch together so its modifier precedence is
    // easy to compare with the published widget.
    #[allow(clippy::too_many_lines)]
    fn handle_key(&mut self, key: &KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(Modifiers::CTRL);
        let shift = key.modifiers.contains(Modifiers::SHIFT);
        let _alt = key.modifiers.contains(Modifiers::ALT);
        // Command/Windows-key chords belong to the host, not the buffer. Only
        // CTRL used to be excluded, so on macOS every Cmd+<letter> shortcut
        // typed its letter: Cmd+V inserted a literal "v" next to the paste.
        let cmd = key.modifiers.contains(Modifiers::SUPER);

        match key.code {
            KeyCode::Char(c) if !ctrl && !cmd => {
                self.insert_char(c);
                true
            }
            KeyCode::Enter => {
                self.insert_newline();
                true
            }
            KeyCode::Backspace => {
                if ctrl {
                    self.delete_word_backward();
                } else {
                    self.delete_backward();
                }
                true
            }
            KeyCode::Delete => {
                if ctrl {
                    self.delete_word_forward();
                } else {
                    self.delete_forward();
                }
                true
            }
            KeyCode::Left => {
                if ctrl && shift {
                    self.select_word_left();
                } else if ctrl {
                    self.move_word_left();
                } else if shift {
                    self.select_left();
                } else {
                    self.move_left();
                }
                true
            }
            KeyCode::Right => {
                if ctrl && shift {
                    self.select_word_right();
                } else if ctrl {
                    self.move_word_right();
                } else if shift {
                    self.select_right();
                } else {
                    self.move_right();
                }
                true
            }
            KeyCode::Up => {
                if ctrl && shift {
                    self.select_paragraph_up();
                } else if ctrl {
                    self.move_paragraph_up();
                } else if shift {
                    self.select_up();
                } else {
                    self.move_up();
                }
                true
            }
            KeyCode::Down => {
                if ctrl && shift {
                    self.select_paragraph_down();
                } else if ctrl {
                    self.move_paragraph_down();
                } else if shift {
                    self.select_down();
                } else {
                    self.move_down();
                }
                true
            }
            KeyCode::Home => {
                self.move_to_line_start();
                true
            }
            KeyCode::End => {
                self.move_to_line_end();
                true
            }
            KeyCode::PageUp => {
                let page = self.last_viewport_height.get().max(1);
                if self.soft_wrap {
                    self.move_cursor_visual_up(page, shift);
                } else {
                    for _ in 0..page {
                        if shift {
                            self.editor.select_up();
                        } else {
                            self.editor.move_up();
                        }
                    }
                }
                self.ensure_cursor_visible();
                true
            }
            KeyCode::PageDown => {
                let page = self.last_viewport_height.get().max(1);
                if self.soft_wrap {
                    self.move_cursor_visual_down(page, shift);
                } else {
                    for _ in 0..page {
                        if shift {
                            self.editor.select_down();
                        } else {
                            self.editor.move_down();
                        }
                    }
                }
                self.ensure_cursor_visible();
                true
            }
            KeyCode::Char('a') if ctrl => {
                self.select_all();
                true
            }
            // Ctrl+K: Delete to end of line (common emacs/shell binding)
            KeyCode::Char('k') if ctrl => {
                self.delete_to_end_of_line();
                true
            }
            // Ctrl+Z: Undo
            KeyCode::Char('z') if ctrl => {
                self.undo();
                true
            }
            // Ctrl+Y: Redo
            KeyCode::Char('y') if ctrl => {
                self.redo();
                true
            }
            _ => false,
        }
    }

    // ── Builder methods ────────────────────────────────────────────

    /// Set initial text content (builder).
    #[must_use]
    pub fn with_text(mut self, text: &str) -> Self {
        self.reset_mouse_interaction();
        self.replacement_generation = self.replacement_generation.wrapping_add(1);
        self.editor = Editor::with_text(text);
        self.editor.move_to_document_start();
        self
    }

    /// Set placeholder text (builder).
    #[must_use]
    pub fn with_placeholder(mut self, text: impl Into<String>) -> Self {
        self.placeholder = text.into();
        self
    }

    /// Set focused state (builder).
    #[must_use]
    pub fn with_focus(mut self, focused: bool) -> Self {
        self.set_focused(focused);
        self
    }

    /// Enable line numbers (builder).
    #[must_use]
    pub const fn with_line_numbers(mut self, show: bool) -> Self {
        self.show_line_numbers = show;
        self
    }

    /// Install a per-line syntax style provider (builder). See
    /// [`LineHighlighter`].
    #[must_use]
    pub fn with_highlighter(mut self, highlighter: LineHighlighter) -> Self {
        self.highlighter = Some(highlighter);
        self
    }

    /// Set or clear the per-line syntax style provider.
    pub fn set_highlighter(&mut self, highlighter: Option<LineHighlighter>) {
        self.highlighter = highlighter;
    }

    /// Whether a syntax highlighter is installed.
    #[must_use]
    pub const fn has_highlighter(&self) -> bool {
        self.highlighter.is_some()
    }

    /// The highlight spans for one line, or empty when there is no highlighter
    /// or styling is disabled. Ranges that don't sit on grapheme boundaries of
    /// `line_text` are dropped so partial-grapheme styling can't occur.
    fn line_spans(
        &self,
        line_idx: usize,
        line_text: &str,
        styling: bool,
    ) -> Vec<(Range<usize>, Style)> {
        if !styling {
            return Vec::new();
        }
        let Some(highlighter) = self.highlighter.as_ref() else {
            return Vec::new();
        };
        highlighter(line_idx, line_text)
            .into_iter()
            .filter(|(range, _)| {
                range.start < range.end
                    && range.end <= line_text.len()
                    && line_text.is_char_boundary(range.start)
                    && line_text.is_char_boundary(range.end)
            })
            .collect()
    }

    /// Set base style (builder).
    #[must_use]
    pub const fn with_style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }

    /// Set cursor line highlight style (builder).
    #[must_use]
    pub const fn with_cursor_line_style(mut self, style: Style) -> Self {
        self.cursor_line_style = Some(style);
        self
    }

    /// Set selection style (builder).
    #[must_use]
    pub const fn with_selection_style(mut self, style: Style) -> Self {
        self.selection_style = style;
        self
    }

    /// Enable soft wrapping (builder).
    #[must_use]
    pub const fn with_soft_wrap(mut self, wrap: bool) -> Self {
        self.soft_wrap = wrap;
        self
    }

    /// Set maximum height in lines (builder). 0 = fill available area.
    #[must_use]
    pub const fn with_max_height(mut self, max: usize) -> Self {
        self.max_height = max;
        self
    }

    // ── State access ───────────────────────────────────────────────

    /// Get the full text content.
    #[must_use]
    pub fn text(&self) -> String {
        self.editor.text()
    }

    /// Set the full text content (resets cursor and undo history).
    pub fn set_text(&mut self, text: &str) {
        self.reset_mouse_interaction();
        self.replacement_generation = self.replacement_generation.wrapping_add(1);
        self.editor.set_text(text);
        self.scroll_anchor.set((0, 0));
        self.scroll_left.set(0);
    }

    /// Identify the current draft across delayed work. Full replacements
    /// advance this generation even when their bytes are unchanged; typing,
    /// deletion, undo, selection, and cursor movement retain it.
    #[must_use]
    pub const fn replacement_generation(&self) -> u64 {
        self.replacement_generation
    }

    /// Number of lines.
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.editor.line_count()
    }

    /// Number of visual rows at the given widget width, including the empty
    /// logical line after a trailing newline. Uses the same word/grapheme
    /// wrapping as rendering, and accounts for the line-number gutter.
    ///
    /// Hosts with a content-sized editor should use this instead of counting
    /// `str::lines()`, which drops a trailing empty row and ignores soft wraps.
    /// pi_agent_rust patch: keep growing-editor layout and rendering in sync.
    #[must_use]
    pub fn visual_line_count(&self, width: u16) -> usize {
        self.visual_line_count_up_to(width, usize::MAX)
    }

    /// Measure at most `max_rows` rows (at least one for an empty editor).
    /// A capped composer need not wrap every line of a large pasted document
    /// on each frame once its viewport is already full.
    #[must_use]
    pub fn visual_line_count_up_to(&self, width: u16, max_rows: usize) -> usize {
        let max_rows = max_rows.max(1);
        let text_width = usize::from(width.saturating_sub(self.gutter_width()));
        if !self.soft_wrap || text_width == 0 {
            return self.line_count().min(max_rows);
        }

        let rope = self.editor.rope();
        let mut rows = 0_usize;
        for line_idx in 0..self.line_count() {
            let line = rope
                .line(line_idx)
                .unwrap_or(std::borrow::Cow::Borrowed(""));
            rows = rows.saturating_add(self.line_wrap_count(
                line_idx,
                line.trim_end_matches(['\n', '\r']),
                text_width,
            ));
            if rows >= max_rows {
                return max_rows;
            }
        }
        rows
    }

    /// Width of the last nonempty render, when the editor has been displayed.
    /// Input ownership should follow the geometry the user actually sees,
    /// even if a resize event has not reached the host model yet.
    #[must_use]
    pub fn rendered_width(&self) -> Option<u16> {
        let width = self.last_area.get().width;
        (width > 0).then_some(width)
    }

    /// Cancel an in-flight drag and multi-click sequence when the host hides
    /// or replaces this editor. Mouse input waits for its next nonempty
    /// render; text, selection, undo, and the known layout width are retained.
    pub fn reset_mouse_interaction(&self) {
        self.mouse_selecting.set(false);
        self.last_click.set(None);
        self.mouse_area_valid.set(false);
    }

    /// Current cursor position.
    #[inline]
    #[must_use]
    pub fn cursor(&self) -> CursorPosition {
        self.editor.cursor()
    }

    /// Set cursor position (clamped to bounds). Clears selection.
    pub fn set_cursor_position(&mut self, pos: CursorPosition) {
        self.editor.set_cursor(pos);
        self.ensure_cursor_visible();
    }

    /// Whether the textarea is empty.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.editor.is_empty()
    }

    /// Current selection, if any.
    #[must_use = "use the returned selection (if any)"]
    pub fn selection(&self) -> Option<Selection> {
        self.editor.selection()
    }

    /// Get selected text.
    #[must_use = "use the returned selected text (if any)"]
    pub fn selected_text(&self) -> Option<String> {
        self.editor.selected_text()
    }

    /// Whether the widget has focus.
    #[must_use]
    pub const fn is_focused(&self) -> bool {
        self.focused
    }

    /// Set focus state.
    pub fn set_focused(&mut self, focused: bool) {
        if !focused {
            self.reset_mouse_interaction();
            self.editor.break_undo_group();
        }
        self.focused = focused;
    }

    /// Set the idle interval for typing/deletion undo groups (default 500 ms).
    /// Zero disables coalescing.
    pub fn set_undo_coalesce_idle(&mut self, idle: Duration) {
        self.editor.set_coalesce_idle(idle);
    }

    /// Number of available undo steps.
    #[must_use]
    pub fn undo_group_count(&self) -> usize {
        self.editor.undo_group_count()
    }

    /// Number of available redo steps.
    #[must_use]
    pub fn redo_group_count(&self) -> usize {
        self.editor.redo_group_count()
    }

    /// Number of primitive edits retained in the undo history.
    #[must_use]
    pub fn undo_op_count(&self) -> usize {
        self.editor.undo_op_count()
    }

    /// Access the underlying editor.
    #[must_use]
    pub const fn editor(&self) -> &Editor {
        &self.editor
    }

    /// Mutable access to the underlying editor.
    pub const fn editor_mut(&mut self) -> &mut Editor {
        &mut self.editor
    }

    // ── Editing operations (delegated to Editor) ───────────────────

    /// Insert text at cursor.
    pub fn insert_text(&mut self, text: &str) {
        self.editor.insert_text(text);
        self.ensure_cursor_visible();
    }

    /// Insert a single character.
    pub fn insert_char(&mut self, ch: char) {
        self.editor.insert_char(ch);
        self.ensure_cursor_visible();
    }

    /// Insert a newline.
    pub fn insert_newline(&mut self) {
        self.editor.insert_newline();
        self.ensure_cursor_visible();
    }

    /// Delete backward (backspace).
    pub fn delete_backward(&mut self) {
        self.editor.delete_backward();
        self.ensure_cursor_visible();
    }

    /// Delete forward (delete key).
    pub fn delete_forward(&mut self) {
        self.editor.delete_forward();
        self.ensure_cursor_visible();
    }

    /// Delete word backward (Ctrl+Backspace).
    pub fn delete_word_backward(&mut self) {
        self.editor.delete_word_backward();
        self.ensure_cursor_visible();
    }

    /// Delete word forward (Ctrl+Delete).
    pub fn delete_word_forward(&mut self) {
        self.editor.delete_word_forward();
        self.ensure_cursor_visible();
    }

    /// Delete to end of line (Ctrl+K).
    pub fn delete_to_end_of_line(&mut self) {
        self.editor.delete_to_end_of_line();
        self.ensure_cursor_visible();
    }

    /// Undo last edit.
    pub fn undo(&mut self) {
        self.editor.undo();
        self.ensure_cursor_visible();
    }

    /// Redo last undo.
    pub fn redo(&mut self) {
        self.editor.redo();
        self.ensure_cursor_visible();
    }

    // ── Navigation ─────────────────────────────────────────────────

    /// Move cursor left.
    pub fn move_left(&mut self) {
        self.editor.move_left();
        self.ensure_cursor_visible();
    }

    /// Move cursor right.
    pub fn move_right(&mut self) {
        self.editor.move_right();
        self.ensure_cursor_visible();
    }

    /// Move cursor up.
    pub fn move_up(&mut self) {
        if self.soft_wrap {
            self.move_cursor_visual_up(1, false);
        } else {
            self.editor.move_up();
        }
        self.ensure_cursor_visible();
    }

    /// Move cursor down.
    pub fn move_down(&mut self) {
        if self.soft_wrap {
            self.move_cursor_visual_down(1, false);
        } else {
            self.editor.move_down();
        }
        self.ensure_cursor_visible();
    }

    /// Move cursor left by word.
    pub fn move_word_left(&mut self) {
        self.editor.move_word_left();
        self.ensure_cursor_visible();
    }

    /// Move cursor right by word.
    pub fn move_word_right(&mut self) {
        self.editor.move_word_right();
        self.ensure_cursor_visible();
    }

    /// Extend selection left by word.
    pub fn select_word_left(&mut self) {
        self.editor.select_word_left();
        self.ensure_cursor_visible();
    }

    /// Extend selection right by word.
    pub fn select_word_right(&mut self) {
        self.editor.select_word_right();
        self.ensure_cursor_visible();
    }

    /// Move to the preceding blank-line paragraph boundary (Ctrl+Up).
    /// See [`CursorNavigator::move_paragraph_up`] for whitespace rules and examples.
    pub fn move_paragraph_up(&mut self) {
        self.editor.move_paragraph_up();
        self.ensure_cursor_visible();
    }

    /// Move to the following blank-line paragraph boundary (Ctrl+Down).
    /// See [`CursorNavigator::move_paragraph_down`] for whitespace rules and examples.
    pub fn move_paragraph_down(&mut self) {
        self.editor.move_paragraph_down();
        self.ensure_cursor_visible();
    }

    /// Extend selection to the preceding paragraph boundary (Ctrl+Shift+Up).
    /// See [`CursorNavigator::move_paragraph_up`] for whitespace rules and examples.
    pub fn select_paragraph_up(&mut self) {
        self.editor.select_paragraph_up();
        self.ensure_cursor_visible();
    }

    /// Extend selection to the following paragraph boundary (Ctrl+Shift+Down).
    /// See [`CursorNavigator::move_paragraph_down`] for whitespace rules and examples.
    pub fn select_paragraph_down(&mut self) {
        self.editor.select_paragraph_down();
        self.ensure_cursor_visible();
    }

    /// Move to start of line.
    pub fn move_to_line_start(&mut self) {
        self.editor.move_to_line_start();
        self.ensure_cursor_visible();
    }

    /// Move to end of line.
    pub fn move_to_line_end(&mut self) {
        self.editor.move_to_line_end();
        self.ensure_cursor_visible();
    }

    /// Move to start of document.
    pub fn move_to_document_start(&mut self) {
        self.editor.move_to_document_start();
        self.ensure_cursor_visible();
    }

    /// Move cursor to end of document.
    pub fn move_to_document_end(&mut self) {
        self.editor.move_to_document_end();
        self.ensure_cursor_visible();
    }

    fn move_cursor_visual_down(&mut self, count: usize, extend_selection: bool) {
        let width = self.last_viewport_width.get();
        if width == 0 {
            for _ in 0..count {
                if extend_selection {
                    self.editor.select_down();
                } else {
                    self.editor.move_down();
                }
            }
            return;
        }

        let rope = self.editor.rope();
        let mut cursor = self.editor.cursor();
        let mut remaining = count;

        let line_text = rope
            .line(cursor.line)
            .unwrap_or(std::borrow::Cow::Borrowed(""));
        let line_text = line_text.trim_end_matches(['\n', '\r']);
        let (mut current_v_row, _) =
            self.cursor_wrap_position(cursor.line, line_text, width, cursor.visual_col);

        // Calculate target screen X (relative to slice start) to preserve column
        let initial_slices = self.line_wrap_slices(cursor.line, line_text, width);
        let initial_slice_start = initial_slices.get(current_v_row).map_or(0, |s| s.start_col);
        let target_screen_x = cursor.visual_col.saturating_sub(initial_slice_start);

        while remaining > 0 {
            let line_text = rope
                .line(cursor.line)
                .unwrap_or(std::borrow::Cow::Borrowed(""));
            let line_text = line_text.trim_end_matches(['\n', '\r']);
            let wrap_count = self.line_wrap_count(cursor.line, line_text, width);

            let available_in_line = wrap_count.saturating_sub(1).saturating_sub(current_v_row);

            if remaining <= available_in_line {
                current_v_row += remaining;
                remaining = 0;
            } else {
                remaining -= available_in_line + 1;
                if cursor.line + 1 < self.editor.line_count() {
                    cursor.line += 1;
                    current_v_row = 0;
                } else {
                    current_v_row = wrap_count.saturating_sub(1);
                    remaining = 0;
                }
            }
        }

        let line_text = rope
            .line(cursor.line)
            .unwrap_or(std::borrow::Cow::Borrowed(""));
        let line_text = line_text.trim_end_matches(['\n', '\r']);
        let slices = self.line_wrap_slices(cursor.line, line_text, width);

        if let Some(slice) = slices.get(current_v_row) {
            let target_in_slice =
                slice.clamp_cursor_column(target_screen_x, current_v_row + 1 < slices.len());

            let mut g_idx = 0;
            let mut v_w = 0;
            for g in graphemes(slice.text) {
                let w = display_width(g);
                if v_w + w > target_in_slice {
                    break;
                }
                v_w += w;
                g_idx += 1;
            }

            let nav = CursorNavigator::new(rope);
            let line_start_byte = nav.to_byte_index(nav.from_line_grapheme(cursor.line, 0));
            let slice_byte_offset: usize = graphemes(slice.text).take(g_idx).map(str::len).sum();
            let final_byte = line_start_byte + slice.start_byte + slice_byte_offset;
            cursor = nav.from_byte_index(final_byte);
        }

        if extend_selection {
            self.editor.extend_selection_to(cursor);
        } else {
            self.editor.set_cursor(cursor);
        }
    }

    fn move_cursor_visual_up(&mut self, count: usize, extend_selection: bool) {
        let width = self.last_viewport_width.get();
        if width == 0 {
            for _ in 0..count {
                if extend_selection {
                    self.editor.select_up();
                } else {
                    self.editor.move_up();
                }
            }
            return;
        }

        let rope = self.editor.rope();
        let mut cursor = self.editor.cursor();
        let mut remaining = count;

        let line_text = rope
            .line(cursor.line)
            .unwrap_or(std::borrow::Cow::Borrowed(""));
        let line_text = line_text.trim_end_matches(['\n', '\r']);
        let (mut current_v_row, _) =
            self.cursor_wrap_position(cursor.line, line_text, width, cursor.visual_col);

        // Calculate target screen X (relative to slice start) to preserve column
        let initial_slices = self.line_wrap_slices(cursor.line, line_text, width);
        let initial_slice_start = initial_slices.get(current_v_row).map_or(0, |s| s.start_col);
        let target_screen_x = cursor.visual_col.saturating_sub(initial_slice_start);

        while remaining > 0 {
            if remaining <= current_v_row {
                current_v_row -= remaining;
                remaining = 0;
            } else {
                remaining -= current_v_row + 1;
                if cursor.line > 0 {
                    cursor.line -= 1;
                    let line_text = rope
                        .line(cursor.line)
                        .unwrap_or(std::borrow::Cow::Borrowed(""));
                    let line_text = line_text.trim_end_matches(['\n', '\r']);
                    let wrap_count = self.line_wrap_count(cursor.line, line_text, width);
                    current_v_row = wrap_count.saturating_sub(1);
                } else {
                    current_v_row = 0;
                    remaining = 0;
                }
            }
        }

        let line_text = rope
            .line(cursor.line)
            .unwrap_or(std::borrow::Cow::Borrowed(""));
        let line_text = line_text.trim_end_matches(['\n', '\r']);
        let slices = self.line_wrap_slices(cursor.line, line_text, width);

        if let Some(slice) = slices.get(current_v_row) {
            let target_in_slice =
                slice.clamp_cursor_column(target_screen_x, current_v_row + 1 < slices.len());

            let mut g_idx = 0;
            let mut v_w = 0;
            for g in graphemes(slice.text) {
                let w = display_width(g);
                if v_w + w > target_in_slice {
                    break;
                }
                v_w += w;
                g_idx += 1;
            }

            let nav = CursorNavigator::new(rope);
            let line_start_byte = nav.to_byte_index(nav.from_line_grapheme(cursor.line, 0));
            let slice_byte_offset: usize = graphemes(slice.text).take(g_idx).map(str::len).sum();
            let final_byte = line_start_byte + slice.start_byte + slice_byte_offset;
            cursor = nav.from_byte_index(final_byte);
        }

        if extend_selection {
            self.editor.extend_selection_to(cursor);
        } else {
            self.editor.set_cursor(cursor);
        }
    }

    // ── Selection ──────────────────────────────────────────────────

    /// Extend selection left.
    pub fn select_left(&mut self) {
        self.editor.select_left();
        self.ensure_cursor_visible();
    }

    /// Extend selection right.
    pub fn select_right(&mut self) {
        self.editor.select_right();
        self.ensure_cursor_visible();
    }

    /// Extend selection up.
    pub fn select_up(&mut self) {
        if self.soft_wrap {
            self.move_cursor_visual_up(1, true);
        } else {
            self.editor.select_up();
        }
        self.ensure_cursor_visible();
    }

    /// Extend selection down.
    pub fn select_down(&mut self) {
        if self.soft_wrap {
            self.move_cursor_visual_down(1, true);
        } else {
            self.editor.select_down();
        }
        self.ensure_cursor_visible();
    }

    /// Select all.
    pub fn select_all(&mut self) {
        self.editor.select_all();
    }

    /// Clear selection.
    pub fn clear_selection(&mut self) {
        self.editor.clear_selection();
    }

    // ── Viewport management ────────────────────────────────────────

    /// Page up (move viewport and cursor up by viewport height).
    pub fn page_up(&mut self, state: &TextAreaState) {
        let page = usize::from(state.last_viewport_height.max(1));
        let text_area_width = state
            .last_viewport_width
            .saturating_sub(self.gutter_width());
        self.last_viewport_height.set(page);
        self.last_viewport_width.set(usize::from(text_area_width));
        if self.soft_wrap {
            if text_area_width > 0 {
                self.move_cursor_visual_up(page, false);
            } else {
                for _ in 0..page {
                    self.editor.move_up();
                }
            }
        } else {
            for _ in 0..page {
                self.editor.move_up();
            }
        }
        self.ensure_cursor_visible();
    }

    /// Page down (move viewport and cursor down by viewport height).
    pub fn page_down(&mut self, state: &TextAreaState) {
        let page = usize::from(state.last_viewport_height.max(1));
        let text_area_width = state
            .last_viewport_width
            .saturating_sub(self.gutter_width());
        self.last_viewport_height.set(page);
        self.last_viewport_width.set(usize::from(text_area_width));
        if self.soft_wrap {
            if text_area_width > 0 {
                self.move_cursor_visual_down(page, false);
            } else {
                for _ in 0..page {
                    self.editor.move_down();
                }
            }
        } else {
            for _ in 0..page {
                self.editor.move_down();
            }
        }
        self.ensure_cursor_visible();
    }

    /// Width of the line number gutter.
    fn gutter_width(&self) -> u16 {
        if !self.show_line_numbers {
            return 0;
        }
        let digits = {
            let mut count = self.line_count().max(1);
            let mut d: u16 = 0;
            while count > 0 {
                d += 1;
                count /= 10;
            }
            d
        };
        digits + 2 // digit width + space + separator
    }

    /// Measure wrapped content without allocating the rendered slices.
    ///
    /// The last row's width also tells layout whether an insertion cursor at
    /// the end needs its own cell on a continuation row.
    fn measure_wrap_metrics(line_text: &str, max_width: usize) -> WrapMetrics {
        if line_text.is_empty() {
            return WrapMetrics {
                rows: 1,
                last_width: 0,
                total_width: 0,
            };
        }

        let mut metrics = WrapMetrics {
            rows: 0,
            last_width: 0,
            total_width: 0,
        };
        let mut has_content = false;

        Self::run_wrapping_logic(line_text, max_width, |_, width, flush| {
            if flush {
                if width > 0 {
                    metrics.rows += 1;
                    metrics.total_width = metrics.total_width.saturating_add(width);
                }
                metrics.last_width = 0;
                has_content = false;
            } else {
                metrics.last_width = width;
                has_content = true;
            }
        });

        if has_content || metrics.rows == 0 {
            metrics.rows += 1;
            metrics.total_width = metrics.total_width.saturating_add(metrics.last_width);
        }

        metrics
    }

    /// A full final line retains its continuation while the cursor moves
    /// within the document, so Up/Down can reach its insertion position.
    /// Earlier hard lines need one only while the caret is at their end:
    /// typing a hard newline replaces, rather than duplicates, the final
    /// continuation row. An unfocused widget has no insertion caret.
    fn has_caret_continuation(
        &self,
        line_idx: usize,
        max_width: usize,
        last_width: usize,
        total_width: usize,
    ) -> bool {
        let cursor = self.editor.cursor();
        self.focused
            && max_width > 0
            && last_width == max_width
            && (line_idx == self.line_count().saturating_sub(1)
                || (cursor.line == line_idx && cursor.visual_col == total_width))
    }

    fn line_wrap_count(&self, line_idx: usize, line_text: &str, max_width: usize) -> usize {
        let metrics = Self::measure_wrap_metrics(line_text, max_width);
        metrics
            .rows
            .saturating_add(usize::from(self.has_caret_continuation(
                line_idx,
                max_width,
                metrics.last_width,
                metrics.total_width,
            )))
    }

    /// The same visual rows drive rendering, hit-testing, and navigation.
    /// A virtual continuation has no bytes, so it cannot alter submission,
    /// selection ranges, or the undo buffer.
    fn line_wrap_slices<'a>(
        &self,
        line_idx: usize,
        line_text: &'a str,
        max_width: usize,
    ) -> Vec<WrappedSlice<'a>> {
        let mut slices = Self::wrap_line_slices(line_text, max_width);
        if let Some(last) = slices.last() {
            let total_width = last.start_col.saturating_add(last.width);
            if self.has_caret_continuation(line_idx, max_width, last.width, total_width) {
                slices.push(WrappedSlice {
                    text: "",
                    start_byte: line_text.len(),
                    start_col: total_width,
                    width: 0,
                });
            }
        }
        slices
    }

    /// Core wrapping logic that emits events for layout or slicing.
    ///
    /// The callback receives `(start_index, width, flush)`.
    /// - `flush == true`: The current line is full/done. `width` is the width of the flushed line.
    /// - `flush == false`: Update current line width.
    fn run_wrapping_logic<F>(line_text: &str, max_width: usize, mut callback: F)
    where
        F: FnMut(usize, usize, bool),
    {
        let mut current_width = 0;
        let mut byte_cursor = 0;

        for segment in word_segments(line_text) {
            let seg_len = segment.len();
            let seg_width: usize = graphemes(segment).map(display_width).sum();

            if max_width > 0 && current_width + seg_width > max_width {
                // Flush current
                callback(byte_cursor, current_width, true);
                current_width = 0;
            }

            if max_width > 0 && seg_width > max_width {
                for grapheme in graphemes(segment) {
                    let g_width = display_width(grapheme);
                    let g_len = grapheme.len();

                    if max_width > 0 && current_width + g_width > max_width && current_width > 0 {
                        callback(byte_cursor, current_width, true);
                        current_width = 0;
                    }

                    current_width += g_width;
                    byte_cursor += g_len;
                    callback(byte_cursor, current_width, false);
                }
                continue;
            }

            current_width += seg_width;
            byte_cursor += seg_len;
            callback(byte_cursor, current_width, false);
        }
    }

    fn wrap_line_slices<'a>(line_text: &'a str, max_width: usize) -> Vec<WrappedSlice<'a>> {
        if line_text.is_empty() {
            return vec![WrappedSlice {
                text: "",
                start_byte: 0,
                start_col: 0,
                width: 0,
            }];
        }

        let mut slices = Vec::new();
        let mut current_width = 0;
        let mut slice_start_byte = 0;
        let mut slice_start_col = 0;
        let mut byte_cursor = 0;
        let mut col_cursor = 0;

        let push_current = |slices: &mut Vec<WrappedSlice<'a>>,
                            width: &mut usize,
                            start_byte: &mut usize,
                            start_col: &mut usize,
                            byte_cursor: usize,
                            col_cursor: usize| {
            if byte_cursor == *start_byte && *width == 0 {
                return;
            }
            slices.push(WrappedSlice {
                text: &line_text[*start_byte..byte_cursor],
                start_byte: *start_byte,
                start_col: *start_col,
                width: *width,
            });
            *start_byte = byte_cursor;
            *start_col = col_cursor;
            *width = 0;
        };

        for segment in word_segments(line_text) {
            let seg_len = segment.len();
            let seg_width: usize = graphemes(segment).map(display_width).sum();

            if max_width > 0 && current_width + seg_width > max_width {
                push_current(
                    &mut slices,
                    &mut current_width,
                    &mut slice_start_byte,
                    &mut slice_start_col,
                    byte_cursor,
                    col_cursor,
                );
            }

            if max_width > 0 && seg_width > max_width {
                for grapheme in graphemes(segment) {
                    let g_width = display_width(grapheme);
                    let g_len = grapheme.len();

                    if max_width > 0 && current_width + g_width > max_width && current_width > 0 {
                        push_current(
                            &mut slices,
                            &mut current_width,
                            &mut slice_start_byte,
                            &mut slice_start_col,
                            byte_cursor,
                            col_cursor,
                        );
                    }

                    current_width += g_width;
                    byte_cursor += g_len;
                    col_cursor += g_width;
                }
                continue;
            }

            current_width += seg_width;
            byte_cursor += seg_len;
            col_cursor += seg_width;
        }

        if byte_cursor > slice_start_byte || current_width > 0 || slices.is_empty() {
            slices.push(WrappedSlice {
                text: &line_text[slice_start_byte..byte_cursor],
                start_byte: slice_start_byte,
                start_col: slice_start_col,
                width: current_width,
            });
        }

        slices
    }

    fn cursor_wrap_position(
        &self,
        line_idx: usize,
        line_text: &str,
        max_width: usize,
        cursor_col: usize,
    ) -> (usize, usize) {
        let slices = self.line_wrap_slices(line_idx, line_text, max_width);
        if slices.is_empty() {
            return (0, 0);
        }

        for (idx, slice) in slices.iter().enumerate() {
            let end_col = slice.start_col.saturating_add(slice.width);
            // If cursor is at the wrap boundary (equal to end_col), we prefer the start
            // of the *next* slice (by skipping this one) unless this is the last slice.
            let is_last = idx == slices.len().saturating_sub(1);
            if cursor_col < end_col || (cursor_col == end_col && is_last) {
                let col_in_slice = cursor_col.saturating_sub(slice.start_col);
                return (idx, col_in_slice.min(slice.width));
            }
        }

        (0, 0)
    }

    /// Get the visual width of the character immediately before the cursor.
    fn get_prev_char_width(&self) -> usize {
        let cursor = self.editor.cursor();
        if cursor.grapheme == 0 {
            return 0;
        }
        let rope = self.editor.rope();
        let line = rope
            .line(cursor.line)
            .unwrap_or(std::borrow::Cow::Borrowed(""));

        graphemes(&line)
            .nth(cursor.grapheme - 1)
            .map_or(0, display_width)
    }

    /// Ensure the cursor line and column are visible in the viewport.
    fn ensure_cursor_visible(&self) {
        let cursor = self.editor.cursor();

        let last_height = self.last_viewport_height.get();
        let vp_height = if last_height == 0 { 20 } else { last_height };

        let last_width = self.last_viewport_width.get();
        let vp_width = if last_width == 0 { 80 } else { last_width };

        if self.scroll_anchor.get().0 == usize::MAX {
            self.scroll_anchor.set((0, 0));
        }

        // pi_agent_rust patch (gh #276): an edit can remove the anchor line,
        // and a wider viewport can remove its wrapped row. Normalize before
        // the visibility walk so it never starts from a nonexistent row.
        let (anchor_line, anchor_vrow) = self.scroll_anchor.get();
        let anchor_line = anchor_line.min(self.line_count().saturating_sub(1));
        let anchor_vrow = if self.soft_wrap {
            let line = self
                .editor
                .rope()
                .line(anchor_line)
                .unwrap_or(std::borrow::Cow::Borrowed(""));
            anchor_vrow.min(
                self.line_wrap_count(anchor_line, line.trim_end_matches(['\n', '\r']), vp_width)
                    .saturating_sub(1),
            )
        } else {
            0
        };
        self.scroll_anchor.set((anchor_line, anchor_vrow));

        self.ensure_cursor_visible_internal(vp_height, vp_width, cursor);
        self.fill_viewport_from_end(vp_height, vp_width);
    }

    /// Backfill unused rows at the end of the document without disturbing an
    /// already full viewport. In particular, inserting a newline can scroll
    /// against the *previous* one-row viewport before the host grows it; just
    /// keeping the cursor visible on the next render leaves earlier text hidden.
    /// No cursor, selection, or undo state changes during this reconciliation.
    fn fill_viewport_from_end(&self, vp_height: usize, vp_width: usize) {
        if vp_height == 0 {
            return;
        }
        let (mut anchor_line, mut anchor_vrow) = self.scroll_anchor.get();
        if !self.soft_wrap {
            self.scroll_anchor.set((
                anchor_line.min(self.line_count().saturating_sub(vp_height)),
                0,
            ));
            return;
        }

        let rope = self.editor.rope();
        let mut rows_below = 0_usize;
        for line_idx in anchor_line..self.line_count() {
            let line = rope
                .line(line_idx)
                .unwrap_or(std::borrow::Cow::Borrowed(""));
            let wraps =
                self.line_wrap_count(line_idx, line.trim_end_matches(['\n', '\r']), vp_width);
            let skipped = if line_idx == anchor_line {
                anchor_vrow
            } else {
                0
            };
            rows_below = rows_below.saturating_add(wraps.saturating_sub(skipped));
            if rows_below >= vp_height {
                return;
            }
        }

        let mut missing = vp_height - rows_below;
        let from_current_line = missing.min(anchor_vrow);
        anchor_vrow -= from_current_line;
        missing -= from_current_line;
        while missing > 0 && anchor_line > 0 {
            anchor_line -= 1;
            let line = rope
                .line(anchor_line)
                .unwrap_or(std::borrow::Cow::Borrowed(""));
            let wraps =
                self.line_wrap_count(anchor_line, line.trim_end_matches(['\n', '\r']), vp_width);
            let taken = missing.min(wraps);
            anchor_vrow = wraps - taken;
            missing -= taken;
        }
        self.scroll_anchor.set((anchor_line, anchor_vrow));
    }

    // The upstream forward/backward scans form one viewport operation.
    #[allow(clippy::too_many_lines)]
    fn ensure_cursor_visible_internal(
        &self,
        vp_height: usize,
        vp_width: usize,
        cursor: CursorPosition,
    ) {
        let (anchor_line, anchor_vrow) = self.scroll_anchor.get();

        if !self.soft_wrap {
            // Vertical scroll (logical lines)
            if cursor.line < anchor_line {
                self.scroll_anchor.set((cursor.line, 0));
            } else if vp_height > 0 && cursor.line >= anchor_line + vp_height {
                self.scroll_anchor
                    .set((cursor.line.saturating_sub(vp_height - 1), 0));
            }

            // Horizontal scroll
            let current_left = self.scroll_left.get();
            let visual_col = cursor.visual_col;

            if visual_col < current_left {
                self.scroll_left.set(visual_col);
            } else if vp_width > 0 && visual_col >= current_left + vp_width {
                let candidate_scroll = visual_col.saturating_sub(vp_width - 1);
                let prev_width = self.get_prev_char_width();
                let max_scroll_for_prev = visual_col.saturating_sub(prev_width);

                let new_scroll = if vp_width > prev_width {
                    candidate_scroll.min(max_scroll_for_prev)
                } else {
                    candidate_scroll
                };

                self.scroll_left.set(new_scroll);
            }
            return;
        }

        // Soft wrap logic
        let rope = self.editor.rope();

        // 1. Is cursor before anchor?
        if cursor.line < anchor_line {
            let line_text = rope
                .line(cursor.line)
                .unwrap_or(std::borrow::Cow::Borrowed(""));
            let line_text = line_text.trim_end_matches(['\n', '\r']);
            let (v_row, _) =
                self.cursor_wrap_position(cursor.line, line_text, vp_width, cursor.visual_col);
            self.scroll_anchor.set((cursor.line, v_row));
            return;
        }

        if cursor.line == anchor_line {
            let line_text = rope
                .line(cursor.line)
                .unwrap_or(std::borrow::Cow::Borrowed(""));
            let line_text = line_text.trim_end_matches(['\n', '\r']);
            let (v_row, _) =
                self.cursor_wrap_position(cursor.line, line_text, vp_width, cursor.visual_col);
            if v_row < anchor_vrow {
                self.scroll_anchor.set((cursor.line, v_row));
                return;
            }
        }

        // 2. Is cursor after viewport? Trace forward from anchor.
        let mut visual_rows_capacity = vp_height;
        let mut current_line = anchor_line;
        let mut current_v_start = anchor_vrow;

        loop {
            if current_line > cursor.line {
                // Reached past cursor line without exhausting capacity -> Visible.
                return;
            }

            let line_text = rope
                .line(current_line)
                .unwrap_or(std::borrow::Cow::Borrowed(""));
            let line_text = line_text.trim_end_matches(['\n', '\r']);
            let wrap_count = self.line_wrap_count(current_line, line_text, vp_width);

            if current_line == cursor.line {
                let (cursor_v_row, _) =
                    self.cursor_wrap_position(cursor.line, line_text, vp_width, cursor.visual_col);
                if cursor_v_row >= current_v_start {
                    let displayed_row_index = cursor_v_row - current_v_start;
                    if displayed_row_index < visual_rows_capacity {
                        // Visible.
                        return;
                    } else {
                        // Not visible (below).
                        break;
                    }
                } else {
                    // Cursor is before the anchor start? Handled by step 1.
                    return;
                }
            }

            let rows_remaining_in_line = wrap_count.saturating_sub(current_v_start);
            if rows_remaining_in_line >= visual_rows_capacity {
                break;
            }

            visual_rows_capacity -= rows_remaining_in_line;
            current_line += 1;
            current_v_start = 0;

            if current_line >= self.editor.line_count() {
                break;
            }
        }

        // 3. Scroll Down (Backwards Scan)
        let mut needed = vp_height;
        let mut scan_line = cursor.line;

        // For the cursor line itself
        let line_text = rope
            .line(scan_line)
            .unwrap_or(std::borrow::Cow::Borrowed(""));
        let line_text = line_text.trim_end_matches(['\n', '\r']);
        let (cursor_v, _) =
            self.cursor_wrap_position(cursor.line, line_text, vp_width, cursor.visual_col);

        let rows_above = cursor_v + 1;
        if rows_above >= needed {
            let new_v_start = cursor_v + 1 - needed;
            self.scroll_anchor.set((scan_line, new_v_start));
            return;
        }

        needed -= rows_above;

        // Scan previous lines
        while scan_line > 0 {
            scan_line -= 1;
            let line_text = rope
                .line(scan_line)
                .unwrap_or(std::borrow::Cow::Borrowed(""));
            let line_text = line_text.trim_end_matches(['\n', '\r']);
            let wrap_count = self.line_wrap_count(scan_line, line_text, vp_width);

            if wrap_count >= needed {
                let new_v_start = wrap_count - needed;
                self.scroll_anchor.set((scan_line, new_v_start));
                return;
            }
            needed -= wrap_count;
        }

        // Reached top
        self.scroll_anchor.set((0, 0));
    }
}

impl Widget for TextArea {
    // Keep the upstream soft-wrap and no-wrap render paths together.
    #[allow(clippy::too_many_lines)]
    fn render(&self, area: Rect, frame: &mut Frame) {
        if area.width < 1 || area.height < 1 {
            self.reset_mouse_interaction();
            return;
        }

        if self.last_area.get() != area {
            // A press in the old rectangle cannot start a selection in the
            // relocated or resized editor. Unchanged frames keep the gesture.
            self.reset_mouse_interaction();
        }

        if frame.a11y_enabled() {
            frame.push_a11y_nodes(ftui_a11y::Accessible::accessibility_nodes(self, area));
        }

        self.last_viewport_height.set(usize::from(area.height));
        self.last_area.set(area);
        self.mouse_area_valid.set(true);

        let deg = frame.buffer.degradation;
        let base_style = if deg.apply_styling() {
            self.style
        } else {
            Style::default()
        };
        clear_text_area(frame, area, base_style);

        let gutter_w = self.gutter_width();
        let text_area_x = area.x.saturating_add(gutter_w);
        let text_area_w = usize::from(area.width.saturating_sub(gutter_w));
        let vp_height = usize::from(area.height);

        self.last_viewport_width.set(text_area_w);

        let cursor = self.editor.cursor();

        self.ensure_cursor_visible();

        // Re-fetch potentially updated anchor
        let (scroll_top_line, scroll_top_vrow) = self.scroll_anchor.get();
        let scroll_left = self.scroll_left.get();

        let rope = self.editor.rope();
        let nav = CursorNavigator::new(rope);

        // Selection byte range for highlighting
        let sel_range = self.editor.selection().and_then(|sel| {
            if sel.is_empty() {
                None
            } else {
                let (a, b) = sel.byte_range(&nav);
                Some((a, b))
            }
        });

        // Show placeholder if empty
        if self.editor.is_empty() && !self.placeholder.is_empty() {
            let style = if deg.apply_styling() {
                self.placeholder_style
            } else {
                Style::default()
            };
            draw_text_span(
                frame,
                text_area_x,
                area.y,
                &self.placeholder,
                style,
                area.right(),
            );
            if self.focused {
                frame.set_cursor(Some((text_area_x, area.y)));
            }
            return;
        }

        if self.soft_wrap {
            self.scroll_left.set(0);

            // Pre-calculate cursor wrap position
            let cursor_line_text = rope
                .line(cursor.line)
                .unwrap_or(std::borrow::Cow::Borrowed(""));
            let cursor_line_text = cursor_line_text.trim_end_matches(['\n', '\r']);
            let (cursor_wrap_idx, cursor_col_in_wrap) = self.cursor_wrap_position(
                cursor.line,
                cursor_line_text,
                text_area_w,
                cursor.visual_col,
            );

            // Render wrapped lines starting from anchor
            let mut current_y = area.y;
            let bottom_y = area.bottom();
            let line_count = self.editor.line_count();

            for line_idx in scroll_top_line..line_count {
                if current_y >= bottom_y {
                    break;
                }

                let line_text = rope
                    .line(line_idx)
                    .unwrap_or(std::borrow::Cow::Borrowed(""));
                let line_text = line_text.trim_end_matches(['\n', '\r']);

                let line_start_byte = nav.to_byte_index(nav.from_line_grapheme(line_idx, 0));
                // Syntax highlight spans for the whole logical line, computed
                // once and reused across every wrapped slice.
                let hl_spans = self.line_spans(line_idx, line_text, deg.apply_styling());
                let slices = self.line_wrap_slices(line_idx, line_text, text_area_w);

                // If this is the start line, skip slices before anchor_vrow
                let start_slice = if line_idx == scroll_top_line {
                    scroll_top_vrow
                } else {
                    0
                };

                for (slice_idx, slice) in slices.iter().enumerate().skip(start_slice) {
                    if current_y >= bottom_y {
                        break;
                    }

                    // Line number gutter (only for first wrapped slice of the line)
                    if self.show_line_numbers && slice_idx == 0 {
                        let style = if deg.apply_styling() {
                            self.line_number_style
                        } else {
                            Style::default()
                        };
                        let num_str = format!(
                            "{:>width$} ",
                            line_idx + 1,
                            width = usize::from(gutter_w - 2)
                        );
                        draw_text_span(frame, area.x, current_y, &num_str, style, text_area_x);
                    }

                    // Cursor line highlight
                    if line_idx == cursor.line
                        && slice_idx == cursor_wrap_idx
                        && let Some(cl_style) = self.cursor_line_style
                        && deg.apply_styling()
                    {
                        for cx in text_area_x..area.right() {
                            if let Some(cell) = frame.buffer.get_mut(cx, current_y) {
                                apply_style(cell, cl_style);
                            }
                        }
                    }

                    // Render graphemes inside the wrapped slice
                    let mut visual_x: usize = 0;
                    let mut grapheme_byte_offset = line_start_byte + slice.start_byte;

                    for g in graphemes(slice.text) {
                        let g_width = display_width(g);
                        let g_byte_len = g.len();

                        if visual_x >= text_area_w {
                            break;
                        }

                        let px = text_area_x + u16::try_from(visual_x).unwrap_or(u16::MAX);

                        // Syntax highlight first, then selection over it.
                        let mut g_style = base_style;
                        if let Some(hl) = style_at(
                            &hl_spans,
                            grapheme_byte_offset.saturating_sub(line_start_byte),
                        ) {
                            g_style = g_style.patch(&hl);
                        }
                        if let Some((sel_start, sel_end)) = sel_range
                            && grapheme_byte_offset >= sel_start
                            && grapheme_byte_offset < sel_end
                            && deg.apply_styling()
                        {
                            g_style = g_style.patch(&self.selection_style);
                        }

                        if g_width > 0 {
                            draw_text_span(frame, px, current_y, g, g_style, area.right());
                        }

                        visual_x += g_width;
                        grapheme_byte_offset += g_byte_len;
                    }

                    // Set cursor position if focused
                    if self.focused && line_idx == cursor.line && slice_idx == cursor_wrap_idx {
                        let cursor_screen_x = text_area_x
                            .saturating_add(u16::try_from(cursor_col_in_wrap).unwrap_or(u16::MAX));
                        if cursor_screen_x < area.right() {
                            frame.set_cursor(Some((cursor_screen_x, current_y)));
                        }
                    }

                    current_y += 1;
                }
            }

            return;
        }

        // Render visible lines (no soft wrap)
        for row in 0..vp_height {
            let line_idx = scroll_top_line + row;
            let y = area
                .y
                .saturating_add(u16::try_from(row).unwrap_or(u16::MAX));

            if line_idx >= self.editor.line_count() {
                break;
            }

            // Line number gutter
            if self.show_line_numbers {
                let style = if deg.apply_styling() {
                    self.line_number_style
                } else {
                    Style::default()
                };
                let num_str = format!(
                    "{:>width$} ",
                    line_idx + 1,
                    width = usize::from(gutter_w - 2)
                );
                draw_text_span(frame, area.x, y, &num_str, style, text_area_x);
            }

            // Cursor line highlight
            if line_idx == cursor.line
                && let Some(cl_style) = self.cursor_line_style
                && deg.apply_styling()
            {
                for cx in text_area_x..area.right() {
                    if let Some(cell) = frame.buffer.get_mut(cx, y) {
                        apply_style(cell, cl_style);
                    }
                }
            }

            // Get line text
            let line_text = rope
                .line(line_idx)
                .unwrap_or(std::borrow::Cow::Borrowed(""));
            let line_text = line_text.trim_end_matches(['\n', '\r']);

            // Calculate line byte offset for selection mapping
            let line_start_byte = nav.to_byte_index(nav.from_line_grapheme(line_idx, 0));
            let hl_spans = self.line_spans(line_idx, line_text, deg.apply_styling());

            // Render each grapheme
            let mut visual_x: usize = 0;
            let graphemes: Vec<&str> = graphemes(line_text).collect();
            let mut grapheme_byte_offset = line_start_byte;

            for g in &graphemes {
                let g_width = display_width(g);
                let g_byte_len = g.len();

                // Syntax highlight first, then selection over it.
                let mut g_style = base_style;
                if let Some(hl) = style_at(
                    &hl_spans,
                    grapheme_byte_offset.saturating_sub(line_start_byte),
                ) {
                    g_style = g_style.patch(&hl);
                }
                if let Some((sel_start, sel_end)) = sel_range
                    && grapheme_byte_offset >= sel_start
                    && grapheme_byte_offset < sel_end
                    && deg.apply_styling()
                {
                    g_style = g_style.patch(&self.selection_style);
                }

                // Skip graphemes before horizontal scroll
                if visual_x + g_width <= scroll_left {
                    visual_x += g_width;
                    grapheme_byte_offset += g_byte_len;
                    continue;
                }

                // Handle partial overlap at left edge
                if visual_x < scroll_left {
                    // Calculate visible width of the partially scrolled grapheme
                    let end_x = visual_x + g_width;
                    let visible_width = end_x.saturating_sub(scroll_left);

                    // Render padding spaces for the visible portion to preserve background style
                    for i in 0..visible_width {
                        let screen_x = i; // Starts at 0 relative to text area
                        let px = text_area_x + u16::try_from(screen_x).unwrap_or(u16::MAX);
                        if px < area.right() {
                            draw_text_span(frame, px, y, " ", g_style, area.right());
                        }
                    }

                    visual_x += g_width;
                    grapheme_byte_offset += g_byte_len;
                    continue;
                }

                // Stop if past viewport
                let screen_x = visual_x.saturating_sub(scroll_left);
                if screen_x >= text_area_w {
                    break;
                }

                let px = text_area_x + u16::try_from(screen_x).unwrap_or(u16::MAX);

                // Write grapheme to buffer
                if g_width > 0 {
                    draw_text_span(frame, px, y, g, g_style, area.right());
                }

                visual_x += g_width;
                grapheme_byte_offset += g_byte_len;
            }
        }

        // Set cursor position if focused
        if self.focused {
            let cursor_row = cursor.line.saturating_sub(scroll_top_line);
            if cursor_row < vp_height {
                let cursor_screen_x = u16::try_from(cursor.visual_col.saturating_sub(scroll_left))
                    .unwrap_or(u16::MAX)
                    .saturating_add(text_area_x);
                let cursor_screen_y = area
                    .y
                    .saturating_add(u16::try_from(cursor_row).unwrap_or(u16::MAX));
                if cursor_screen_x < area.right() && cursor_screen_y < area.bottom() {
                    frame.set_cursor(Some((cursor_screen_x, cursor_screen_y)));
                }
            }
        }
    }

    fn is_essential(&self) -> bool {
        true
    }
}

impl ftui_a11y::Accessible for TextArea {
    fn accessibility_nodes(&self, area: Rect) -> Vec<ftui_a11y::node::A11yNodeInfo> {
        use ftui_a11y::node::{A11yNodeInfo, A11yRole, A11yState};

        if area.is_empty() {
            return Vec::new();
        }
        let text = self.text();
        let name = if text.is_empty() {
            self.placeholder.clone()
        } else {
            text
        };
        vec![
            A11yNodeInfo::new(a11y_node_id(area), A11yRole::TextInput, area)
                .with_name(name)
                .with_description("multi-line text input")
                .with_state(A11yState {
                    focused: self.focused,
                    ..A11yState::default()
                }),
        ]
    }
}

impl StatefulWidget for TextArea {
    type State = TextAreaState;

    fn render(&self, area: Rect, frame: &mut Frame, state: &mut Self::State) {
        state.last_viewport_height = area.height;
        state.last_viewport_width = area.width;
        Widget::render(self, area, frame);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod mouse {
        use super::*;
        use ftui_render::grapheme_pool::GraphemePool;

        fn rendered(ta: &TextArea, area: Rect) {
            let mut pool = GraphemePool::new();
            let mut frame = Frame::new(area.right(), area.bottom(), &mut pool);
            Widget::render(ta, area, &mut frame);
        }

        fn press(ta: &mut TextArea, x: u16, y: u16, at_ms: u64) -> bool {
            let down = MouseEvent::new(MouseEventKind::Down(MouseButton::Left), x, y);
            let changed = ta.handle_event_at(&Event::Mouse(down), at_ms);
            let up = MouseEvent::new(MouseEventKind::Up(MouseButton::Left), x, y);
            ta.handle_event_at(&Event::Mouse(up), at_ms);
            changed
        }

        const AREA: Rect = Rect::new(2, 1, 30, 5);

        #[test]
        fn click_places_the_cursor_under_the_pointer() {
            let mut ta = TextArea::new().with_text("hello world\nsecond line");
            rendered(&ta, AREA);
            assert!(press(&mut ta, AREA.x + 7, AREA.y + 1, 0));
            assert_eq!((ta.cursor().line, ta.cursor().grapheme), (1, 7));
            assert_eq!(ta.selection(), None);
            // Past the end of a line lands at its end.
            press(&mut ta, AREA.x + 25, AREA.y, 1000);
            assert_eq!((ta.cursor().line, ta.cursor().grapheme), (0, 11));
        }

        #[test]
        fn textarea_double_click_selects_word() {
            let mut ta = TextArea::new().with_text("hello brave world");
            rendered(&ta, AREA);
            press(&mut ta, AREA.x + 8, AREA.y, 0);
            rendered(&ta, AREA);
            press(&mut ta, AREA.x + 8, AREA.y, 200);
            assert_eq!(ta.selected_text().as_deref(), Some("brave"));
        }

        #[test]
        fn textarea_triple_click_selects_line() {
            let mut ta = TextArea::new().with_text("first line\nhello brave world\nlast");
            rendered(&ta, AREA);
            for at in [0, 150, 300] {
                press(&mut ta, AREA.x + 3, AREA.y + 1, at);
                rendered(&ta, AREA);
            }
            assert_eq!(ta.selected_text().as_deref(), Some("hello brave world"));
        }

        #[test]
        fn slow_or_distant_second_press_is_a_new_click() {
            let mut ta = TextArea::new().with_text("hello brave world");
            rendered(&ta, AREA);
            press(&mut ta, AREA.x + 8, AREA.y, 0);
            press(&mut ta, AREA.x + 8, AREA.y, 301);
            assert_eq!(ta.selection(), None, "outside the 300 ms window");
            press(&mut ta, AREA.x + 14, AREA.y, 400);
            assert_eq!(ta.selection(), None, "more than one cell away");
            assert_eq!(ta.cursor().grapheme, 14);
        }

        #[test]
        fn drag_extends_the_selection_from_the_press() {
            let mut ta = TextArea::new().with_text("hello brave world\nnext");
            rendered(&ta, AREA);
            let down = MouseEvent::new(MouseEventKind::Down(MouseButton::Left), AREA.x + 6, AREA.y);
            ta.handle_event_at(&Event::Mouse(down), 0);
            rendered(&ta, AREA);
            let drag = MouseEvent::new(
                MouseEventKind::Drag(MouseButton::Left),
                AREA.x + 2,
                AREA.y + 1,
            );
            assert!(ta.handle_event_at(&Event::Mouse(drag), 10));
            assert_eq!(ta.selected_text().as_deref(), Some("brave world\nne"));
        }

        #[test]
        fn mouse_reset_preserves_editor_state_and_waits_for_render() {
            let mut ta = TextArea::new().with_text("hello world\nnext");
            ta.move_to_document_end();
            ta.insert_text("!");
            rendered(&ta, AREA);
            let down = MouseEvent::new(MouseEventKind::Down(MouseButton::Left), AREA.x + 1, AREA.y);
            let drag = MouseEvent::new(MouseEventKind::Drag(MouseButton::Left), AREA.x + 5, AREA.y);
            ta.handle_event_at(&Event::Mouse(down), 0);
            ta.handle_event_at(&Event::Mouse(drag), 10);
            assert_eq!(ta.selected_text().as_deref(), Some("ello"));
            let cursor = ta.cursor();
            let selection = ta.selection();
            let history = (
                ta.undo_group_count(),
                ta.undo_op_count(),
                ta.redo_group_count(),
            );
            assert!(history.0 > 0);

            ta.reset_mouse_interaction();
            assert_eq!(ta.rendered_width(), Some(AREA.width));
            assert!(!ta.handle_event_at(&Event::Mouse(drag), 20));
            assert!(!ta.handle_event_at(&Event::Mouse(down), 30));
            assert_eq!(ta.text(), "hello world\nnext!");
            assert_eq!(ta.cursor(), cursor);
            assert_eq!(ta.selection(), selection);
            assert_eq!(
                (
                    ta.undo_group_count(),
                    ta.undo_op_count(),
                    ta.redo_group_count(),
                ),
                history
            );

            rendered(&ta, AREA);
            assert!(!ta.handle_event_at(&Event::Mouse(drag), 40));
            assert!(press(&mut ta, down.x, down.y, 100));
            assert_eq!(ta.selection(), None, "the old click sequence was cancelled");
            assert_eq!(ta.cursor().grapheme, 1);
            ta.undo();
            assert_eq!(ta.text(), "hello world\nnext");
            ta.redo();
            assert_eq!(ta.text(), "hello world\nnext!");
        }

        #[test]
        fn replacing_text_cancels_mouse_and_keeps_cursor_contracts() {
            for builder in [false, true] {
                let mut ta = TextArea::new().with_text("hello world");
                rendered(&ta, AREA);
                let down =
                    MouseEvent::new(MouseEventKind::Down(MouseButton::Left), AREA.x + 1, AREA.y);
                let drag =
                    MouseEvent::new(MouseEventKind::Drag(MouseButton::Left), AREA.x + 3, AREA.y);
                ta.handle_event_at(&Event::Mouse(down), 0);
                if builder {
                    ta = ta.with_text("new\ntext");
                } else {
                    ta.set_text("new\ntext");
                }
                let expected = if builder { (0, 0) } else { (1, 4) };
                assert_eq!((ta.cursor().line, ta.cursor().grapheme), expected);
                assert_eq!(ta.rendered_width(), Some(AREA.width));
                assert!(!ta.handle_event_at(&Event::Mouse(down), 20));
                assert!(!ta.handle_event_at(&Event::Mouse(drag), 30));
                assert_eq!((ta.cursor().line, ta.cursor().grapheme), expected);

                rendered(&ta, AREA);
                assert!(!ta.handle_event_at(&Event::Mouse(drag), 40));
                assert!(press(&mut ta, down.x, down.y, 100));
                assert_eq!(ta.selection(), None, "builder={builder}");
                assert_eq!((ta.cursor().line, ta.cursor().grapheme), (0, 1));
                assert_eq!(ta.text(), "new\ntext");
            }
        }

        #[test]
        fn focus_loss_cancels_mouse_without_clearing_selection() {
            for focus_event in [false, true] {
                let mut ta = TextArea::new().with_text("hello world").with_focus(true);
                rendered(&ta, AREA);
                let down =
                    MouseEvent::new(MouseEventKind::Down(MouseButton::Left), AREA.x + 1, AREA.y);
                let drag =
                    MouseEvent::new(MouseEventKind::Drag(MouseButton::Left), AREA.x + 5, AREA.y);
                ta.handle_event_at(&Event::Mouse(down), 0);
                ta.handle_event_at(&Event::Mouse(drag), 10);
                let cursor = ta.cursor();
                let selection = ta.selection();
                if focus_event {
                    ta.handle_event_at(&Event::Focus(false), 20);
                } else {
                    ta.set_focused(false);
                }
                assert!(!ta.handle_event_at(&Event::Mouse(down), 30));
                assert!(!ta.handle_event_at(&Event::Mouse(drag), 40));
                assert_eq!(ta.cursor(), cursor);
                assert_eq!(ta.selection(), selection);
                assert_eq!(ta.text(), "hello world");
                assert_eq!(ta.rendered_width(), Some(AREA.width));

                ta.set_focused(true);
                rendered(&ta, AREA);
                assert!(!ta.handle_event_at(&Event::Mouse(drag), 50));
                assert!(press(&mut ta, down.x, down.y, 100));
                assert_eq!(ta.selection(), None, "focus_event={focus_event}");
            }
        }

        #[test]
        fn empty_render_invalidates_mouse_but_retains_layout_width() {
            for empty in [
                Rect::new(AREA.x, AREA.y, 0, AREA.height),
                Rect::new(AREA.x, AREA.y, AREA.width, 0),
            ] {
                let mut ta = TextArea::new().with_text("hello world");
                rendered(&ta, AREA);
                let down =
                    MouseEvent::new(MouseEventKind::Down(MouseButton::Left), AREA.x + 1, AREA.y);
                let drag =
                    MouseEvent::new(MouseEventKind::Drag(MouseButton::Left), AREA.x + 5, AREA.y);
                ta.handle_event_at(&Event::Mouse(down), 0);
                let cursor = ta.cursor();
                rendered(&ta, empty);
                assert_eq!(ta.rendered_width(), Some(AREA.width));
                assert!(!ta.handle_event_at(&Event::Mouse(drag), 20));
                assert!(!ta.handle_event_at(&Event::Mouse(down), 30));
                assert_eq!(ta.cursor(), cursor);

                rendered(&ta, AREA);
                assert!(!ta.handle_event_at(&Event::Mouse(drag), 40));
                assert!(press(&mut ta, down.x, down.y, 100));
                assert_eq!(ta.selection(), None, "empty={empty:?}");
                assert_eq!(ta.text(), "hello world");
            }
        }

        #[test]
        fn changed_render_rectangle_cancels_drag_and_click_sequence() {
            for moved in [
                Rect::new(AREA.x + 1, AREA.y, AREA.width, AREA.height),
                Rect::new(AREA.x, AREA.y + 1, AREA.width, AREA.height),
                Rect::new(AREA.x, AREA.y, AREA.width - 1, AREA.height),
                Rect::new(AREA.x, AREA.y, AREA.width, AREA.height - 1),
            ] {
                let mut ta = TextArea::new().with_text("hello world\nnext line");
                rendered(&ta, AREA);
                let down = MouseEvent::new(
                    MouseEventKind::Down(MouseButton::Left),
                    AREA.x + 2,
                    AREA.y + 1,
                );
                ta.handle_event_at(&Event::Mouse(down), 0);
                let cursor = ta.cursor();
                rendered(&ta, moved);
                assert_eq!(ta.rendered_width(), Some(moved.width));
                let drag = MouseEvent::new(
                    MouseEventKind::Drag(MouseButton::Left),
                    moved.x + 5,
                    moved.y,
                );
                assert!(!ta.handle_event_at(&Event::Mouse(drag), 50));
                assert_eq!(ta.cursor(), cursor);
                assert!(press(&mut ta, down.x, down.y, 100));
                assert_eq!(ta.selection(), None, "moved={moved:?}");
                assert_eq!(ta.text(), "hello world\nnext line");
            }
        }

        #[test]
        fn outside_press_breaks_click_sequence_without_invalidating_area() {
            let mut ta = TextArea::new().with_text("hello world");
            rendered(&ta, AREA);
            assert!(press(&mut ta, AREA.x + 2, AREA.y, 0));
            assert!(!press(&mut ta, AREA.x - 1, AREA.y, 50));
            assert!(press(&mut ta, AREA.x + 2, AREA.y, 100));
            assert_eq!(ta.selection(), None);
            assert_eq!(ta.cursor().grapheme, 2);
        }

        #[test]
        fn events_outside_the_area_are_ignored() {
            let mut ta = TextArea::new().with_text("hello");
            // Before any render there is no area to hit.
            assert!(!press(&mut ta, 0, 0, 0));
            rendered(&ta, AREA);
            let before = ta.cursor();
            assert!(!press(&mut ta, AREA.x - 1, AREA.y, 1000));
            assert!(!press(&mut ta, AREA.x, AREA.bottom(), 2000));
            let drag = MouseEvent::new(MouseEventKind::Drag(MouseButton::Left), AREA.x + 2, AREA.y);
            assert!(
                !ta.handle_event_at(&Event::Mouse(drag), 3000),
                "no drag without a press inside"
            );
            assert_eq!(ta.cursor(), before);
        }

        #[test]
        fn click_accounts_for_gutter_and_vertical_scroll() {
            let text: String = (0..20).map(|i| format!("line{i}\n")).collect();
            let mut ta = TextArea::new().with_text(&text).with_line_numbers(true);
            ta.set_cursor_position(CursorPosition::new(19, 0, 0));
            rendered(&ta, AREA);
            let (top, _) = ta.scroll_anchor.get();
            assert!(top > 0, "the cursor at line 19 scrolled the view");
            let gutter = ta.gutter_width();
            press(&mut ta, AREA.x + gutter + 4, AREA.y + 2, 0);
            assert_eq!((ta.cursor().line, ta.cursor().grapheme), (top + 2, 4));
            // A click in the gutter lands at column 0.
            press(&mut ta, AREA.x, AREA.y, 1000);
            assert_eq!((ta.cursor().line, ta.cursor().grapheme), (top, 0));
        }

        #[test]
        fn click_on_a_wrapped_row_maps_into_its_slice() {
            // Ten columns wide: "aaaa bbbb " fits the first row, "cccc" wraps.
            let mut ta = TextArea::new()
                .with_text("aaaa bbbb cccc")
                .with_soft_wrap(true);
            let area = Rect::new(0, 0, 10, 3);
            rendered(&ta, area);
            press(&mut ta, 2, 1, 0);
            assert_eq!((ta.cursor().line, ta.cursor().grapheme), (0, 12));
            // Past the end of a row that wraps stays on that row.
            press(&mut ta, 9, 0, 1000);
            assert_eq!(ta.cursor().grapheme, 9);
        }

        fn key(ta: &mut TextArea, code: KeyCode, modifiers: Modifiers) {
            ta.handle_event(&Event::Key(KeyEvent {
                code,
                modifiers,
                kind: KeyEventKind::Press,
            }));
        }

        fn drawn_cursor(ta: &TextArea, area: Rect) -> (Option<(u16, u16)>, Vec<String>) {
            let mut pool = GraphemePool::new();
            let mut frame = Frame::new(area.right(), area.bottom(), &mut pool);
            Widget::render(ta, area, &mut frame);
            let row: Vec<String> = (area.x..area.right())
                .map(|x| {
                    let cell = frame.buffer.get(x, area.y).unwrap();
                    cell.content
                        .as_char()
                        .map(String::from)
                        .or_else(|| {
                            cell.content
                                .grapheme_id()
                                .and_then(|id| frame.pool.get(id).map(str::to_string))
                        })
                        .unwrap_or_default()
                })
                .collect();
            (frame.cursor_position, row)
        }

        /// #102 (3): TextArea draws text in logical order, so keys, the drawn
        /// caret, clicks, delete and undo must all use the same logical
        /// layout, with whole grapheme clusters (default features include
        /// `bidi`).
        #[test]
        fn rtl_and_clusters_render_navigate_click_delete_consistently() {
            let text = "a\u{0301} \u{05D0}\u{05D1}";
            for wrap in [false, true] {
                let mut ta = TextArea::new()
                    .with_text(text)
                    .with_soft_wrap(wrap)
                    .with_focus(true);
                let (_, row) = drawn_cursor(&ta, AREA);
                assert_eq!(
                    &row[..4],
                    ["a\u{0301}", " ", "\u{05D0}", "\u{05D1}"],
                    "wrap={wrap}"
                );

                key(&mut ta, KeyCode::Home, Modifiers::NONE);
                assert_eq!(ta.cursor().grapheme, 0);
                key(&mut ta, KeyCode::Right, Modifiers::NONE);
                key(&mut ta, KeyCode::Right, Modifiers::NONE);
                assert_eq!(ta.cursor().grapheme, 2, "wrap={wrap}");
                let (cursor, _) = drawn_cursor(&ta, AREA);
                assert_eq!(
                    cursor,
                    Some((AREA.x + 2, AREA.y)),
                    "wrap={wrap}: caret before the alef"
                );

                key(&mut ta, KeyCode::End, Modifiers::NONE);
                assert_eq!(ta.cursor().grapheme, 4);
                let (cursor, _) = drawn_cursor(&ta, AREA);
                assert_eq!(cursor, Some((AREA.x + 4, AREA.y)), "wrap={wrap}");

                // Clicking between the alef and bet lands between them.
                press(&mut ta, AREA.x + 3, AREA.y, 0);
                assert_eq!(ta.cursor().grapheme, 3, "wrap={wrap}");
                key(&mut ta, KeyCode::Backspace, Modifiers::NONE);
                assert_eq!(ta.text(), "a\u{0301} \u{05D1}", "wrap={wrap}");
                ta.undo();
                assert_eq!(ta.text(), text, "wrap={wrap}");

                // Backspace after the combining cluster removes it whole.
                press(&mut ta, AREA.x + 1, AREA.y, 5000);
                assert_eq!(ta.cursor().grapheme, 1, "wrap={wrap}");
                key(&mut ta, KeyCode::Backspace, Modifiers::NONE);
                assert_eq!(ta.text(), " \u{05D0}\u{05D1}", "wrap={wrap}");
            }
        }
    }

    #[test]
    fn undo_groups_keyboard_clock_paste_and_focus() {
        let key = |c| {
            Event::Key(KeyEvent {
                code: KeyCode::Char(c),
                modifiers: Modifiers::NONE,
                kind: KeyEventKind::Press,
            })
        };
        let mut ta = TextArea::new();
        for (tick, c) in "hello world".chars().enumerate() {
            ta.handle_event_at(&key(c), u64::try_from(tick).expect("test clock"));
        }
        assert_eq!(ta.undo_group_count(), 2);
        assert_eq!(ta.undo_op_count(), 11);
        ta.handle_event_at(
            &Event::Key(KeyEvent {
                code: KeyCode::Char('z'),
                modifiers: Modifiers::CTRL,
                kind: KeyEventKind::Press,
            }),
            11,
        );
        assert_eq!(ta.text(), "hello ");
        ta.redo();
        ta.handle_event_at(&key('x'), 600);
        assert_eq!(ta.undo_group_count(), 3);
        ta.handle_event_at(
            &Event::Paste(ftui_core::event::PasteEvent::bracketed("p")),
            601,
        );
        ta.handle_event_at(&key('y'), 602);
        assert_eq!(ta.undo_group_count(), 5);
        ta.handle_event_at(&Event::Focus(false), 603);
        ta.handle_event_at(&key('z'), 604);
        assert_eq!(ta.undo_group_count(), 6);
        ta.set_focused(false);
        ta.handle_event_at(&key('q'), 605);
        assert_eq!(ta.undo_group_count(), 7);
        // Exercise the normal event entrypoint without a wall-clock deadline.
        let mut live = TextArea::new();
        live.set_undo_coalesce_idle(Duration::MAX);
        live.handle_event(&key('a'));
        live.handle_event(&key('b'));
        assert_eq!(live.undo_group_count(), 1);
        live.undo();
        assert_eq!(live.text(), "");
    }

    #[test]
    fn selection_edit_keyboard_undo_redo_restores_cursor() {
        for edit in [KeyCode::Char('X'), KeyCode::Backspace, KeyCode::Delete] {
            let mut ta = TextArea::new().with_text("界e\u{301}\nsecond");
            let key = |code, modifiers| {
                Event::Key(KeyEvent {
                    code,
                    modifiers,
                    kind: KeyEventKind::Press,
                })
            };
            ta.handle_event(&key(KeyCode::Char('a'), Modifiers::CTRL));
            let original_cursor = ta.cursor();
            assert_eq!(original_cursor, CursorPosition::new(1, 6, 6));
            ta.handle_event(&key(edit, Modifiers::empty()));
            let expected = if edit == KeyCode::Char('X') { "X" } else { "" };
            assert_eq!(ta.text(), expected);
            let edited_cursor = ta.cursor();
            ta.handle_event(&key(KeyCode::Char('z'), Modifiers::CTRL));
            assert_eq!(ta.text(), "界e\u{301}\nsecond");
            assert_eq!(ta.cursor(), original_cursor);
            ta.handle_event(&key(KeyCode::Char('y'), Modifiers::CTRL));
            assert_eq!(ta.text(), expected);
            assert_eq!(ta.cursor(), edited_cursor);
        }
    }

    #[test]
    fn paragraph_keys_move_select_and_preserve_line_navigation() {
        for soft_wrap in [false, true] {
            let mut ta = TextArea::new()
                .with_text("first\ncontinued\n\nsecond\n\nlast")
                .with_soft_wrap(soft_wrap);
            ta.move_to_document_start();
            let key = |code, modifiers, kind| {
                Event::Key(KeyEvent {
                    code,
                    modifiers,
                    kind,
                })
            };
            assert!(ta.handle_event(&key(KeyCode::Down, Modifiers::CTRL, KeyEventKind::Press)));
            assert_eq!(ta.cursor(), CursorPosition::new(2, 0, 0));
            assert!(ta.handle_event(&key(
                KeyCode::Down,
                Modifiers::CTRL | Modifiers::SHIFT,
                KeyEventKind::Repeat
            )));
            assert_eq!(ta.cursor(), CursorPosition::new(4, 0, 0));
            assert_eq!(ta.selected_text().as_deref(), Some("\nsecond\n"));
            let anchor = ta.selection().unwrap().anchor;
            ta.handle_event(&key(
                KeyCode::Up,
                Modifiers::CTRL | Modifiers::SHIFT,
                KeyEventKind::Press,
            ));
            assert_eq!(ta.selection().unwrap().anchor, anchor);
            assert_eq!(ta.cursor(), anchor);
            assert!(!ta.handle_event(&key(KeyCode::Up, Modifiers::CTRL, KeyEventKind::Release)));
            assert_eq!(ta.cursor(), anchor);
            ta.handle_event(&key(KeyCode::Up, Modifiers::CTRL, KeyEventKind::Press));
            assert_eq!(ta.cursor(), CursorPosition::default());
            assert!(ta.selection().is_none());
            ta.handle_event(&key(KeyCode::Down, Modifiers::empty(), KeyEventKind::Press));
            assert_eq!(ta.cursor().line, 1);
            ta.handle_event(&key(KeyCode::Up, Modifiers::SHIFT, KeyEventKind::Press));
            assert_eq!(ta.cursor().line, 0);
            assert_eq!(ta.selected_text().as_deref(), Some("first\n"));
        }
    }

    fn raw_row_text(frame: &Frame, y: u16, width: u16) -> String {
        (0..width)
            .map(|x| {
                frame
                    .buffer
                    .get(x, y)
                    .and_then(|cell| cell.content.as_char())
                    .unwrap_or(' ')
            })
            .collect()
    }

    fn viewport_snapshot(
        textarea: &TextArea,
        width: u16,
        height: u16,
    ) -> (Vec<String>, Option<(u16, u16)>) {
        let mut pool = ftui_render::grapheme_pool::GraphemePool::new();
        let mut frame = Frame::new(width, height, &mut pool);
        Widget::render(textarea, Rect::new(0, 0, width, height), &mut frame);
        let rows = (0..height)
            .map(|row| raw_row_text(&frame, row, width).trim_end().to_string())
            .collect();
        (rows, frame.cursor_position)
    }

    #[test]
    fn visual_row_count_includes_empty_lines_and_native_word_grapheme_wraps() {
        for (text, width, expected) in [
            ("", 10, 1),
            ("\n", 10, 2),
            ("a\n\n", 10, 3),
            ("12345\n", 5, 2),
            ("aaaa bbbb cccc", 10, 2),
            ("    a", 3, 2),
            ("ab\tcd\n", 3, 3),
            ("界界界\n", 4, 3),
            ("e\u{301}e\u{301}e\u{301}\n", 2, 3),
            ("👩\u{200d}🚀👩\u{200d}🚀x\n", 4, 3),
        ] {
            let textarea = TextArea::new().with_text(text).with_soft_wrap(true);
            assert_eq!(textarea.visual_line_count(width), expected, "{text:?}");
            assert_eq!(
                textarea.visual_line_count_up_to(width, 2),
                expected.min(2),
                "{text:?}"
            );
            assert_eq!(textarea.text(), text);
        }

        let textarea = TextArea::new()
            .with_text("abcd\n")
            .with_line_numbers(true)
            .with_soft_wrap(true);
        assert_eq!(textarea.visual_line_count(5), 3, "three-cell gutter");
        assert_eq!(textarea.visual_line_count(0), 2, "no available width");
        let textarea = textarea.with_soft_wrap(false);
        assert_eq!(textarea.visual_line_count(5), 2, "hard lines only");
    }

    #[test]
    fn exact_width_caret_rows_preserve_graphemes_mouse_mapping_and_vertical_selection() {
        for (text, width) in [
            ("12345", 5),
            ("界界", 4),
            ("e\u{301}e\u{301}", 2),
            ("👩\u{200d}🚀👩\u{200d}🚀", 4),
            ("ab\tcd", 5),
        ] {
            let mut textarea = TextArea::new().with_soft_wrap(true).with_focus(true);
            textarea.insert_text(text);
            let end = textarea.cursor();
            let undo_groups = textarea.undo_group_count();
            assert_eq!(textarea.visual_line_count(width), 2, "{text:?}");
            assert_eq!(textarea.visual_line_count_up_to(width, 1), 1);
            assert_eq!(viewport_snapshot(&textarea, width, 2).1, Some((0, 1)));

            let click = |x, y| {
                Event::Mouse(MouseEvent::new(
                    MouseEventKind::Down(MouseButton::Left),
                    x,
                    y,
                ))
            };
            assert!(textarea.handle_event_at(&click(width - 1, 0), 0));
            assert!(textarea.cursor().grapheme < end.grapheme, "{text:?}");
            assert_eq!(
                viewport_snapshot(&textarea, width, 2).1.map(|(_, y)| y),
                Some(0)
            );
            assert!(textarea.handle_event_at(&click(width - 1, 1), 1000));
            assert_eq!(textarea.cursor(), end, "clicking the empty caret row");

            textarea.move_up();
            assert_eq!(textarea.cursor(), CursorPosition::new(0, 0, 0));
            assert_eq!(viewport_snapshot(&textarea, width, 2).1, Some((0, 0)));
            textarea.move_down();
            assert_eq!(textarea.cursor(), end);
            assert_eq!(viewport_snapshot(&textarea, width, 2).1, Some((0, 1)));
            textarea.select_up();
            assert_eq!(textarea.selected_text().as_deref(), Some(text));
            assert_eq!(textarea.text(), text);
            assert_eq!(textarea.undo_group_count(), undo_groups);
        }
    }

    #[test]
    fn hard_newline_reuses_the_caret_row_and_nonfinal_end_rows_are_transient() {
        let mut textarea = TextArea::new().with_soft_wrap(true).with_focus(true);
        textarea.insert_text("12345");
        assert_eq!(textarea.visual_line_count(5), 2);
        assert_eq!(viewport_snapshot(&textarea, 5, 2).1, Some((0, 1)));
        textarea.insert_newline();
        assert_eq!(textarea.text(), "12345\n");
        assert_eq!(textarea.visual_line_count(5), 2);
        let (rows, cursor) = viewport_snapshot(&textarea, 5, 2);
        assert_eq!(rows, ["12345", ""]);
        assert_eq!(cursor, Some((0, 1)));
        textarea.insert_text("x");
        assert_eq!(textarea.visual_line_count(5), 2);
        assert_eq!(viewport_snapshot(&textarea, 5, 2).1, Some((1, 1)));

        textarea.move_to_document_start();
        textarea.move_to_line_end();
        assert_eq!(textarea.visual_line_count(5), 3);
        let (rows, cursor) = viewport_snapshot(&textarea, 5, 3);
        assert_eq!(rows, ["12345", "", "x"]);
        assert_eq!(cursor, Some((0, 1)));
        textarea.move_down();
        assert_eq!(textarea.cursor(), CursorPosition::new(1, 0, 0));
        assert_eq!(textarea.visual_line_count(5), 2);
        let (rows, cursor) = viewport_snapshot(&textarea, 5, 2);
        assert_eq!(rows, ["12345", "x"]);
        assert_eq!(cursor, Some((0, 1)));
        textarea.move_up();
        textarea.move_to_line_end();
        assert_eq!(textarea.visual_line_count(5), 3);
        textarea.move_left();
        assert_eq!(textarea.visual_line_count(5), 2);
        assert_eq!(viewport_snapshot(&textarea, 5, 2).1, Some((4, 0)));
        assert_eq!(textarea.text(), "12345\nx");
    }

    #[test]
    fn exact_width_caret_reflows_on_resize_and_focus_without_changing_editor_state() {
        let mut textarea = TextArea::new().with_soft_wrap(true).with_focus(true);
        assert_eq!(textarea.rendered_width(), None);
        textarea.insert_text("1234567890");
        let end = textarea.cursor();
        let undo_groups = textarea.undo_group_count();
        for (width, height, cursor) in [
            (5, 3, (0, 2)),
            (6, 2, (4, 1)),
            (10, 2, (0, 1)),
            (11, 1, (10, 0)),
            (5, 3, (0, 2)),
        ] {
            assert_eq!(textarea.visual_line_count(width), usize::from(height));
            assert_eq!(viewport_snapshot(&textarea, width, height).1, Some(cursor));
            assert_eq!(textarea.rendered_width(), Some(width));
            assert_eq!(textarea.cursor(), end);
            assert_eq!(textarea.text(), "1234567890");
            assert_eq!(textarea.undo_group_count(), undo_groups);
        }
        textarea.set_focused(false);
        assert_eq!(textarea.visual_line_count(10), 1);
        assert_eq!(viewport_snapshot(&textarea, 10, 1).1, None);
        textarea.set_focused(true);
        assert_eq!(textarea.visual_line_count(10), 2);
        assert_eq!(viewport_snapshot(&textarea, 10, 2).1, Some((0, 1)));
        assert_eq!(textarea.cursor(), end);
        assert_eq!(textarea.undo_group_count(), undo_groups);
    }

    #[test]
    fn capped_caret_viewport_backfills_after_delete_and_undo_restores_pasted_text() {
        let mut textarea = TextArea::new().with_soft_wrap(true).with_focus(true);
        let original = "a\nb\nc\nd\n12345";
        textarea.handle_event_at(
            &Event::Paste(ftui_core::event::PasteEvent::bracketed(original)),
            0,
        );
        assert_eq!(textarea.visual_line_count(5), 6);
        assert_eq!(textarea.visual_line_count_up_to(5, 5), 5);
        let (rows, cursor) = viewport_snapshot(&textarea, 5, 5);
        assert_eq!(rows, ["b", "c", "d", "12345", ""]);
        assert_eq!(cursor, Some((0, 4)));
        textarea.delete_backward();
        assert_eq!(textarea.visual_line_count(5), 5);
        let (rows, cursor) = viewport_snapshot(&textarea, 5, 5);
        assert_eq!(rows, ["a", "b", "c", "d", "1234"]);
        assert_eq!(cursor, Some((4, 4)));
        textarea.undo();
        assert_eq!(textarea.text(), original);
        assert_eq!(textarea.visual_line_count(5), 6);
        assert_eq!(viewport_snapshot(&textarea, 5, 5).1, Some((0, 4)));
        textarea.redo();
        assert_eq!(textarea.text(), "a\nb\nc\nd\n1234");
        assert_eq!(textarea.visual_line_count(5), 5);
        assert_eq!(viewport_snapshot(&textarea, 5, 5).1, Some((4, 4)));
    }

    #[test]
    fn vertical_movement_stays_on_short_wrapped_rows_at_grapheme_boundaries() {
        for (text, start, expected) in [
            (
                "abc defgh",
                CursorPosition::new(0, 8, 8),
                CursorPosition::new(0, 3, 3),
            ),
            (
                "a界bcdef",
                CursorPosition::new(0, 6, 7),
                CursorPosition::new(0, 1, 1),
            ),
        ] {
            let mut textarea = TextArea::new()
                .with_text(text)
                .with_soft_wrap(true)
                .with_focus(true);
            let _ = viewport_snapshot(&textarea, 5, 3);
            textarea.set_cursor_position(start);
            assert_eq!(viewport_snapshot(&textarea, 5, 3).1, Some((4, 1)));
            textarea.move_up();
            assert_eq!(textarea.cursor(), expected, "{text:?}");
            assert_eq!(
                viewport_snapshot(&textarea, 5, 3).1.map(|(_, y)| y),
                Some(0)
            );
            textarea.move_down();
            assert_eq!(
                viewport_snapshot(&textarea, 5, 3).1.map(|(_, y)| y),
                Some(1)
            );
            textarea.set_cursor_position(start);
            textarea.select_up();
            assert_eq!(textarea.cursor(), expected);
            assert!(textarea.selected_text().is_some());
            assert_eq!(textarea.text(), text);
        }

        for (text, expected, drawn) in [
            ("abcdefgh ijklm", CursorPosition::new(0, 8, 8), (3, 1)),
            ("abcdea界bcdef", CursorPosition::new(0, 6, 6), (1, 1)),
        ] {
            let mut textarea = TextArea::new()
                .with_text(text)
                .with_soft_wrap(true)
                .with_focus(true);
            let _ = viewport_snapshot(&textarea, 5, 4);
            textarea.set_cursor_position(CursorPosition::new(0, 4, 4));
            textarea.move_down();
            assert_eq!(textarea.cursor(), expected, "{text:?}");
            assert_eq!(viewport_snapshot(&textarea, 5, 4).1, Some(drawn));
            textarea.set_cursor_position(CursorPosition::new(0, 4, 4));
            textarea.select_down();
            assert_eq!(textarea.cursor(), expected);
            assert!(textarea.selected_text().is_some());
            assert_eq!(textarea.text(), text);
        }
    }

    #[test]
    fn growing_after_newline_reveals_prior_text_before_another_keystroke() {
        for wrap in [false, true] {
            let mut textarea = TextArea::new().with_soft_wrap(wrap).with_focus(true);
            textarea.insert_text("Hello world");
            assert_eq!(
                viewport_snapshot(&textarea, 20, 1),
                (vec![String::from("Hello world")], Some((11, 0)))
            );
            textarea.insert_newline();
            assert_eq!(textarea.text(), "Hello world\n");
            assert_eq!(
                viewport_snapshot(&textarea, 20, 2),
                (
                    vec![String::from("Hello world"), String::new()],
                    Some((0, 1))
                ),
                "wrap={wrap}"
            );
            textarea.insert_newline();
            textarea.insert_text("asd");
            assert_eq!(
                viewport_snapshot(&textarea, 20, 3),
                (
                    vec![
                        String::from("Hello world"),
                        String::new(),
                        String::from("asd")
                    ],
                    Some((3, 2)),
                ),
                "wrap={wrap}"
            );
            assert_eq!(textarea.text(), "Hello world\n\nasd");
        }
    }

    #[test]
    fn capped_viewport_backfills_after_delete_and_preserves_undo_redo() {
        for wrap in [false, true] {
            let mut textarea = TextArea::new().with_soft_wrap(wrap).with_focus(true);
            textarea.insert_text("line0\nline1\nline2\nline3\nline4\n");
            let (rows, cursor) = viewport_snapshot(&textarea, 20, 5);
            assert_eq!(rows, ["line1", "line2", "line3", "line4", ""]);
            assert_eq!(cursor, Some((0, 4)));
            textarea.delete_backward();
            let (rows, cursor) = viewport_snapshot(&textarea, 20, 5);
            assert_eq!(rows, ["line0", "line1", "line2", "line3", "line4"]);
            assert_eq!(cursor, Some((5, 4)));
            textarea.undo();
            let (rows, cursor) = viewport_snapshot(&textarea, 20, 5);
            assert_eq!(rows, ["line1", "line2", "line3", "line4", ""]);
            assert_eq!(cursor, Some((0, 4)));
            textarea.redo();
            let (rows, cursor) = viewport_snapshot(&textarea, 20, 5);
            assert_eq!(rows, ["line0", "line1", "line2", "line3", "line4"]);
            assert_eq!(cursor, Some((5, 4)));
        }
    }

    #[test]
    fn viewport_resize_preserves_selection_cursor_and_undo_history() {
        for wrap in [false, true] {
            let mut textarea = TextArea::new().with_soft_wrap(wrap).with_focus(true);
            textarea.insert_text("line0\nline1\nline2\nline3\nline4\nline5");
            let _ = viewport_snapshot(&textarea, 20, 2);
            textarea.select_left();
            textarea.select_left();
            assert_eq!(textarea.selected_text().as_deref(), Some("e5"));
            let cursor = textarea.cursor();
            let selection = textarea.selection();
            let undo_groups = textarea.undo_group_count();
            let undo_ops = textarea.undo_op_count();
            for height in [5, 1, 6, 2] {
                let (rows, drawn_cursor) = viewport_snapshot(&textarea, 20, height);
                assert_eq!(rows[0], format!("line{}", 6 - height));
                assert_eq!(rows.last().map(String::as_str), Some("line5"));
                assert_eq!(drawn_cursor, Some((3, height - 1)));
                assert_eq!(textarea.cursor(), cursor);
                assert_eq!(textarea.selection(), selection);
                assert_eq!(textarea.undo_group_count(), undo_groups);
                assert_eq!(textarea.undo_op_count(), undo_ops);
                assert_eq!(textarea.redo_group_count(), 0);
            }
        }
    }

    #[test]
    fn width_reflow_clamps_wrapped_anchor_and_preserves_the_document() {
        let mut textarea = TextArea::new().with_soft_wrap(true).with_focus(true);
        textarea.insert_text("aaaa bbbb cccc dddd eeee");
        let (rows, cursor) = viewport_snapshot(&textarea, 10, 1);
        assert_eq!(rows, ["eeee"]);
        assert_eq!(cursor, Some((4, 0)));
        let before = textarea.cursor();
        let (rows, cursor) = viewport_snapshot(&textarea, 30, 1);
        assert_eq!(rows, ["aaaa bbbb cccc dddd eeee"]);
        assert_eq!(cursor, Some((24, 0)));
        assert_eq!(textarea.cursor(), before);
        let (rows, cursor) = viewport_snapshot(&textarea, 10, 3);
        assert_eq!(rows, ["aaaa bbbb", "cccc dddd", "eeee"]);
        assert_eq!(cursor, Some((4, 2)));
        assert_eq!(textarea.text(), "aaaa bbbb cccc dddd eeee");
    }

    #[test]
    fn full_middle_viewport_keeps_its_anchor_when_cursor_is_visible() {
        for wrap in [false, true] {
            let mut textarea = TextArea::new()
                .with_text("line0\nline1\nline2\nline3\nline4\nline5\nline6\nline7")
                .with_soft_wrap(wrap)
                .with_focus(true);
            textarea.set_cursor_position(CursorPosition::new(5, 2, 2));
            let (rows, cursor) = viewport_snapshot(&textarea, 20, 3);
            assert_eq!(rows, ["line3", "line4", "line5"]);
            assert_eq!(cursor, Some((2, 2)));
            let anchor = textarea.scroll_anchor.get();
            let (rows, cursor) = viewport_snapshot(&textarea, 20, 4);
            assert_eq!(rows, ["line3", "line4", "line5", "line6"]);
            assert_eq!(cursor, Some((2, 2)));
            assert_eq!(textarea.scroll_anchor.get(), anchor);
            textarea.move_up();
            let (_, cursor) = viewport_snapshot(&textarea, 20, 4);
            assert_eq!(cursor, Some((2, 1)));
            assert_eq!(textarea.scroll_anchor.get(), anchor);
        }
    }

    #[test]
    fn new_textarea_is_empty() {
        let ta = TextArea::new();
        assert!(ta.is_empty());
        assert_eq!(ta.text(), "");
        assert_eq!(ta.line_count(), 1); // empty rope has 1 line
    }

    #[test]
    fn accessibility_preserves_multiline_content_and_focus() {
        use ftui_a11y::Accessible;
        use ftui_a11y::node::A11yRole;

        let area = Rect::new(3, 4, 20, 2);
        let mut textarea = TextArea::new().with_text("first é\nsecond 日");
        let initial = textarea.accessibility_nodes(area);
        assert_eq!(initial.len(), 1);
        assert_eq!(initial[0].role, A11yRole::TextInput);
        assert_eq!(initial[0].name.as_deref(), Some("first é\nsecond 日"));
        assert!(!initial[0].state.focused);
        textarea.set_focused(true);
        let focused = textarea.accessibility_nodes(area);
        assert!(focused[0].state.focused);
        assert_eq!(focused[0].id, initial[0].id);
        assert_eq!(focused[0].bounds, area);
        assert!(
            textarea
                .accessibility_nodes(Rect::new(0, 0, 0, 2))
                .is_empty()
        );
        let empty = TextArea::new().with_placeholder("Write a message");
        assert_eq!(
            empty.accessibility_nodes(area)[0].name.as_deref(),
            Some("Write a message")
        );
    }

    #[test]
    fn render_collects_textarea_accessibility_only_when_enabled() {
        use ftui_a11y::tree::A11yTreeBuilder;
        use ftui_render::grapheme_pool::GraphemePool;

        let textarea = TextArea::new()
            .with_text("line one\nline two")
            .with_focus(true);
        let area = Rect::new(2, 1, 12, 2);
        let mut pool = GraphemePool::new();
        let mut frame = Frame::new(20, 5, &mut pool);
        Widget::render(&textarea, area, &mut frame);
        assert!(frame.a11y_order().is_empty());

        let mut builder = A11yTreeBuilder::new();
        let mut frame = Frame::new(20, 5, &mut pool);
        frame.set_a11y(&mut builder);
        Widget::render(&textarea, area, &mut frame);
        frame.finish_a11y();
        drop(frame);
        let tree = builder.build();
        assert_eq!(tree.nodes().count(), 1);
        assert_eq!(
            tree.focused().and_then(|node| node.name.as_deref()),
            Some("line one\nline two")
        );
    }

    #[test]
    fn with_text_builder() {
        let ta = TextArea::new().with_text("hello\nworld");
        assert_eq!(ta.text(), "hello\nworld");
        assert_eq!(ta.line_count(), 2);
    }

    #[test]
    fn insert_text_and_newline() {
        let mut ta = TextArea::new();
        ta.insert_text("hello");
        ta.insert_newline();
        ta.insert_text("world");
        assert_eq!(ta.text(), "hello\nworld");
        assert_eq!(ta.line_count(), 2);
    }

    #[test]
    fn delete_backward_works() {
        let mut ta = TextArea::new().with_text("hello");
        ta.move_to_document_end();
        ta.delete_backward();
        assert_eq!(ta.text(), "hell");
    }

    #[test]
    fn cursor_movement() {
        let mut ta = TextArea::new().with_text("abc\ndef\nghi");
        ta.move_to_document_start();
        assert_eq!(ta.cursor().line, 0);
        assert_eq!(ta.cursor().grapheme, 0);

        ta.move_down();
        assert_eq!(ta.cursor().line, 1);

        ta.move_to_line_end();
        assert_eq!(ta.cursor().grapheme, 3);

        ta.move_to_document_end();
        assert_eq!(ta.cursor().line, 2);
    }

    #[test]
    fn undo_redo() {
        let mut ta = TextArea::new();
        ta.insert_text("abc");
        assert_eq!(ta.text(), "abc");
        ta.undo();
        assert_eq!(ta.text(), "");
        ta.redo();
        assert_eq!(ta.text(), "abc");
    }

    #[test]
    fn selection_and_delete() {
        let mut ta = TextArea::new().with_text("hello world");
        ta.move_to_document_start();
        for _ in 0..5 {
            ta.select_right();
        }
        assert_eq!(ta.selected_text(), Some("hello".to_string()));
        ta.delete_backward();
        assert_eq!(ta.text(), " world");
    }

    #[test]
    fn select_all() {
        let mut ta = TextArea::new().with_text("abc\ndef");
        ta.select_all();
        assert_eq!(ta.selected_text(), Some("abc\ndef".to_string()));
    }

    #[test]
    fn set_text_resets() {
        let mut ta = TextArea::new().with_text("old");
        ta.insert_text(" stuff");
        ta.set_text("new");
        assert_eq!(ta.text(), "new");
    }

    #[test]
    fn replacement_generation_tracks_drafts_without_invalidating_edits() {
        let mut ta = TextArea::new().with_text("界e\u{301}\nsecond");
        let draft = ta.replacement_generation();
        for code in [KeyCode::Char('x'), KeyCode::Enter, KeyCode::Backspace] {
            ta.handle_event(&Event::Key(KeyEvent {
                code,
                modifiers: Modifiers::NONE,
                kind: KeyEventKind::Press,
            }));
            assert_eq!(ta.replacement_generation(), draft);
        }
        ta.handle_event(&Event::Paste(ftui_core::event::PasteEvent::bracketed(
            "pasted\ntext",
        )));
        ta.move_to_document_end();
        ta.select_left();
        ta.delete_backward();
        ta.undo();
        ta.redo();
        ta.insert_newline();
        ta.insert_char('界');
        ta.insert_text(" tail");
        ta.set_cursor_position(CursorPosition::new(0, 0, 0));
        ta.select_all();
        ta.clear_selection();
        ta.set_focused(false);
        ta.reset_mouse_interaction();
        assert_eq!(ta.replacement_generation(), draft);

        // Restoring the same bytes is a new draft, even though comparing
        // content or editor length would miss the ownership change.
        let text = ta.text();
        ta.set_text(&text);
        assert_ne!(ta.replacement_generation(), draft);
        assert_eq!(ta.text(), text);
        let replaced = ta.replacement_generation();
        ta = ta.with_text(&text);
        assert_ne!(ta.replacement_generation(), replaced);
        assert_eq!(ta.text(), text);

        // An empty prompt can also be replaced repeatedly between owners.
        ta.set_text("");
        let empty = ta.replacement_generation();
        ta.set_text("");
        assert_ne!(ta.replacement_generation(), empty);
        assert!(ta.is_empty());
    }

    #[test]
    fn scroll_follows_cursor() {
        let mut ta = TextArea::new();
        // Insert many lines
        for i in 0..50 {
            ta.insert_text(&format!("line {}\n", i));
        }
        // Cursor should be at the bottom, scroll anchor adjusted.
        assert!(ta.scroll_anchor.get().0 > 0);
        assert!(ta.cursor().line >= 49);

        // Move to top
        ta.move_to_document_start();
        assert_eq!(ta.scroll_anchor.get().0, 0);
    }

    #[test]
    fn gutter_width_without_line_numbers() {
        let ta = TextArea::new();
        assert_eq!(ta.gutter_width(), 0);
    }

    #[test]
    fn gutter_width_with_line_numbers() {
        let mut ta = TextArea::new().with_line_numbers(true);
        ta.insert_text("a\nb\nc");
        assert_eq!(ta.gutter_width(), 3); // 1 digit + space + separator
    }

    #[test]
    fn gutter_width_many_lines() {
        let mut ta = TextArea::new().with_line_numbers(true);
        for i in 0..100 {
            ta.insert_text(&format!("line {}\n", i));
        }
        assert_eq!(ta.gutter_width(), 5); // 3 digits + space + separator
    }

    #[test]
    fn focus_state() {
        let mut ta = TextArea::new();
        assert!(!ta.is_focused());
        ta.set_focused(true);
        assert!(ta.is_focused());
    }

    #[test]
    fn word_movement() {
        let mut ta = TextArea::new().with_text("hello world foo");
        ta.move_to_document_start();
        ta.move_word_right();
        assert_eq!(ta.cursor().grapheme, 6);
        ta.move_word_left();
        assert_eq!(ta.cursor().grapheme, 0);
    }

    #[test]
    fn page_up_down() {
        let mut ta = TextArea::new();
        for i in 0..50 {
            ta.insert_text(&format!("line {}\n", i));
        }
        ta.move_to_document_start();
        let state = TextAreaState {
            last_viewport_height: 10,
            last_viewport_width: 80,
        };
        ta.page_down(&state);
        assert!(ta.cursor().line >= 10);
        ta.page_up(&state);
        assert_eq!(ta.cursor().line, 0);
    }

    #[test]
    fn insert_replaces_selection() {
        let mut ta = TextArea::new().with_text("hello world");
        ta.move_to_document_start();
        for _ in 0..5 {
            ta.select_right();
        }
        ta.insert_text("goodbye");
        assert_eq!(ta.text(), "goodbye world");
    }

    #[test]
    fn insert_single_char() {
        let mut ta = TextArea::new();
        ta.insert_char('X');
        assert_eq!(ta.text(), "X");
        assert_eq!(ta.cursor().grapheme, 1);
    }

    #[test]
    fn insert_multiline_text() {
        let mut ta = TextArea::new();
        ta.insert_text("line1\nline2\nline3");
        assert_eq!(ta.line_count(), 3);
        assert_eq!(ta.cursor().line, 2);
    }

    #[test]
    fn delete_forward_works() {
        let mut ta = TextArea::new().with_text("hello");
        ta.move_to_document_start();
        ta.delete_forward();
        assert_eq!(ta.text(), "ello");
    }

    #[test]
    fn delete_backward_at_line_start_joins_lines() {
        let mut ta = TextArea::new().with_text("abc\ndef");
        // Move to start of line 2
        ta.move_to_document_start();
        ta.move_down();
        ta.move_to_line_start();
        ta.delete_backward();
        assert_eq!(ta.text(), "abcdef");
        assert_eq!(ta.line_count(), 1);
    }

    #[test]
    fn cursor_horizontal_movement() {
        let mut ta = TextArea::new().with_text("abc");
        ta.move_to_document_start();
        ta.move_right();
        assert_eq!(ta.cursor().grapheme, 1);
        ta.move_right();
        assert_eq!(ta.cursor().grapheme, 2);
        ta.move_left();
        assert_eq!(ta.cursor().grapheme, 1);
    }

    #[test]
    fn cursor_vertical_maintains_column() {
        let mut ta = TextArea::new().with_text("abcde\nfg\nhijkl");
        ta.move_to_document_start();
        ta.move_to_line_end(); // col 5
        ta.move_down(); // line 1 only has 2 chars, should clamp
        assert_eq!(ta.cursor().line, 1);
        ta.move_down(); // line 2 has 5 chars, should restore col
        assert_eq!(ta.cursor().line, 2);
    }

    #[test]
    fn selection_shift_arrow() {
        let mut ta = TextArea::new().with_text("abcdef");
        ta.move_to_document_start();
        ta.select_right();
        ta.select_right();
        ta.select_right();
        assert_eq!(ta.selected_text(), Some("abc".to_string()));
    }

    #[test]
    fn selection_extends_up_down() {
        let mut ta = TextArea::new().with_text("line1\nline2\nline3");
        ta.move_to_document_start();
        ta.select_down();
        let sel = ta.selected_text().unwrap();
        assert!(sel.contains('\n'));
    }

    #[test]
    fn undo_chain() {
        let mut ta = TextArea::new();
        ta.insert_text("a");
        ta.insert_text("b");
        ta.insert_text("c");
        assert_eq!(ta.text(), "abc");
        ta.undo();
        ta.undo();
        ta.undo();
        assert_eq!(ta.text(), "");
    }

    #[test]
    fn redo_discarded_on_new_edit() {
        let mut ta = TextArea::new();
        ta.insert_text("abc");
        ta.undo();
        ta.insert_text("xyz");
        ta.redo(); // should be no-op
        assert_eq!(ta.text(), "xyz");
    }

    #[test]
    fn clear_selection() {
        let mut ta = TextArea::new().with_text("hello");
        ta.select_all();
        assert!(ta.selection().is_some());
        ta.clear_selection();
        assert!(ta.selection().is_none());
    }

    #[test]
    fn delete_word_backward() {
        let mut ta = TextArea::new().with_text("hello world");
        ta.move_to_document_end();
        ta.delete_word_backward();
        assert_eq!(ta.text(), "hello ");
    }

    #[test]
    fn delete_to_end_of_line() {
        let mut ta = TextArea::new().with_text("hello world");
        ta.move_to_document_start();
        ta.move_right(); // after 'h'
        ta.delete_to_end_of_line();
        assert_eq!(ta.text(), "h");
    }

    #[test]
    fn placeholder_builder() {
        let ta = TextArea::new().with_placeholder("Enter text...");
        assert!(ta.is_empty());
        assert_eq!(ta.placeholder, "Enter text...");
    }

    #[test]
    fn soft_wrap_builder() {
        let ta = TextArea::new().with_soft_wrap(true);
        assert!(ta.soft_wrap);
    }

    #[test]
    fn soft_wrap_renders_wrapped_lines() {
        use ftui::Widget;
        use ftui_render::grapheme_pool::GraphemePool;

        let ta = TextArea::new().with_soft_wrap(true).with_text("abcdef");
        let area = Rect::new(0, 0, 3, 2);
        let mut pool = GraphemePool::new();
        let mut frame = Frame::new(3, 2, &mut pool);
        Widget::render(&ta, area, &mut frame);

        assert_eq!(frame.buffer.get(0, 0).unwrap().content.as_char(), Some('a'));
        assert_eq!(frame.buffer.get(2, 0).unwrap().content.as_char(), Some('c'));
        assert_eq!(frame.buffer.get(0, 1).unwrap().content.as_char(), Some('d'));
        assert_eq!(frame.buffer.get(2, 1).unwrap().content.as_char(), Some('f'));
    }

    #[test]
    fn max_height_builder() {
        let ta = TextArea::new().with_max_height(10);
        assert_eq!(ta.max_height, 10);
    }

    #[test]
    fn editor_access() {
        let mut ta = TextArea::new().with_text("test");
        assert_eq!(ta.editor().text(), "test");
        ta.editor_mut().insert_char('!');
        assert!(ta.text().contains('!'));
    }

    #[test]
    fn move_to_line_start_and_end() {
        let mut ta = TextArea::new().with_text("hello world");
        ta.move_to_document_start();
        ta.move_to_line_end();
        assert_eq!(ta.cursor().grapheme, 11);
        ta.move_to_line_start();
        assert_eq!(ta.cursor().grapheme, 0);
    }

    #[test]
    fn render_empty_with_placeholder() {
        use ftui_render::grapheme_pool::GraphemePool;
        let ta = TextArea::new()
            .with_placeholder("Type here")
            .with_focus(true);
        let mut pool = GraphemePool::new();
        let mut frame = Frame::new(20, 5, &mut pool);
        let area = Rect::new(0, 0, 20, 5);
        Widget::render(&ta, area, &mut frame);
        // Placeholder should be rendered
        let cell = frame.buffer.get(0, 0).unwrap();
        assert_eq!(cell.content.as_char(), Some('T'));
        // Cursor should be set
        assert!(frame.cursor_position.is_some());
    }

    #[test]
    fn render_with_content() {
        use ftui_render::grapheme_pool::GraphemePool;
        let ta = TextArea::new().with_text("abc\ndef").with_focus(true);
        let mut pool = GraphemePool::new();
        let mut frame = Frame::new(20, 5, &mut pool);
        let area = Rect::new(0, 0, 20, 5);
        Widget::render(&ta, area, &mut frame);
        let cell = frame.buffer.get(0, 0).unwrap();
        assert_eq!(cell.content.as_char(), Some('a'));
    }

    #[test]
    fn render_shorter_text_clears_stale_suffix_and_extra_lines() {
        use ftui_render::grapheme_pool::GraphemePool;

        let area = Rect::new(0, 0, 8, 3);
        let mut pool = GraphemePool::new();
        let mut frame = Frame::new(8, 3, &mut pool);

        Widget::render(
            &TextArea::new().with_text("abcdef\nghijkl"),
            area,
            &mut frame,
        );
        Widget::render(&TextArea::new().with_text("hi"), area, &mut frame);

        assert_eq!(raw_row_text(&frame, 0, 8), "hi      ");
        assert_eq!(raw_row_text(&frame, 1, 8), "        ");
        assert_eq!(raw_row_text(&frame, 2, 8), "        ");
    }

    #[test]
    fn render_line_numbers_without_styling() {
        use ftui_render::budget::DegradationLevel;
        use ftui_render::grapheme_pool::GraphemePool;

        let ta = TextArea::new().with_text("a\nb").with_line_numbers(true);
        let mut pool = GraphemePool::new();
        let mut frame = Frame::new(8, 2, &mut pool);
        frame.set_degradation(DegradationLevel::NoStyling);

        Widget::render(&ta, Rect::new(0, 0, 8, 2), &mut frame);

        let cell = frame.buffer.get(0, 0).unwrap();
        assert_eq!(cell.content.as_char(), Some('1'));
    }

    #[test]
    fn stateful_render_updates_viewport_state() {
        use ftui_render::grapheme_pool::GraphemePool;

        let ta = TextArea::new();
        let mut state = TextAreaState::default();
        let mut pool = GraphemePool::new();
        let mut frame = Frame::new(10, 3, &mut pool);
        let area = Rect::new(0, 0, 10, 3);

        StatefulWidget::render(&ta, area, &mut frame, &mut state);

        assert_eq!(state.last_viewport_height, 3);
        assert_eq!(state.last_viewport_width, 10);
    }

    #[test]
    fn render_zero_area_no_panic() {
        let ta = TextArea::new().with_text("test");
        use ftui_render::grapheme_pool::GraphemePool;
        let mut pool = GraphemePool::new();
        let mut frame = Frame::new(10, 10, &mut pool);
        Widget::render(&ta, Rect::new(0, 0, 0, 0), &mut frame);
    }

    #[test]
    fn is_essential() {
        let ta = TextArea::new();
        assert!(Widget::is_essential(&ta));
    }

    #[test]
    fn default_impl() {
        let ta = TextArea::default();
        assert!(ta.is_empty());
    }

    #[test]
    fn insert_newline_splits_line() {
        let mut ta = TextArea::new().with_text("abcdef");
        ta.move_to_document_start();
        ta.move_right();
        ta.move_right();
        ta.move_right();
        ta.insert_newline();
        assert_eq!(ta.line_count(), 2);
        assert_eq!(ta.cursor().line, 1);
    }

    #[test]
    fn command_chords_do_not_type_their_letter() {
        // macOS sends Cmd as SUPER. Cmd+V is a paste shortcut handled by the
        // host; the buffer must not also receive a literal "v". Same for the
        // other common Cmd chords a user will hit over a focused editor.
        for (label, ch) in [("paste", 'v'), ("copy", 'c'), ("cut", 'x'), ("all", 'a')] {
            let mut ta = TextArea::new();
            let handled = ta.handle_event(&Event::Key(
                KeyEvent::new(KeyCode::Char(ch)).with_modifiers(Modifiers::SUPER),
            ));
            assert_eq!(ta.text(), "", "Cmd+{ch} ({label}) typed into the buffer");
            assert!(!handled, "Cmd+{ch} ({label}) should be left to the host");
        }
    }

    #[test]
    fn key_release_does_not_type() {
        // Synthetic drivers (the demo's guided tour) send a press followed by a
        // release so key-latching screens unlatch. The release must be inert
        // here, or every driven keystroke would be typed twice.
        let mut ta = TextArea::new();
        let handled = ta.handle_event(&Event::Key(
            KeyEvent::new(KeyCode::Char('x')).with_kind(KeyEventKind::Release),
        ));
        assert!(!handled, "a key release should not be consumed as typing");
        assert_eq!(ta.text(), "");
    }

    #[test]
    fn plain_and_shifted_characters_still_type() {
        let mut ta = TextArea::new();
        assert!(ta.handle_event(&Event::Key(KeyEvent::new(KeyCode::Char('v')))));
        assert!(ta.handle_event(&Event::Key(
            KeyEvent::new(KeyCode::Char('V')).with_modifiers(Modifiers::SHIFT),
        )));
        assert_eq!(ta.text(), "vV");
    }

    #[test]
    fn paste_event_inserts_clipboard_text() {
        let mut ta = TextArea::new().with_text("ab");
        ta.move_to_document_end();
        assert!(ta.handle_event(&Event::Paste(ftui_core::event::PasteEvent::bracketed("cd"))));
        assert_eq!(ta.text(), "abcd");
    }

    #[test]
    fn unicode_grapheme_cluster() {
        let mut ta = TextArea::new();
        ta.insert_text("café");
        // 'é' is a single grapheme even if composed
        assert_eq!(ta.text(), "café");
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn insert_delete_inverse(text in "[a-zA-Z0-9 ]{1,50}") {
                let mut ta = TextArea::new();
                ta.insert_text(&text);
                // Delete all characters backwards
                for _ in 0..text.len() {
                    ta.delete_backward();
                }
                prop_assert!(ta.is_empty() || ta.text().is_empty());
            }

            #[test]
            fn undo_redo_inverse(text in "[a-zA-Z0-9]{1,30}") {
                let mut ta = TextArea::new();
                ta.insert_text(&text);
                let after_insert = ta.text();
                ta.undo();
                ta.redo();
                prop_assert_eq!(ta.text(), after_insert);
            }

            #[test]
            fn cursor_always_valid(ops in proptest::collection::vec(0u8..10, 1..20)) {
                let mut ta = TextArea::new().with_text("abc\ndef\nghi\njkl");
                for op in ops {
                    match op {
                        0 => ta.move_left(),
                        1 => ta.move_right(),
                        2 => ta.move_up(),
                        3 => ta.move_down(),
                        4 => ta.move_to_line_start(),
                        5 => ta.move_to_line_end(),
                        6 => ta.move_to_document_start(),
                        7 => ta.move_to_document_end(),
                        8 => ta.move_word_left(),
                        _ => ta.move_word_right(),
                    }
                    let cursor = ta.cursor();
                    prop_assert!(cursor.line < ta.line_count(),
                        "cursor line {} >= line_count {}", cursor.line, ta.line_count());
                }
            }

            #[test]
            fn selection_ordered(n in 1usize..20) {
                let mut ta = TextArea::new().with_text("hello world foo bar");
                ta.move_to_document_start();
                for _ in 0..n {
                    ta.select_right();
                }
                if let Some(sel) = ta.selection() {
                    // When selecting right from start, anchor should be at/before head
                    prop_assert!(sel.anchor.line <= sel.head.line
                        || (sel.anchor.line == sel.head.line
                            && sel.anchor.grapheme <= sel.head.grapheme));
                }
            }
        }
    }

    mod highlighter_tests {
        use super::*;
        use ftui_render::cell::{PackedRgba, StyleFlags};
        use ftui_render::frame::Frame;
        use ftui_render::grapheme_pool::GraphemePool;
        use proptest::prelude::*;

        fn render(ta: &TextArea, w: u16, h: u16, mut f: impl FnMut(&Frame)) {
            let mut pool = GraphemePool::new();
            let mut frame = Frame::new(w, h, &mut pool);
            Widget::render(ta, Rect::new(0, 0, w, h), &mut frame);
            f(&frame);
        }

        #[test]
        fn highlighter_receives_each_visible_line_once() {
            let seen = Arc::new(std::sync::Mutex::new(Vec::<usize>::new()));
            let recorder = seen.clone();
            let ta = TextArea::new()
                .with_text("one\ntwo\nthree")
                .with_highlighter(Arc::new(move |line, _text| {
                    recorder.lock().unwrap().push(line); // ubs:ignore test fixture: a poisoned capture mutex must fail the assertion.
                    Vec::new()
                }));
            assert!(ta.has_highlighter());
            render(&ta, 20, 3, |_| {});
            let mut lines = seen.lock().unwrap().clone(); // ubs:ignore test fixture: a poisoned capture mutex must fail the assertion.
            lines.sort_unstable();
            assert_eq!(lines, vec![0, 1, 2]);
        }

        #[test]
        fn highlighter_ranges_style_graphemes() {
            let ta = TextArea::new()
                .with_text("abcdef")
                .with_highlighter(Arc::new(|_l, _t| {
                    vec![(0..3, Style::new().fg(PackedRgba::RED))]
                }));
            render(&ta, 10, 1, |frame| {
                for x in 0..3 {
                    assert_eq!(
                        frame.buffer.get(x, 0).unwrap().fg,
                        PackedRgba::RED,
                        "col {x}"
                    );
                }
                assert_ne!(frame.buffer.get(3, 0).unwrap().fg, PackedRgba::RED);
            });
        }

        #[test]
        fn out_of_range_and_non_boundary_ranges_ignored() {
            // "café": c(0) a(1) f(2) é(3..5); len 5. All three ranges are
            // invalid (past end / splits 'é' / inverted) and are dropped.
            let ta = TextArea::new()
                .with_text("café")
                .with_highlighter(Arc::new(|_l, _t| {
                    // Struct literal so clippy does not flag the reversed range.
                    let inverted = Range { start: 5, end: 2 };
                    vec![
                        (0..100, Style::new().fg(PackedRgba::RED)),
                        (3..4, Style::new().fg(PackedRgba::GREEN)),
                        (inverted, Style::new().fg(PackedRgba::BLUE)),
                    ]
                }));
            assert!(ta.line_spans(0, "café", true).is_empty());
            render(&ta, 10, 1, |_| {}); // no panic
        }

        #[test]
        fn multibyte_ranges_align_to_graphemes() {
            // Highlight only the 2-byte 'é' (bytes 3..5).
            let ta = TextArea::new()
                .with_text("café")
                .with_highlighter(Arc::new(|_l, _t| {
                    vec![(3..5, Style::new().fg(PackedRgba::RED))]
                }));
            assert_eq!(ta.line_spans(0, "café", true).len(), 1);
            render(&ta, 10, 1, |frame| {
                assert_eq!(frame.buffer.get(3, 0).unwrap().fg, PackedRgba::RED, "'é'");
                assert_ne!(frame.buffer.get(0, 0).unwrap().fg, PackedRgba::RED, "'c'");
            });
        }

        #[test]
        fn selection_style_merges_over_highlight() {
            let mut ta = TextArea::new()
                .with_text("abcdef")
                .with_focus(true)
                .with_selection_style(Style::new().bg(PackedRgba::GREEN))
                .with_highlighter(Arc::new(|_l, _t| {
                    vec![(0..6, Style::new().bg(PackedRgba::RED).fg(PackedRgba::WHITE))]
                }));
            ta.move_to_document_start();
            ta.select_right();
            ta.select_right();
            ta.select_right();
            render(&ta, 10, 1, |frame| {
                let selected = frame.buffer.get(0, 0).unwrap();
                assert_eq!(
                    selected.bg,
                    PackedRgba::GREEN,
                    "selection bg wins over highlight"
                );
                assert_eq!(selected.fg, PackedRgba::WHITE, "highlight fg preserved");
                let highlighted = frame.buffer.get(3, 0).unwrap();
                assert_eq!(
                    highlighted.bg,
                    PackedRgba::RED,
                    "unselected cell keeps highlight bg"
                );
            });
        }

        #[test]
        fn soft_wrap_applies_highlight_on_continuation_rows() {
            // 10 chars wrapped at width 5 -> row 0 "abcde", row 1 "fghij"; the
            // whole-line highlight must reach the continuation row.
            let ta = TextArea::new()
                .with_text("abcdefghij")
                .with_soft_wrap(true)
                .with_highlighter(Arc::new(|_l, _t| {
                    vec![(0..10, Style::new().fg(PackedRgba::RED))]
                }));
            render(&ta, 5, 2, |frame| {
                assert_eq!(
                    frame.buffer.get(0, 0).unwrap().fg,
                    PackedRgba::RED,
                    "first row"
                );
                assert_eq!(
                    frame.buffer.get(0, 1).unwrap().fg,
                    PackedRgba::RED,
                    "continuation row highlighted"
                );
            });
        }

        #[test]
        fn degraded_frame_ignores_highlight() {
            use ftui_render::budget::DegradationLevel;

            let ta = TextArea::new()
                .with_text("abcdef")
                .with_highlighter(Arc::new(|_l, _t| {
                    vec![(0..3, Style::new().fg(PackedRgba::RED))]
                }));
            let mut pool = GraphemePool::new();
            let mut frame = Frame::new(10, 1, &mut pool);
            frame.buffer.degradation = DegradationLevel::NoStyling;
            Widget::render(&ta, Rect::new(0, 0, 10, 1), &mut frame);
            for x in 0..6 {
                assert_ne!(
                    frame.buffer.get(x, 0).unwrap().fg,
                    PackedRgba::RED,
                    "cell at x={x} must not have highlight fg under NoStyling"
                );
            }
        }

        #[test]
        fn style_at_last_range_wins() {
            let style_a = Style::new().fg(PackedRgba::RED);
            let style_b = Style::new().fg(PackedRgba::BLUE);
            let spans = vec![(0..5, style_a), (2..4, style_b)];
            assert_eq!(style_at(&spans, 0), Some(style_a));
            assert_eq!(style_at(&spans, 1), Some(style_a));
            assert_eq!(style_at(&spans, 2), Some(style_b));
            assert_eq!(style_at(&spans, 3), Some(style_b));
            assert_eq!(style_at(&spans, 4), Some(style_a));
            assert_eq!(style_at(&spans, 5), None);
        }

        #[test]
        fn cjk_and_emoji_multibyte_ranges_align_to_graphemes() {
            // "日本語 ok 🎉"
            // 日本 is bytes 0..6 (columns 0 and 2; each width 2)
            // 🎉 is bytes 13..17 (column 10; width 2)
            let text = "日本語 ok 🎉";
            let ta = TextArea::new()
                .with_text(text)
                .with_highlighter(Arc::new(|_l, _t| {
                    vec![
                        (0..6, Style::new().fg(PackedRgba::RED)),
                        (13..17, Style::new().fg(PackedRgba::BLUE)),
                    ]
                }));
            render(&ta, 20, 1, |frame| {
                // Both wide graphemes in 日本 received RED
                assert_eq!(
                    frame.buffer.get(0, 0).unwrap().fg,
                    PackedRgba::RED,
                    "日 at col 0"
                );
                assert_eq!(
                    frame.buffer.get(2, 0).unwrap().fg,
                    PackedRgba::RED,
                    "本 at col 2"
                );

                // Unstyled graphemes 語 (col 4), ' ' (col 6), 'o' (col 7), 'k' (col 8), ' ' (col 9)
                for col in [4, 6, 7, 8, 9] {
                    assert_ne!(
                        frame.buffer.get(col, 0).unwrap().fg,
                        PackedRgba::RED,
                        "col {col} must not be red"
                    );
                    assert_ne!(
                        frame.buffer.get(col, 0).unwrap().fg,
                        PackedRgba::BLUE,
                        "col {col} must not be blue"
                    );
                }

                // Emoji 🎉 covering bytes 13..17 styles the whole emoji at col 10
                assert_eq!(
                    frame.buffer.get(10, 0).unwrap().fg,
                    PackedRgba::BLUE,
                    "col 10 of 🎉 must be blue"
                );
            });
        }

        fn toy_rust_highlighter(_line_idx: usize, line: &str) -> Vec<(Range<usize>, Style)> {
            let mut spans = Vec::new();
            let keyword_style = Style::new().bold();
            let string_style = Style::new().fg(PackedRgba::GREEN);
            let comment_style = Style::new().dim();

            let code_part = if let Some(comment_pos) = line.find("//") {
                spans.push((comment_pos..line.len(), comment_style));
                &line[..comment_pos]
            } else {
                line
            };

            let mut in_quote: Option<usize> = None;
            for (idx, ch) in code_part.char_indices() {
                if ch == '"' {
                    if let Some(start) = in_quote {
                        spans.push((start..idx + 1, string_style));
                        in_quote = None;
                    } else {
                        in_quote = Some(idx);
                    }
                }
            }

            for kw in ["fn", "let"] {
                let mut search_from = 0;
                while let Some(pos) = code_part[search_from..].find(kw) {
                    let abs_pos = search_from + pos;
                    let end_pos = abs_pos + kw.len();
                    let prev_ok = abs_pos == 0
                        || !code_part[..abs_pos]
                            .chars()
                            .next_back()
                            .is_some_and(|c| c.is_alphanumeric() || c == '_');
                    let next_ok = end_pos == code_part.len()
                        || !code_part[end_pos..]
                            .chars()
                            .next()
                            .is_some_and(|c| c.is_alphanumeric() || c == '_');
                    let in_str = spans
                        .iter()
                        .any(|(r, s)| *s == string_style && r.contains(&abs_pos));
                    if prev_ok && next_ok && !in_str {
                        spans.push((abs_pos..end_pos, keyword_style));
                    }
                    search_from = end_pos;
                }
            }

            spans.sort_by_key(|(r, _)| r.start);
            spans
        }

        fn style_row_text(frame: &Frame, y: u16, width: u16) -> String {
            (0..width)
                .map(|x| {
                    let cell = frame.buffer.get(x, y).unwrap();
                    if cell.attrs.has_flag(StyleFlags::BOLD) {
                        'B'
                    } else if cell.fg == PackedRgba::GREEN {
                        'S'
                    } else if cell.attrs.has_flag(StyleFlags::DIM) {
                        'C'
                    } else {
                        '.'
                    }
                })
                .collect()
        }

        #[test]
        fn textarea_highlighted_rust_60x8() {
            let snippet =
                "fn main() {\n    let name = \"ftui\"; // greet\n    println!(\"hi {name}\");\n}";
            let ta = TextArea::new()
                .with_text(snippet)
                .with_highlighter(Arc::new(toy_rust_highlighter));

            render(&ta, 60, 8, |frame| {
                // Verify text content of lines
                assert_eq!(raw_row_text(frame, 0, 60), format!("{:<60}", "fn main() {"));
                assert_eq!(
                    raw_row_text(frame, 1, 60),
                    format!("{:<60}", "    let name = \"ftui\"; // greet")
                );
                assert_eq!(
                    raw_row_text(frame, 2, 60),
                    format!("{:<60}", "    println!(\"hi {name}\");")
                );
                assert_eq!(raw_row_text(frame, 3, 60), format!("{:<60}", "}"));
                for y in 4..8 {
                    assert_eq!(raw_row_text(frame, y, 60), " ".repeat(60));
                }

                // Verify style masks
                // Row 0: fn (bold) main() {
                assert_eq!(
                    style_row_text(frame, 0, 60),
                    format!("BB{}", ".".repeat(58))
                );
                // Row 1: let (bold), "ftui" (string/green), // greet (dim/comment)
                assert_eq!(
                    style_row_text(frame, 1, 60),
                    format!("....BBB........SSSSSS..CCCCCCCC{}", ".".repeat(29))
                );
                // Row 2: "hi {name}" (string/green)
                assert_eq!(
                    style_row_text(frame, 2, 60),
                    format!(".............SSSSSSSSSSS..{}", ".".repeat(34))
                );
                // Row 3: }
                assert_eq!(style_row_text(frame, 3, 60), ".".repeat(60));
                // Rows 4..8: empty
                for y in 4..8 {
                    assert_eq!(style_row_text(frame, y, 60), ".".repeat(60));
                }
            });
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(512))]

            /// `style_at` returns the last covering range's style and never
            /// panics for any byte offset.
            #[test]
            fn style_at_returns_last_covering_range(
                bounds in prop::collection::vec((0usize..20, 0usize..20), 0..6),
                byte in 0usize..25,
            ) {
                let spans: Vec<(Range<usize>, Style)> = bounds
                    .iter()
                    .filter(|(a, b)| a < b)
                    .enumerate()
                    .map(|(i, (a, b))| (*a..*b, Style::new().fg(PackedRgba::rgb(u8::try_from((i * 30) % 256).expect("test color"), 1, 2))))
                    .collect();
                let expected = spans.iter().rev().find(|(r, _)| r.contains(&byte)).map(|(_, s)| *s);
                prop_assert_eq!(style_at(&spans, byte), expected);
            }
        }
    }
}
