//! Mouse drag selection in the captured FrankenTUI transcript.
//! All positions refer to already wrapped visual rows, not raw markdown.

use ftui::text::Line;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Point {
    pub row: usize,
    pub cell: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Selection {
    pub anchor: Point,
    pub focus: Point,
}

impl Selection {
    pub(super) const fn new(point: Point) -> Self {
        Self { anchor: point, focus: point }
    }

    pub(super) fn bounds(self) -> (Point, Point) {
        if self.anchor <= self.focus {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        }
    }

    pub(super) fn is_empty(self) -> bool {
        self.anchor == self.focus
    }

    fn row_bounds(self, row: usize, width: usize) -> Option<(usize, usize)> {
        let (start, end) = self.bounds();
        if row < start.row || row > end.row || self.is_empty() {
            return None;
        }
        let first = if row == start.row { start.cell } else { 0 };
        let last = if row == end.row { end.cell } else { width };
        (first < last).then_some((first, last))
    }
}

/// Extract an exact bounded selection from the same wrapped rows users see.
/// `split_at_cell` preserves grapheme boundaries, even for wide glyphs.
pub(super) fn selected_text(
    lines: &[Line<'static>],
    selection: Selection,
    max_bytes: usize,
) -> Option<String> {
    let (start, end) = selection.bounds();
    if selection.is_empty() || start.row >= lines.len() || end.row >= lines.len() {
        return None;
    }
    let mut output = String::new();
    for row in start.row..=end.row {
        if row > start.row {
            output.push('\n');
        }
        let line = &lines[row];
        let begin = if row == start.row { start.cell } else { 0 };
        let finish = if row == end.row { end.cell } else { line.width() };
        let mut consumed = 0;
        for span in line.spans() {
            let width = span.width();
            let from = begin.saturating_sub(consumed).min(width);
            let until = finish.saturating_sub(consumed).min(width);
            if from < until {
                let (_, rest) = span.split_at_cell(from);
                let (piece, _) = rest.split_at_cell(until - from);
                output.push_str(piece.as_str());
                if output.len() > max_bytes {
                    return None;
                }
            }
            consumed += width;
        }
        if output.len() > max_bytes {
            return None;
        }
    }
    (!output.is_empty()).then_some(output)
}

/// Highlight only the selected cells. OSC-8 destinations remain on split spans.
pub(super) fn highlight(
    lines: &mut [Line<'static>],
    selection: Selection,
    accent: ftui::PackedRgba,
) {
    let (start, end) = selection.bounds();
    if selection.is_empty() || lines.is_empty() {
        return;
    }
    for (row, line) in lines.iter_mut().enumerate().take(end.row.saturating_add(1)).skip(start.row) {
        let Some((begin, finish)) = selection.row_bounds(row, line.width()) else {
            continue;
        };
        let mut pieces = Vec::new();
        let mut consumed = 0;
        for span in line.spans() {
            let width = span.width();
            let from = begin.saturating_sub(consumed).min(width);
            let until = finish.saturating_sub(consumed).min(width);
            let (before, rest) = span.split_at_cell(from);
            let (mut middle, after) = rest.split_at_cell(until - from);
            if !before.is_empty() { pieces.push(before); }
            if !middle.is_empty() {
                middle.style = Some(middle.style.unwrap_or_else(ftui::Style::new).bg(accent));
                pieces.push(middle);
            }
            if !after.is_empty() { pieces.push(after); }
            consumed += width;
        }
        *line = Line::from_spans(pieces);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_across_wrapped_link_rows_keeps_full_text() {
        let target = "https://example.test/oauth?challenge=abcdefghijklmnopqrstuvwxyz";
        let mut span = ftui::text::Span::raw(target.to_string());
        span.link = Some(target.into());
        let line = Line::from_spans(vec![span]);
        let rows = line.wrap(18, ftui::text::WrapMode::WordChar);
        assert!(rows.len() > 2);
        let last = rows.len() - 1;
        let selection = Selection {
            anchor: Point { row: 0, cell: 0 },
            focus: Point { row: last, cell: rows[last].width() },
        };
        let extracted = selected_text(&rows, selection, 1024).expect("selected link");
        assert_eq!(extracted.replace('\n', ""), target);
        let mut marked = rows;
        highlight(&mut marked, selection, ftui::PackedRgba::rgb(70, 100, 130));
        for row in &marked {
            for span in row.spans() {
                assert_eq!(span.link.as_deref(), Some(target));
            }
        }
    }

    #[test]
    fn selection_supports_reverse_drag_and_bounded_output() {
        let lines = vec![Line::raw("before".to_string()), Line::raw("after".to_string())];
        let selection = Selection {
            anchor: Point { row: 1, cell: 3 },
            focus: Point { row: 0, cell: 2 },
        };
        assert_eq!(selected_text(&lines, selection, 100).as_deref(), Some("fore\naft"));
        assert!(selected_text(&lines, selection, 3).is_none());
    }

    #[test]
    fn identical_endpoints_are_not_a_selection() {
        let lines = vec![Line::raw("hello".to_string())];
        let point = Point { row: 0, cell: 2 };
        assert!(selected_text(&lines, Selection::new(point), 100).is_none());
    }
}
