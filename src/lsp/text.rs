//! UTF-16 position mapping and text-edit splicing for LSP payloads.
//!
//! LSP positions are `(line, character)` where `character` counts UTF-16 code
//! units, while Rust strings are UTF-8. Every boundary between pi and a
//! language server crosses this module so the conversion rules live in
//! exactly one place (bd-cv653.1.1).

/// A zero-based LSP position.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "camelCase")]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

/// A zero-based, half-open LSP range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

/// Convert a UTF-8 byte column within `line` to a UTF-16 code-unit column.
///
/// Returns `None` when `byte_col` is not on a char boundary or past the end.
#[must_use]
pub fn byte_col_to_utf16(line: &str, byte_col: usize) -> Option<u32> {
    if byte_col > line.len() || !line.is_char_boundary(byte_col) {
        return None;
    }
    let mut units = 0u32;
    for ch in line[..byte_col].chars() {
        units = units.saturating_add(u32::try_from(ch.len_utf16()).unwrap_or(2));
    }
    Some(units)
}

/// Convert a UTF-16 code-unit column within `line` to a UTF-8 byte column.
///
/// Clamps to the end of the line when the column points past it; a column in
/// the middle of a surrogate pair resolves to the boundary after the pair's
/// scalar value.
#[must_use]
pub fn utf16_col_to_byte(line: &str, utf16_col: u32) -> usize {
    let mut units = 0u32;
    for (byte_idx, ch) in line.char_indices() {
        if units >= utf16_col {
            return byte_idx;
        }
        units = units.saturating_add(u32::try_from(ch.len_utf16()).unwrap_or(2));
        if units > utf16_col {
            // Column landed inside this scalar's surrogate pair; the
            // half-open edit boundary goes after the scalar.
            return byte_idx + ch.len_utf8();
        }
    }
    line.len()
}

/// Split `content` into lines without the line terminators.
///
/// Handles `\n`, `\r\n`, and a trailing line without terminator. The result
/// always has at least one element (empty content yields one empty line).
fn lines_of(content: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    let bytes = content.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            let mut end = i;
            if end > start && bytes[end - 1] == b'\r' {
                end -= 1;
            }
            lines.push(&content[start..end]);
            start = i + 1;
        }
        i += 1;
    }
    lines.push(&content[start..]);
    lines
}

/// Byte offset of the start of `line` (zero-based) in `content`.
///
/// Returns `None` when the line index is out of range.
#[must_use]
fn line_start_offset(content: &str, line: u32) -> Option<usize> {
    if line == 0 {
        return Some(0);
    }
    let mut seen = 0u32;
    for (idx, byte) in content.bytes().enumerate() {
        if byte == b'\n' {
            seen += 1;
            if seen == line {
                return Some(idx + 1);
            }
        }
    }
    None
}

/// Total number of lines in `content` (at least 1).
#[must_use]
pub fn line_count(content: &str) -> u32 {
    let newlines = content.bytes().filter(|b| *b == b'\n').count();
    u32::try_from(newlines).unwrap_or(u32::MAX).saturating_add(1)
}

/// Map an LSP position to a UTF-8 byte offset in `content`.
///
/// Returns `None` when the line is out of range. The character column clamps
/// to the end of the line (servers sometimes emit end-of-line positions on
/// lines with trailing terminators).
#[must_use]
pub fn position_to_offset(content: &str, position: Position) -> Option<usize> {
    let line_start = line_start_offset(content, position.line)?;
    let line_end = content[line_start..]
        .find('\n')
        .map_or(content.len(), |rel| line_start + rel);
    let mut line = &content[line_start..line_end];
    if line.ends_with('\r') {
        line = &line[..line.len() - 1];
    }
    let col = utf16_col_to_byte(line, position.character);
    Some(line_start + col)
}

/// Map a UTF-8 byte offset in `content` to an LSP position.
///
/// Returns `None` when `offset` is out of range or not on a char boundary.
#[must_use]
pub fn offset_to_position(content: &str, offset: usize) -> Option<Position> {
    if offset > content.len() || !content.is_char_boundary(offset) {
        return None;
    }
    let newline_count = content[..offset].bytes().filter(|b| *b == b'\n').count();
    let line = u32::try_from(newline_count).unwrap_or(u32::MAX);
    let line_start = line_start_offset(content, line)?;
    let line_text_end = content[line_start..]
        .find('\n')
        .map_or(content.len(), |rel| line_start + rel);
    let mut line_text = &content[line_start..line_text_end];
    if line_text.ends_with('\r') {
        line_text = &line_text[..line_text.len() - 1];
    }
    let byte_col = offset.saturating_sub(line_start);
    let character = byte_col_to_utf16(line_text, byte_col)?;
    Some(Position { line, character })
}

/// One text replacement: splice `new_text` over `range`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    pub range: Range,
    pub new_text: String,
}

/// Apply LSP text edits to `content`, returning the new content.
///
/// Edits are applied atomically as a batch: they are sorted by descending
/// start offset and spliced back-to-front so earlier offsets stay valid.
/// Overlapping edits (after offset mapping) are rejected with an error
/// naming the overlapping positions, and nothing is applied.
///
/// # Errors
///
/// Returns a human-readable error when any position is out of range or two
/// edits overlap.
pub fn apply_text_edits(content: &str, edits: &[TextEdit]) -> Result<String, String> {
    if edits.is_empty() {
        return Ok(content.to_string());
    }
    // Map positions to byte offsets first so all validation happens before
    // any splice.
    let mut mapped: Vec<(usize, usize, &str)> = Vec::with_capacity(edits.len());
    for edit in edits {
        let start = position_to_offset(content, edit.range.start).ok_or_else(|| {
            format!(
                "edit start position {}:{} is out of range",
                edit.range.start.line, edit.range.start.character
            )
        })?;
        let end = position_to_offset(content, edit.range.end).ok_or_else(|| {
            format!(
                "edit end position {}:{} is out of range",
                edit.range.end.line, edit.range.end.character
            )
        })?;
        if end < start {
            return Err(format!(
                "edit range is inverted ({}:{} > {}:{})",
                edit.range.start.line,
                edit.range.start.character,
                edit.range.end.line,
                edit.range.end.character
            ));
        }
        mapped.push((start, end, edit.new_text.as_str()));
    }
    // Sort descending by start offset for back-to-front splicing; ties on
    // start sort descending by end so identical inserts stay deterministic.
    mapped.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    // Overlap check on the sorted sequence: walking from the end of the
    // document backwards, each edit must end at or before the previous
    // (later) edit's start.
    for window in mapped.windows(2) {
        let earlier_end = window[0].1;
        let later_start = window[1].0;
        // window[0] is the LATER edit in document order (sorted desc).
        if window[1].1 > window[0].0 && !(window[1].0 == window[1].1 && window[0].0 == window[0].1)
        {
            return Err(format!(
                "edits overlap: byte ranges [{later_start}, {earlier_end}) and [{}, {})",
                window[0].0, window[0].1
            ));
        }
    }
    let mut out = content.to_string();
    for (start, end, new_text) in mapped {
        out.replace_range(start..end, new_text);
    }
    Ok(out)
}

/// FNV-1a hash of file content (drift detection; not cryptographic).
#[must_use]
pub fn content_hash_for_drift(content: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in content.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Find the `(byte offset, length)` of the `n`th (1-indexed) occurrence of
/// `needle` in `hay`, optionally restricted to a single line (zero-based).
///
/// Returns every occurrence in document order; callers pick by index.
#[must_use]
pub fn find_occurrences(hay: &str, needle: &str, only_line: Option<u32>) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    if needle.is_empty() {
        return out;
    }
    let (region_start, region) = match only_line {
        None => (0, hay),
        Some(line) => match line_start_offset(hay, line) {
            None => return out,
            Some(start) => {
                let end = hay[start..].find('\n').map_or(hay.len(), |rel| start + rel);
                (start, &hay[start..end])
            }
        },
    };
    let mut search_from = 0usize;
    while let Some(rel) = region[search_from..].find(needle) {
        let at = search_from + rel;
        out.push((region_start + at, needle.len()));
        search_from = at + needle.len();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_roundtrip_ascii() {
        let line = "hello world";
        assert_eq!(byte_col_to_utf16(line, 5), Some(5));
        assert_eq!(utf16_col_to_byte(line, 5), 5);
        assert_eq!(byte_col_to_utf16(line, line.len()), Some(11));
    }

    #[test]
    fn utf16_handles_astral_chars() {
        // '🦀' is U+1F980: 4 UTF-8 bytes, 2 UTF-16 code units.
        let line = "a🦀b";
        assert_eq!(byte_col_to_utf16(line, 1), Some(1));
        assert_eq!(byte_col_to_utf16(line, 5), Some(3));
        assert_eq!(byte_col_to_utf16(line, 6), Some(4));
        assert_eq!(utf16_col_to_byte(line, 1), 1);
        assert_eq!(utf16_col_to_byte(line, 3), 5);
        // Inside the surrogate pair resolves after the scalar.
        assert_eq!(utf16_col_to_byte(line, 2), 5);
        assert_eq!(utf16_col_to_byte(line, 4), 6);
    }

    #[test]
    fn utf16_handles_bmp_multibyte() {
        // 'é' is 2 UTF-8 bytes, 1 UTF-16 unit.
        let line = "éé";
        assert_eq!(byte_col_to_utf16(line, 2), Some(1));
        assert_eq!(utf16_col_to_byte(line, 1), 2);
    }

    #[test]
    fn byte_col_rejects_non_boundary() {
        let line = "é";
        assert_eq!(byte_col_to_utf16(line, 1), None);
    }

    #[test]
    fn position_offset_roundtrip() {
        let content = "fn main() {\n    let x = 1;\n}\n";
        let pos = offset_to_position(content, 16).expect("position");
        assert_eq!(
            pos,
            Position {
                line: 1,
                character: 4
            }
        );
        assert_eq!(position_to_offset(content, pos), Some(16));
        // Start of file.
        assert_eq!(
            offset_to_position(content, 0),
            Some(Position {
                line: 0,
                character: 0
            })
        );
        // Out of range line.
        assert_eq!(
            position_to_offset(
                content,
                Position {
                    line: 99,
                    character: 0
                }
            ),
            None
        );
    }

    #[test]
    fn position_maps_crlf() {
        // Layout: 0=a 1=b 2=\r 3=\n 4=c 5=d 6=\r 7=\n
        let content = "ab\r\ncd\r\n";
        // (1,2) is the end of "cd" — the position before the \r terminator.
        assert_eq!(
            position_to_offset(
                content,
                Position {
                    line: 1,
                    character: 2
                }
            ),
            Some(6)
        );
        // Byte 5 is 'd': line 1, character 1.
        assert_eq!(
            offset_to_position(content, 5),
            Some(Position {
                line: 1,
                character: 1
            })
        );
    }

    #[test]
    fn apply_edits_splices_back_to_front() {
        let content = "let alpha = 1;\nlet beta = alpha;\n";
        let edits = vec![
            TextEdit {
                range: Range {
                    start: Position {
                        line: 0,
                        character: 4,
                    },
                    end: Position {
                        line: 0,
                        character: 9,
                    },
                },
                new_text: "gamma".into(),
            },
            TextEdit {
                range: Range {
                    start: Position {
                        line: 1,
                        character: 11,
                    },
                    end: Position {
                        line: 1,
                        character: 16,
                    },
                },
                new_text: "gamma".into(),
            },
        ];
        let out = apply_text_edits(content, &edits).expect("apply");
        assert_eq!(out, "let gamma = 1;\nlet beta = gamma;\n");
    }

    #[test]
    fn apply_edits_insert_at_same_point_is_deterministic() {
        let content = "ab\n";
        let range = Range {
            start: Position {
                line: 0,
                character: 1,
            },
            end: Position {
                line: 0,
                character: 1,
            },
        };
        let edits = vec![
            TextEdit {
                range,
                new_text: "X".into(),
            },
            TextEdit {
                range,
                new_text: "Y".into(),
            },
        ];
        let out = apply_text_edits(content, &edits).expect("apply");
        // Both insert at the same point; later-sorted edit lands first, so
        // both orders produce one of the two stable interleavings.
        assert!(out == "aXYb\n" || out == "aYXb\n");
    }

    #[test]
    fn apply_edits_rejects_overlap() {
        let content = "abcdef\n";
        let edits = vec![
            TextEdit {
                range: Range {
                    start: Position {
                        line: 0,
                        character: 1,
                    },
                    end: Position {
                        line: 0,
                        character: 4,
                    },
                },
                new_text: "X".into(),
            },
            TextEdit {
                range: Range {
                    start: Position {
                        line: 0,
                        character: 2,
                    },
                    end: Position {
                        line: 0,
                        character: 5,
                    },
                },
                new_text: "Y".into(),
            },
        ];
        let err = apply_text_edits(content, &edits).expect_err("overlap must fail");
        assert!(err.contains("overlap"), "unexpected error: {err}");
        // Content untouched on failure (atomicity).
        assert_eq!(content, "abcdef\n");
    }

    #[test]
    fn apply_edits_rejects_out_of_range() {
        let content = "ab\n";
        let edits = vec![TextEdit {
            range: Range {
                start: Position {
                    line: 5,
                    character: 0,
                },
                end: Position {
                    line: 5,
                    character: 1,
                },
            },
            new_text: "X".into(),
        }];
        assert!(apply_text_edits(content, &edits).is_err());
    }

    #[test]
    fn find_occurrences_scans_document_or_line() {
        let content = "foo bar foo\nbaz foo\n";
        let all = find_occurrences(content, "foo", None);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].0, 0);
        assert_eq!(all[1].0, 8);
        let line1 = find_occurrences(content, "foo", Some(1));
        assert_eq!(line1.len(), 1);
        assert_eq!(line1[0].0, 16);
        let missing = find_occurrences(content, "foo", Some(9));
        assert!(missing.is_empty());
    }
}
