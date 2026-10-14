//! Exact code excerpts from a validated, immutable document-symbol source.
//!
//! There is no file access here. The outline's one-pass endpoint map is reused
//! even for thousands of duplicate/nested definitions. Selection ranges never
//! stand in for implementation ranges, and clipped output remains explicit.

use std::collections::BTreeMap;

use sha2::{Digest as _, Sha256};

use super::{Budget, Outline, Position, Result, Symbol, Value, json, malformed};

const MAX_EXCERPT_BYTES: usize = 16 * 1024;
const CHECK_CHUNK: usize = 4096;

pub(super) fn fingerprint(text: &str, budget: &Budget) -> Result<String> {
    let mut digest = Sha256::new();
    for chunk in text.as_bytes().chunks(CHECK_CHUNK) {
        budget.remaining()?;
        digest.update(chunk);
    }
    budget.remaining()?;
    Ok(crate::package_manager::hex_encode(&digest.finalize()))
}

/// Advance over at most one bounded excerpt, not the preceding document.
/// No valid excerpt endpoint can lie between the CR and LF of one delimiter.
fn end_position(mut position: Position, text: &str, budget: &Budget) -> Result<Position> {
    let mut chars = text.char_indices().peekable();
    let mut checked = 0;
    while let Some((offset, ch)) = chars.next() {
        if offset.saturating_sub(checked) >= CHECK_CHUNK {
            budget.remaining()?;
            checked = offset;
        }
        if matches!(ch, '\r' | '\n') {
            if ch == '\r' && chars.peek().is_some_and(|(_, next)| *next == '\n') {
                chars.next();
            }
            position.line = position.line.checked_add(1)
                .ok_or_else(|| malformed("source excerpt line overflow"))?;
            position.character = 0;
        } else {
            position.character = position.character.checked_add(u32::try_from(ch.len_utf16()).unwrap_or(2))
                .ok_or_else(|| malformed("source excerpt column overflow"))?;
        }
    }
    budget.remaining()?;
    Ok(position)
}

/// A bounded sparse index, not one allocation for every line. The source is
/// traversed once; each later position lookup scans about one 4 KiB interval.
/// Retaining only validated symbol endpoints would still rescan an entire
/// large class for every clipped window inside that class.
pub(super) struct SourceText<'a> {
    text: &'a str,
    checkpoints: BTreeMap<usize, Position>,
    focus: Option<usize>,
    focus_line: Option<u32>,
}

impl<'a> SourceText<'a> {
    pub(super) fn new(text: &'a str, focus_line: Option<u32>, budget: &Budget) -> Result<Self> {
        budget.remaining()?;
        let mut current = Position { line: 0, character: 0 };
        let mut checkpoints = BTreeMap::from([(0, current)]);
        let mut chars = text.char_indices().peekable();
        let mut offset = 0;
        let mut next_checkpoint = CHECK_CHUNK;
        let mut focus = None;
        loop {
            if focus_line == Some(current.line) && current.character == 0 {
                focus = Some(offset);
            }
            if offset >= next_checkpoint {
                budget.remaining()?;
                checkpoints.insert(offset, current);
                next_checkpoint = offset.saturating_add(CHECK_CHUNK);
            }
            let Some((at, ch)) = chars.next() else { break };
            offset = at + ch.len_utf8();
            if matches!(ch, '\r' | '\n') {
                if ch == '\r' && chars.peek().is_some_and(|(_, ch)| *ch == '\n') {
                    chars.next();
                    offset += 1;
                }
                current.line = current.line.checked_add(1)
                    .ok_or_else(|| malformed("source excerpt line overflow"))?;
                current.character = 0;
            } else {
                current.character = current.character.checked_add(u32::try_from(ch.len_utf16()).unwrap_or(2))
                    .ok_or_else(|| malformed("source excerpt column overflow"))?;
            }
        }
        budget.remaining()?;
        if focus_line.is_some() && focus.is_none() {
            return Err(malformed("source excerpt focus line is outside the source"));
        }
        Ok(Self { text, checkpoints, focus, focus_line })
    }

    fn position(&self, offset: usize, budget: &Budget) -> Result<Position> {
        let (start, position) = self.checkpoints.range(..=offset).next_back()
            .ok_or_else(|| malformed("source excerpt lacks a position checkpoint"))?;
        let text = self.text.get(*start..offset)
            .ok_or_else(|| malformed("source excerpt offset is not a UTF-8 boundary"))?;
        if offset > 0 && self.text.as_bytes().get(offset - 1) == Some(&b'\r')
            && self.text.as_bytes().get(offset) == Some(&b'\n')
        {
            return Err(malformed("source excerpt offset splits CRLF"));
        }
        end_position(*position, text, budget)
    }

    pub(super) fn excerpt(&self, outline: &Outline<'_>, symbol: &Symbol<'_>, budget: &Budget) -> Result<Value> {
        self.with_limit(outline, symbol, MAX_EXCERPT_BYTES, budget)
    }

    fn with_limit(&self, outline: &Outline<'_>, symbol: &Symbol<'_>, limit: usize, budget: &Budget) -> Result<Value> {
        budget.remaining()?;
        let offset = |position: Position| {
            outline.offsets.get(&position).copied()
                .ok_or_else(|| malformed("source excerpt lacks a validated endpoint"))
        };
        let start = offset(symbol.range.start)?;
        let end = offset(symbol.range.end)?;
        let full = self.text.get(start..end)
            .ok_or_else(|| malformed("source excerpt is outside the checked source"))?;
        // Keep short definitions whole. For long ones, reserve a quarter of
        // the window for preceding context. Do not shift a near-EOF window
        // backwards merely to fill its quota: rounding that start could push
        // the requested final line out of the clipped end.
        let wanted_start = if full.len() > limit {
            self.focus.map_or(start, |focus| focus.clamp(start, end)
                .saturating_sub(limit / 4).max(start))
        } else { start };
        let first = floor_boundary(self.text, wanted_start);
        let last = floor_boundary(self.text, first.saturating_add(limit).min(end));
        let snippet = self.text.get(first..last)
            .ok_or_else(|| malformed("source excerpt has an invalid window"))?;
        let omitted_before = first - start;
        let omitted_after = end - last;
        let truncated = omitted_before != 0 || omitted_after != 0;
        let mode = if !truncated { "full" } else if self.focus.is_some() { "line_context" } else { "prefix" };
        let range_start = if first == start { symbol.range.start } else { self.position(first, budget)? };
        let range_end = if last == end { symbol.range.end } else { self.position(last, budget)? };
        budget.remaining()?;
        Ok(json!({
            "text":snippet,"mode":mode,"focusLine":self.focus_line.map(|line| u64::from(line) + 1),
            "extent":if symbol.selection.is_some() { "document_symbol_range" } else { "symbol_location_range" },
            "range":{"start":range_start,"end":range_end},
            "byteRange":{"start":first,"end":last},
            "totalBytes":full.len(),"omittedBytes":omitted_before + omitted_after,
            "omittedBeforeBytes":omitted_before,"omittedAfterBytes":omitted_after,
            "truncated":truncated
        }))
    }
}

fn floor_boundary(text: &str, mut offset: usize) -> usize {
    offset = offset.min(text.len());
    while !text.is_char_boundary(offset) { offset -= 1; }
    if offset > 0 && text.as_bytes().get(offset - 1) == Some(&b'\r')
        && text.as_bytes().get(offset) == Some(&b'\n')
    {
        offset -= 1;
    }
    offset
}

// Preserve the original prefix checks as the no-focus case of the same reader.
#[cfg(test)]
fn excerpt(text: &str, outline: &Outline<'_>, symbol: &Symbol<'_>, budget: &Budget) -> Result<Value> {
    SourceText::new(text, None, budget)?.excerpt(outline, symbol, budget)
}

#[cfg(test)]
fn excerpt_with_limit(text: &str, outline: &Outline<'_>, symbol: &Symbol<'_>, limit: usize, budget: &Budget) -> Result<Value> {
    SourceText::new(text, None, budget)?.with_limit(outline, symbol, limit, budget)
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::{MAX_DISPLAY, MAX_PAYLOAD_BYTES, displayed_with_source, parse, validate_boundaries};
    use crate::lsp::text::{offset_to_position, position_to_offset_exact};
    use std::collections::BTreeSet;
    use std::path::Path;
    use std::time::Duration;

    fn budget() -> Budget { Budget::new(Duration::from_secs(5)) }

    fn raw(text: &str, start: usize, end: usize) -> Value {
        let start = offset_to_position(text, start).unwrap();
        let end = offset_to_position(text, end).unwrap();
        json!([{"name":"target","kind":12,
            "range":{"start":start,"end":end},
            "selectionRange":{"start":start,"end":start}}])
    }

    #[test]
    fn excerpts_use_the_full_symbol_not_just_its_identifier() {
        let text = "// target is only a comment\r\nfn target() {\r\n  return \"日本😀\";\r\n}\r\nnext();\n";
        let start = text.find("fn target").unwrap();
        let end = text.find("\r\nnext").unwrap();
        let raw = raw(text, start, end);
        let budget = budget();
        let outline = parse(&raw, Path::new("unused"), text, &budget).unwrap();
        let source = excerpt(text, &outline, &outline.symbols[0], &budget).unwrap();
        assert_eq!(source["text"], &text[start..end]);
        assert_eq!(source["byteRange"], json!({"start":start,"end":end}));
        assert_eq!(source["range"], raw[0]["range"]);
        assert_eq!(source["totalBytes"], end - start);
        assert_eq!(source["omittedBytes"], 0);
        assert_eq!(source["truncated"], false);
    }

    #[test]
    fn every_clip_boundary_roundtrips_unicode_cr_lf_and_crlf() {
        let text = "prefix😀x\r\n界e\u{301}\rnext\nlast";
        let raw = raw(text, 6, text.len());
        let budget = budget();
        let outline = parse(&raw, Path::new("unused"), text, &budget).unwrap();
        for limit in 0..=text.len() {
            let source = excerpt_with_limit(text, &outline, &outline.symbols[0], limit, &budget).unwrap();
            let snippet = source["text"].as_str().unwrap();
            let start: Position = serde_json::from_value(source["range"]["start"].clone()).unwrap();
            let end: Position = serde_json::from_value(source["range"]["end"].clone()).unwrap();
            let first = position_to_offset_exact(text, start).unwrap();
            let last = position_to_offset_exact(text, end).unwrap();
            assert_eq!(&text[first..last], snippet);
            assert!(snippet.len() <= limit);
            assert_eq!(usize::try_from(source["omittedBytes"].as_u64().unwrap()).unwrap() + snippet.len(), text.len() - 6);
            assert_eq!(source["truncated"], last < text.len());
        }
    }

    #[test]
    fn mapped_offsets_equal_the_independent_shared_mapper() {
        for text in ["", "😀", "\r\n", "a\rb\n界\r\n😀z\n"] {
            let wanted: BTreeSet<_> = (0..=text.len())
                .filter_map(|offset| offset_to_position(text, offset)).collect();
            let mapped = validate_boundaries(text, &wanted, &budget()).unwrap();
            for position in wanted {
                assert_eq!(mapped.get(&position).copied(), position_to_offset_exact(text, position));
            }
        }
    }

    #[test]
    fn source_rows_share_the_encoded_output_budget_and_metadata_stays_optional() {
        let text = "\0".repeat(MAX_EXCERPT_BYTES + 1);
        let raw = json!(vec![raw(&text, 0, text.len())[0].clone(); 64]);
        let budget = budget();
        let outline = parse(&raw, Path::new("unused"), &text, &budget).unwrap();
        let rows = displayed_with_source(&outline, 0..64, MAX_DISPLAY, Some((&text, None)), &budget).unwrap();
        assert!(!rows.is_empty() && rows.len() < 64);
        assert!(serde_json::to_vec(&rows).unwrap().len() <= MAX_PAYLOAD_BYTES - 8192);
        assert!(rows.iter().all(|row| row["source"]["truncated"] == true));
        assert_eq!(rows[0]["source"]["text"].as_str().unwrap().len(), MAX_EXCERPT_BYTES);
        let plain = displayed_with_source(&outline, 0..64, 1, None, &budget).unwrap();
        assert!(plain[0].get("source").is_none());
    }

    #[test]
    fn fingerprint_is_exact_and_cancelled_extraction_never_succeeds() {
        let text = "fn target() {}\n";
        let raw = raw(text, 0, text.len());
        let budget = budget();
        let outline = parse(&raw, Path::new("unused"), text, &budget).unwrap();
        let expected = crate::package_manager::hex_encode(&<Sha256 as sha2::Digest>::digest(text.as_bytes()));
        assert_eq!(fingerprint(text, &budget).unwrap(), expected);
        assert_ne!(fingerprint(text.trim_end(), &budget).unwrap(), expected);
        budget.owner.cancel_with(asupersync::types::CancelKind::User, Some("stop source extraction"));
        assert!(fingerprint(text, &budget).is_err());
        assert!(excerpt(text, &outline, &outline.symbols[0], &budget).is_err());
    }

    #[test]
    fn flat_locations_are_not_presented_as_inferred_function_bodies() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().canonicalize().unwrap().join("not-opened.rs");
        let uri = crate::lsp::client::try_path_to_uri(&path).unwrap();
        let text = "fn target() { body(); }";
        let raw = json!([{"name":"target","kind":12,"location":{"uri":uri,"range":{
            "start":{"line":0,"character":3},"end":{"line":0,"character":9}
        }}}]);
        let budget = budget();
        let outline = parse(&raw, &path, text, &budget).unwrap();
        let source = excerpt(text, &outline, &outline.symbols[0], &budget).unwrap();
        assert_eq!(source["text"], "target");
        assert_eq!(source["extent"], "symbol_location_range");
        assert!(!path.exists());
    }

    #[test]
    fn line_context_includes_late_code_instead_of_repeating_a_large_prefix() {
        let text = format!("unrelated\nfn target() {{\n{}    target_line();\n}}\n", "    padding();\r\n".repeat(3000));
        let focus = text.find("    target_line").unwrap();
        let focus_line = offset_to_position(&text, focus).unwrap().line;
        let start = text.find("fn target").unwrap();
        let raw = raw(&text, start, text.len());
        let budget = budget();
        let outline = parse(&raw, Path::new("unused"), &text, &budget).unwrap();
        let reader = SourceText::new(&text, Some(focus_line), &budget).unwrap();
        let source = reader.excerpt(&outline, &outline.symbols[0], &budget).unwrap();
        let snippet = source["text"].as_str().unwrap();
        assert!(snippet.contains("target_line();"));
        assert!(!snippet.contains("unrelated"));
        assert!(snippet.len() <= MAX_EXCERPT_BYTES);
        assert_eq!(source["mode"], "line_context");
        assert_eq!(source["focusLine"], u64::from(focus_line) + 1);
        assert!(source["omittedBeforeBytes"].as_u64().unwrap() > 0);
        assert_eq!(source["omittedAfterBytes"], 0);
        assert_eq!(source["omittedBytes"], source["omittedBeforeBytes"]);
        let a: Position = serde_json::from_value(source["range"]["start"].clone()).unwrap();
        let b: Position = serde_json::from_value(source["range"]["end"].clone()).unwrap();
        assert_eq!(&text[position_to_offset_exact(&text, a).unwrap()..position_to_offset_exact(&text, b).unwrap()], snippet);
    }

    #[test]
    fn sparse_position_index_matches_exact_mapping_without_allocating_per_line() {
        let text = format!("{}😀x\r\n{}界e\u{301}\r", "\n".repeat(9000), "x".repeat(9000));
        let budget = budget();
        let reader = SourceText::new(&text, None, &budget).unwrap();
        assert!(reader.checkpoints.len() < 10, "not a 9000-entry line index");
        for offset in [0, 4095, 4096, 4097, 8192, 9000, 9001, 9004, 9006, text.len() - 1, text.len()] {
            let expected = offset_to_position(&text, offset);
            assert_eq!(reader.position(offset, &budget).ok(), expected, "byte {offset}");
        }
    }

    #[test]
    fn focused_windows_remain_exact_at_all_scalar_and_newline_boundaries() {
        let text = "outside\r\n😀界x\r\n  focused();\rmore\nlast😀";
        let raw = raw(text, 9, text.len());
        let budget = budget();
        let outline = parse(&raw, Path::new("unused"), text, &budget).unwrap();
        for line in 1..=4 {
            let reader = SourceText::new(text, Some(line), &budget).unwrap();
            for limit in 0..=text.len() {
                let source = reader.with_limit(&outline, &outline.symbols[0], limit, &budget).unwrap();
                let a: Position = serde_json::from_value(source["range"]["start"].clone()).unwrap();
                let b: Position = serde_json::from_value(source["range"]["end"].clone()).unwrap();
                let first = position_to_offset_exact(text, a).unwrap();
                let last = position_to_offset_exact(text, b).unwrap();
                let snippet = source["text"].as_str().unwrap();
                assert_eq!(&text[first..last], snippet);
                assert!(first >= 9 && snippet.len() <= limit);
                assert_eq!(source["omittedBeforeBytes"].as_u64().unwrap() + source["omittedAfterBytes"].as_u64().unwrap(), source["omittedBytes"].as_u64().unwrap());
                assert_eq!(u64::try_from(snippet.len()).unwrap() + source["omittedBytes"].as_u64().unwrap(), source["totalBytes"].as_u64().unwrap());
            }
        }
    }

    #[test]
    fn focus_clamps_to_the_definition_and_empty_ranges_remain_empty() {
        let text = "before(); target(); after();\r\n";
        let raw = raw(text, 10, 18);
        let budget = budget();
        let outline = parse(&raw, Path::new("unused"), text, &budget).unwrap();
        let reader = SourceText::new(text, Some(0), &budget).unwrap();
        let source = reader.excerpt(&outline, &outline.symbols[0], &budget).unwrap();
        assert_eq!(source["text"], "target()");
        assert_eq!(source["mode"], "full");
        assert_eq!(source["omittedBytes"], 0);
        assert!(SourceText::new(text, Some(2), &budget).is_err());
        let empty_raw = json!([{"name":"point","kind":12,"range":raw[0]["selectionRange"],"selectionRange":raw[0]["selectionRange"]}]);
        let empty = parse(&empty_raw, Path::new("unused"), text, &budget).unwrap();
        assert_eq!(reader.excerpt(&empty, &empty.symbols[0], &budget).unwrap()["text"], "");
        budget.owner.cancel_with(asupersync::types::CancelKind::User, Some("stop indexing"));
        assert!(SourceText::new(text, Some(0), &budget).is_err());
    }
}
