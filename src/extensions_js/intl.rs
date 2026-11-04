//! Pure Unicode segmentation for the JavaScript Intl facade.
//!
//! The facade keeps the original JavaScript string and replaces lone UTF-16
//! surrogates only in the input used to classify boundaries. This module
//! receives valid Unicode text and returns UTF-16 offsets, so JavaScript can
//! slice the original input without losing code units. No filesystem,
//! environment, process, or network capabilities are involved.

use rquickjs::Ctx;
use rquickjs::function::Func;
use unicode_segmentation::UnicodeSegmentation as _;

fn grapheme_boundaries(input: &str) -> Vec<usize> {
    let mut boundaries = vec![0];
    let mut offset = 0;
    for grapheme in input.graphemes(true) {
        // Counting each cluster once keeps this linear. Rust byte offsets and
        // Unicode scalar counts are not JavaScript string indices.
        offset += grapheme.encode_utf16().count();
        boundaries.push(offset);
    }
    boundaries
}

pub(super) fn install(ctx: &Ctx<'_>) -> rquickjs::Result<()> {
    ctx.globals().set(
        "__pi_intl_grapheme_boundaries",
        Func::from(|input: String| grapheme_boundaries(&input)),
    )
}

#[cfg(test)]
mod tests {
    use super::{grapheme_boundaries, install};
    use crate::extensions_js::{PiJsRuntime, PiJsRuntimeConfig};
    use crate::scheduler::DeterministicClock;
    use serde_json::{Value, json};

    #[test]
    fn native_boundaries_include_empty_ascii_and_utf16_endpoints() {
        assert_eq!(grapheme_boundaries(""), [0]);
        assert_eq!(grapheme_boundaries("abc"), [0, 1, 2, 3]);
        assert_eq!(grapheme_boundaries("A😀B"), [0, 1, 3, 4]);
        assert_eq!(grapheme_boundaries("\r\nX"), [0, 2, 3]);
    }

    #[test]
    fn native_boundaries_keep_combining_and_joined_script_clusters() {
        assert_eq!(grapheme_boundaries("e\u{0301}X"), [0, 2, 3]);
        assert_eq!(grapheme_boundaries("\u{1100}\u{1161}X"), [0, 2, 3]);
        assert_eq!(grapheme_boundaries("किX"), [0, 2, 3]);
        assert_eq!(grapheme_boundaries("क्षX"), [0, 3, 4]);
    }

    #[test]
    fn native_boundaries_keep_emoji_clusters_and_pair_regional_indicators() {
        assert_eq!(grapheme_boundaries("🇺🇸X"), [0, 4, 5]);
        assert_eq!(grapheme_boundaries("🇺🇸🇨"), [0, 4, 6]);
        assert_eq!(grapheme_boundaries("👍🏽X"), [0, 4, 5]);
        assert_eq!(
            grapheme_boundaries("👨\u{200d}👩\u{200d}👧\u{200d}👦X"),
            [0, 11, 12]
        );
        assert_eq!(grapheme_boundaries("✈\u{fe0f}X"), [0, 2, 3]);
    }

    #[test]
    fn replacement_scalars_classify_boundaries_without_changing_utf16_offsets() {
        assert_eq!(grapheme_boundaries("\u{fffd}\u{0301}X"), [0, 2, 3]);
        assert_eq!(grapheme_boundaries("\u{fffd}\u{fffd}"), [0, 1, 2]);
        assert_eq!(grapheme_boundaries("A\u{fffd}\u{200d}😀B"), [0, 1, 3, 5, 6]);
    }

    #[test]
    fn native_callback_returns_a_javascript_array_of_numeric_offsets() {
        let runtime = rquickjs::Runtime::new().expect("QuickJS runtime");
        let context = rquickjs::Context::full(&runtime).expect("QuickJS context");
        context.with(|ctx| {
            install(&ctx).expect("install Unicode segmentation");
            let result: String = ctx
                .eval(
                    r"(() => {
                        const boundaries = __pi_intl_grapheme_boundaries('A😀B');
                        return JSON.stringify({
                            array: Array.isArray(boundaries),
                            numeric: boundaries.every(value => typeof value === 'number'),
                            boundaries,
                        });
                    })()",
                )
                .expect("native boundary callback");
            assert_eq!(
                serde_json::from_str::<Value>(&result).unwrap(),
                json!({"array":true, "numeric":true, "boundaries":[0, 1, 3, 4]})
            );
        });
    }

    fn evaluate_facade(script: &str) -> Value {
        futures::executor::block_on(async {
            let runtime = PiJsRuntime::with_clock_and_config(
                DeterministicClock::new(0),
                PiJsRuntimeConfig {
                    disk_cache_dir: None,
                    ..Default::default()
                },
            )
            .await
            .expect("Intl facade runtime");
            runtime.eval(script).await.expect("Intl facade probe");
            runtime
                .read_global_json("intlProbe")
                .await
                .expect("Intl facade results")
        })
    }

    #[test]
    fn pijs_grapheme_iteration_preserves_clusters_utf16_indices_and_input() {
        let result = evaluate_facade(
            r"
            const input = 'A👨\u200d👩\u200d👧\u200d👦e\u0301🇺🇸B';
            const segmenter = new Intl.Segmenter(undefined, {granularity:'grapheme'});
            const segments = segmenter.segment(input);
            const rows = Array.from(segments, row => [row.segment, row.index, row.input === input]);
            const first = segments[Symbol.iterator]();
            const second = segments[Symbol.iterator]();
            const firstRow = first.next().value;
            firstRow.segment = 'mutated';
            const secondRow = second.next().value;
            globalThis.intlProbe = {
                rows,
                repeated: Array.from(segments, row => row.index),
                firstNext: first.next().value.index,
                secondStart: secondRow.index,
                secondNext: second.next().value.index,
                freshRecord: secondRow.segment === 'A',
                empty: Array.from(segmenter.segment('')).length,
            };
        ",
        );
        assert_eq!(
            result,
            json!({
                "rows": [
                    ["A", 0, true], ["👨\u{200d}👩\u{200d}👧\u{200d}👦", 1, true],
                    ["e\u{0301}", 12, true], ["🇺🇸", 14, true], ["B", 18, true],
                ],
                "repeated": [0, 1, 12, 14, 18],
                "firstNext": 1, "secondStart": 0, "secondNext": 1,
                "freshRecord": true, "empty": 0,
            })
        );
    }

    #[test]
    fn pijs_grapheme_output_preserves_unpaired_surrogates_and_valid_pairs() {
        let result = evaluate_facade(
            r"
            const segmenter = new Intl.Segmenter();
            const inputs = ['\ud800\u0301X', '\udc00\u0301X', '\ud800\ud800', 'A\ud800\u200d😀B', '😀X'];
            globalThis.intlProbe = inputs.map(input => Array.from(segmenter.segment(input), row => ({
                units: Array.from({length:row.segment.length}, (_, index) => row.segment.charCodeAt(index)),
                index: row.index,
                sameInput: row.input === input,
            })));
        ",
        );
        assert_eq!(
            result,
            json!([
                [
                    {"units":[55296, 769], "index":0, "sameInput":true},
                    {"units":[88], "index":2, "sameInput":true},
                ],
                [
                    {"units":[56320, 769], "index":0, "sameInput":true},
                    {"units":[88], "index":2, "sameInput":true},
                ],
                [
                    {"units":[55296], "index":0, "sameInput":true},
                    {"units":[55296], "index":1, "sameInput":true},
                ],
                [
                    {"units":[65], "index":0, "sameInput":true},
                    {"units":[55296, 8205], "index":1, "sameInput":true},
                    {"units":[55357, 56832], "index":3, "sameInput":true},
                    {"units":[66], "index":5, "sameInput":true},
                ],
                [
                    {"units":[55357, 56832], "index":0, "sameInput":true},
                    {"units":[88], "index":2, "sameInput":true},
                ],
            ])
        );
    }
}
