//! Byte spans, not reconstructed lines: only a selected marker block is removed.

use std::ops::Range;

use super::{Error, Result, fail};

#[derive(Debug, Clone)]
pub(super) struct Region {
    pub(super) span: Range<usize>,
    pub(super) ours: Range<usize>,
    pub(super) base: Option<Range<usize>>,
    pub(super) theirs: Range<usize>,
    pub(super) ours_label: String,
    pub(super) theirs_label: String,
}

impl Region {
    pub(super) fn chosen<'a>(&self, text: &'a str, side: &str) -> Result<&'a str> {
        let span = match side {
            "ours" => &self.ours,
            "theirs" => &self.theirs,
            "base" => self.base.as_ref().ok_or_else(|| {
                fail(
                    "BASE_UNAVAILABLE",
                    "this conflict has no diff3 base section",
                )
            })?,
            _ => return Err(fail("SIDE", "choose ours, theirs or base")),
        };
        text.get(span.clone()).ok_or_else(malformed)
    }
}

fn malformed() -> Error {
    fail(
        "MALFORMED",
        "ambiguous or incomplete conflict markers; no file was changed",
    )
}

/// Git labels follow whitespace. An operator or an extra marker character
/// without whitespace is ordinary content. Width is established by the opener.
fn marker(line: &str) -> Option<(u8, usize, &str)> {
    let kind = *line.as_bytes().first()?;
    if !matches!(kind, b'<' | b'|' | b'=' | b'>') {
        return None;
    }
    let width = line.bytes().take_while(|byte| *byte == kind).count();
    if width < 7 {
        return None;
    }
    let tail = &line[width..];
    if kind == b'=' {
        if !tail.is_empty() {
            return None;
        }
    } else if !tail.is_empty() && !tail.starts_with([' ', '\t']) {
        return None;
    }
    Some((kind, width, tail.trim()))
}

struct Open {
    start: usize,
    width: usize,
    ours_start: usize,
    ours_end: Option<usize>,
    base_start: Option<usize>,
    base_end: Option<usize>,
    theirs_start: Option<usize>,
    label: String,
}

pub(super) fn parse(text: &str, limit: usize) -> Result<Vec<Region>> {
    let mut regions = Vec::new();
    let mut open: Option<Open> = None;
    let mut offset = 0;
    for raw in text.split_inclusive('\n') {
        let line = raw.strip_suffix('\n').unwrap_or(raw);
        let line = line.strip_suffix('\r').unwrap_or(line);
        // Retain a UTF-8 BOM outside a marker which starts the first line.
        let line = if offset == 0 {
            line.strip_prefix('\u{feff}').unwrap_or(line)
        } else {
            line
        };
        let start = offset + usize::from(offset == 0 && raw.starts_with('\u{feff}')) * 3;
        let end = offset + raw.len();
        offset = end;
        let Some((kind, width, label)) = marker(line) else {
            continue;
        };
        if let Some(active) = &mut open {
            // Different widths may occur literally inside a Git conflict.
            if width != active.width {
                continue;
            }
            match kind {
                b'|' if active.base_start.is_none() && active.theirs_start.is_none() => {
                    active.ours_end = Some(start);
                    active.base_start = Some(end);
                }
                b'=' if active.theirs_start.is_none() => {
                    if active.base_start.is_some() {
                        active.base_end = Some(start);
                    } else {
                        active.ours_end = Some(start);
                    }
                    active.theirs_start = Some(end);
                }
                b'>' if active.theirs_start.is_some() => {
                    if regions.len() == limit {
                        return Err(fail("LIMIT", "too many conflict regions"));
                    }
                    let active = open.take().ok_or_else(malformed)?;
                    regions.push(Region {
                        span: active.start..end,
                        ours: active.ours_start..active.ours_end.ok_or_else(malformed)?,
                        base: active.base_start.zip(active.base_end).map(|(a, b)| a..b),
                        theirs: active.theirs_start.ok_or_else(malformed)?..start,
                        ours_label: active.label,
                        theirs_label: label.to_owned(),
                    });
                }
                _ => return Err(malformed()),
            }
        } else if kind == b'<' {
            open = Some(Open {
                start,
                width,
                ours_start: end,
                ours_end: None,
                base_start: None,
                base_end: None,
                theirs_start: None,
                label: label.to_owned(),
            });
        }
    }
    if open.is_some() {
        return Err(malformed());
    }
    Ok(regions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_splice_preserves_every_unselected_region_and_no_final_newline() {
        let block = "<<<<<<< ours\none\n=======\ntwo\n>>>>>>> theirs\n";
        let text = format!("prefix\n{block}between\n{block}tail");
        let regions = parse(&text, 10).unwrap();
        for (index, region) in regions.iter().enumerate() {
            let replacement = region.chosen(&text, "theirs").unwrap();
            let result = format!(
                "{}{}{}",
                &text[..region.span.start],
                replacement,
                &text[region.span.end..]
            );
            let expected = if index == 0 {
                format!("prefix\ntwo\nbetween\n{block}tail")
            } else {
                format!("prefix\n{block}between\ntwo\ntail")
            };
            assert_eq!(result, expected);
            assert_eq!(parse(&result, 10).unwrap().len(), 1);
        }
    }

    #[test]
    fn diff3_crlf_bom_and_empty_sides_keep_original_bytes() {
        let text = "\u{feff}<<<<<<<<<< ours\r\n|||||||||| base\r\nbase 😀\r\n==========\r\n雪\r\n>>>>>>>>>> theirs";
        let regions = parse(text, 1).unwrap();
        let region = &regions[0];
        assert_eq!(region.span.start, 3);
        assert_eq!(region.span.end, text.len());
        assert_eq!(region.chosen(text, "ours").unwrap(), "");
        assert_eq!(region.chosen(text, "base").unwrap(), "base 😀\r\n");
        assert_eq!(region.chosen(text, "theirs").unwrap(), "雪\r\n");
    }

    #[test]
    fn missing_base_is_not_an_empty_base() {
        let text = "<<<<<<< a\n=======\n>>>>>>> b\n";
        let region = &parse(text, 1).unwrap()[0];
        assert!(
            region
                .chosen(text, "base")
                .unwrap_err()
                .to_string()
                .contains("BASE_UNAVAILABLE")
        );
        assert_eq!(region.chosen(text, "theirs").unwrap(), "");
        let text = "<<<<<<< a\n||||||| b\n=======\n>>>>>>> c\n";
        assert_eq!(parse(text, 1).unwrap()[0].chosen(text, "base").unwrap(), "");
    }

    #[test]
    fn malformed_blocks_and_limits_never_return_a_resolvable_prefix() {
        for text in [
            "<<<<<<< a\nx",
            "<<<<<<< a\n>>>>>>> b\n",
            "<<<<<<< a\n<<<<<<< b\n=======\n>>>>>>> c\n",
            "<<<<<<< a\n=======\n||||||| b\n>>>>>>> c\n",
        ] {
            assert!(parse(text, 10).is_err(), "{text}");
        }
        let text = "<<<<<<< a\n=======\n>>>>>>> b\n";
        assert!(parse(text, 0).is_err());
        assert!(parse(&text.repeat(2), 1).is_err());
    }

    #[test]
    fn different_width_markers_and_nonmarker_operators_are_literal_content() {
        let text = "<<<<<<<not-a-marker\n<<<<<<<<<< a\n<<<<<<< literal\n=======\n==========\nvalue\n>>>>>>>>>> b\n";
        let regions = parse(text, 2).unwrap();
        assert_eq!(regions.len(), 1);
        assert_eq!(
            regions[0].chosen(text, "ours").unwrap(),
            "<<<<<<< literal\n=======\n"
        );
    }
}
