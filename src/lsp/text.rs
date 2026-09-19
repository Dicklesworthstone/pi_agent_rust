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

/// Byte spans excluding line terminators. LSP treats CR, LF and CRLF as
/// line endings; a trailing terminator introduces a final empty line.
/// All position consumers share this iterator, including symbol selection.
fn line_ranges(content: &str) -> impl Iterator<Item = std::ops::Range<usize>> + '_ {
    let mut start = 0usize;
    let mut finished = false;
    std::iter::from_fn(move || {
        if finished {
            return None;
        }
        if let Some(relative) = content[start..].find(['\r', '\n']) {
            let end = start + relative;
            let span = start..end;
            start = end + 1;
            if content.as_bytes()[end] == b'\r'
                && content.as_bytes().get(start) == Some(&b'\n')
            {
                start += 1;
            }
            Some(span)
        } else {
            finished = true;
            Some(start..content.len())
        }
    })
}

/// Total number of lines in `content` (at least 1).
#[must_use]
pub fn line_count(content: &str) -> u32 {
    u32::try_from(line_ranges(content).count()).unwrap_or(u32::MAX)
}

/// Map an LSP position to a UTF-8 byte offset in `content`.
///
/// Returns `None` when the line is out of range. The character column clamps
/// to the end of the line (servers sometimes emit end-of-line positions on
/// lines with trailing terminators).
#[must_use]
pub fn position_to_offset(content: &str, position: Position) -> Option<usize> {
    let span = line_ranges(content).nth(usize::try_from(position.line).ok()?)?;
    let col = utf16_col_to_byte(&content[span.clone()], position.character);
    Some(span.start + col)
}

/// Map a caller-supplied position without rounding or clamping. Useful for
/// validating an explicitly selected range before sending it to a server.
#[must_use]
pub fn position_to_offset_exact(content: &str, position: Position) -> Option<usize> {
    let span = line_ranges(content).nth(usize::try_from(position.line).ok()?)?;
    let line = &content[span.clone()];
    let col = utf16_col_to_byte(line, position.character);
    (byte_col_to_utf16(line, col)? == position.character).then_some(span.start + col)
}

/// Map a UTF-8 byte offset in `content` to an LSP position.
///
/// Returns `None` for out-of-range offsets, non-character boundaries, and
/// the interior of a CRLF delimiter (which has no distinct LSP position).
#[must_use]
pub fn offset_to_position(content: &str, offset: usize) -> Option<Position> {
    if offset > content.len() || !content.is_char_boundary(offset) {
        return None;
    }
    let (line, span) = line_ranges(content)
        .enumerate()
        .find(|(_, span)| offset <= span.end)?;
    if offset < span.start {
        return None;
    }
    let character = byte_col_to_utf16(&content[span.clone()], offset - span.start)?;
    Some(Position { line: u32::try_from(line).ok()?, character })
}

/// One text replacement: splice `new_text` over `range`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    pub range: Range,
    pub new_text: String,
}

/// Apply LSP text edits to `content`, returning the new content.
///
/// Every edit addresses the original document. Same-position inserts retain
/// their order in the server's array; they may precede one replacement at
/// that position, but cannot follow it. Validate first, then construct the
/// result in one pass rather than shifting the whole suffix for each edit.
///
/// # Errors
///
/// Rejects invalid lines, split surrogate pairs, inverted/overlapping ranges
/// and allocation failures. Past-end columns retain LSP's end-of-line clamp.
pub fn apply_text_edits(content: &str, edits: &[TextEdit]) -> Result<String, String> {
    if edits.is_empty() {
        return Ok(content.to_string());
    }
    // Build the line index once: formatting can return thousands of edits.
    let lines: Vec<_> = line_ranges(content).collect();
    let edit_offset = |position: Position| -> Option<usize> {
        let span = lines.get(usize::try_from(position.line).ok()?)?;
        let line = &content[span.clone()];
        let col = utf16_col_to_byte(line, position.character);
        // Navigation may round, but an edit must not delete half a scalar
        // by silently rounding a UTF-16 surrogate-interior boundary.
        (byte_col_to_utf16(line, col)? <= position.character).then_some(span.start + col)
    };
    let mut mapped: Vec<(usize, usize, &str)> = Vec::with_capacity(edits.len());
    for edit in edits {
        if edit.range.end < edit.range.start {
            return Err(format!("edit range is inverted: {:?}", edit.range));
        }
        let start = edit_offset(edit.range.start).ok_or_else(|| {
            format!(
                "edit start position {}:{} is out of range or splits a surrogate pair",
                edit.range.start.line, edit.range.start.character
            )
        })?;
        let end = edit_offset(edit.range.end).ok_or_else(|| {
            format!(
                "edit end position {}:{} is out of range or splits a surrogate pair",
                edit.range.end.line, edit.range.end.character
            )
        })?;
        mapped.push((start, end, edit.new_text.as_str()));
    }
    // Stable sort is intentional: equal-start edits keep wire order.
    mapped.sort_by_key(|&(start, _, _)| start);
    let mut cursor = 0;
    let mut output_len = content.len();
    for &(start, end, new_text) in &mapped {
        if start < cursor {
            return Err(format!("edits overlap: byte range [{start}, {end}) starts before {cursor}"));
        }
        output_len = output_len.checked_sub(end - start)
            .and_then(|size| size.checked_add(new_text.len()))
            .ok_or_else(|| "edited document length overflow".to_string())?;
        cursor = end;
    }
    let mut out = String::new();
    out.try_reserve_exact(output_len)
        .map_err(|error| format!("cannot allocate edited document: {error}"))?;
    cursor = 0;
    for (start, end, new_text) in mapped {
        out.push_str(&content[cursor..start]);
        out.push_str(new_text);
        cursor = end;
    }
    out.push_str(&content[cursor..]);
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
        Some(line) => match usize::try_from(line).ok().and_then(|line| line_ranges(hay).nth(line)) {
            None => return out,
            Some(span) => (span.start, &hay[span]),
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

    fn replacement(line: u32, start: u32, end: u32, text: &str) -> TextEdit {
        TextEdit {
            range: Range {
                start: Position { line, character: start },
                end: Position { line, character: end },
            },
            new_text: text.to_string(),
        }
    }

    #[test]
    fn inserts_before_same_start_replacement_keep_server_order() {
        let edits = [
            replacement(0, 1, 1, "X"),
            replacement(0, 1, 1, "Y"),
            replacement(0, 1, 3, "Z"),
        ];
        assert_eq!(apply_text_edits("abcd", &edits).unwrap(), "aXYZd");
    }

    #[test]
    fn insertion_after_same_start_replacement_is_rejected() {
        let edits = [replacement(0, 1, 3, "Z"), replacement(0, 1, 1, "X")];
        assert!(apply_text_edits("abcd", &edits).unwrap_err().contains("overlap"));
    }

    #[test]
    fn unsorted_locations_preserve_equal_location_insertion_order() {
        let edits = [
            replacement(0, 5, 5, "!"),
            replacement(0, 1, 1, "X"),
            replacement(0, 5, 5, "?"),
            replacement(0, 1, 2, "Y"),
        ];
        assert_eq!(apply_text_edits("abcdef", &edits).unwrap(), "aXYcde!?f");
    }

    #[test]
    fn mixed_line_endings_and_unicode_have_exact_roundtrips() {
        for content in ["", "\r", "\n", "\r\n", "é\r🦀\r\nx\n", "a\r\rb"] {
            for offset in 0..=content.len() {
                let crlf_interior = offset > 0
                    && content.as_bytes().get(offset - 1) == Some(&b'\r')
                    && content.as_bytes().get(offset) == Some(&b'\n');
                let position = offset_to_position(content, offset);
                if content.is_char_boundary(offset) && !crlf_interior {
                    let position = position.expect("representable boundary");
                    assert_eq!(position_to_offset_exact(content, position), Some(offset));
                    assert_eq!(position_to_offset(content, position), Some(offset));
                } else {
                    assert!(position.is_none(), "{content:?} at {offset}");
                }
            }
        }
        assert_eq!(line_count("é\r🦀\r\nx\n"), 4);
        assert_eq!(offset_to_position("é\r🦀", 3), Some(Position { line: 1, character: 0 }));
    }

    #[test]
    fn explicit_positions_reject_clamping_and_surrogate_interiors() {
        let content = "a🦀b\r\n";
        assert_eq!(position_to_offset_exact(content, Position { line: 0, character: 3 }), Some(5));
        assert_eq!(position_to_offset_exact(content, Position { line: 0, character: 2 }), None);
        assert_eq!(position_to_offset_exact(content, Position { line: 0, character: 99 }), None);
        assert_eq!(position_to_offset(content, Position { line: 0, character: 99 }), Some(6));
    }

    #[test]
    fn edits_cannot_split_surrogates_but_retain_protocol_end_clamping() {
        for edit in [replacement(0, 2, 2, "X"), replacement(0, 1, 2, "X")] {
            assert!(apply_text_edits("a🦀b", &[edit]).unwrap_err().contains("surrogate"));
        }
        assert_eq!(apply_text_edits("a🦀b", &[replacement(0, 3, 99, "Z")]).unwrap(), "a🦀Z");
    }

    #[test]
    fn inverted_ranges_are_rejected_even_when_both_columns_clamp_to_eol() {
        assert!(apply_text_edits("abc", &[replacement(0, 99, 98, "X")])
            .unwrap_err().contains("inverted"));
    }

    #[test]
    fn edits_and_symbol_queries_agree_on_cr_lines() {
        let content = "first\ré old\r\nlast";
        assert_eq!(find_occurrences(content, "old", Some(1)), vec![(9, 3)]);
        assert!(find_occurrences(content, "last", Some(1)).is_empty());
        assert!(find_occurrences(content, "\r", Some(1)).is_empty());
        assert_eq!(apply_text_edits(content, &[replacement(1, 2, 5, "new")]).unwrap(), "first\ré new\r\nlast");
    }

    #[test]
    fn large_reversed_batch_uses_original_positions() {
        let content = "old\r\n".repeat(4096);
        let edits: Vec<_> = (0..4096).rev().map(|line| replacement(line, 0, 3, "formatted")).collect();
        assert_eq!(apply_text_edits(&content, &edits).unwrap(), "formatted\r\n".repeat(4096));
    }

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
        // LSP defines the array order, not either arbitrary interleaving.
        assert_eq!(out, "aXYb\n");
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
