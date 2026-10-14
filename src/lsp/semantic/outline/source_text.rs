//! Exact code excerpts from a validated, immutable document-symbol source.
//!
//! There is no file access here. The outline's one-pass endpoint map is reused
//! even for thousands of duplicate/nested definitions. Selection ranges never
//! stand in for implementation ranges, and clipped output remains explicit.

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

pub(super) fn excerpt(
    text: &str,
    outline: &Outline<'_>,
    symbol: &Symbol<'_>,
    budget: &Budget,
) -> Result<Value> {
    excerpt_with_limit(text, outline, symbol, MAX_EXCERPT_BYTES, budget)
}

fn excerpt_with_limit(
    text: &str,
    outline: &Outline<'_>,
    symbol: &Symbol<'_>,
    limit: usize,
    budget: &Budget,
) -> Result<Value> {
    budget.remaining()?;
    let offset = |position: Position| {
        outline.offsets.get(&position).copied()
            .ok_or_else(|| malformed("source excerpt lacks a validated endpoint"))
    };
    let start = offset(symbol.range.start)?;
    let end = offset(symbol.range.end)?;
    // Keep the slice operation fallible even though the outline established
    // these bounds. No internal cache inconsistency should become a panic.
    let full = text.get(start..end)
        .ok_or_else(|| malformed("source excerpt is outside the checked source"))?;
    let mut retained = full.len().min(limit);
    while !full.is_char_boundary(retained) {
        retained -= 1;
    }
    if retained > 0 && full.as_bytes().get(retained - 1) == Some(&b'\r')
        && full.as_bytes().get(retained) == Some(&b'\n')
    {
        retained -= 1;
    }
    let snippet = &full[..retained];
    let clipped_end = if retained == full.len() {
        symbol.range.end
    } else {
        end_position(symbol.range.start, snippet, budget)?
    };
    budget.remaining()?;
    Ok(json!({
        "text":snippet,
        "extent":if symbol.selection.is_some() { "document_symbol_range" } else { "symbol_location_range" },
        "range":{"start":symbol.range.start,"end":clipped_end},
        "byteRange":{"start":start,"end":start + retained},
        "totalBytes":full.len(),"omittedBytes":full.len() - retained,
        "truncated":retained < full.len()
    }))
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
        let rows = displayed_with_source(&outline, 0..64, MAX_DISPLAY, Some(&text), &budget).unwrap();
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
}
