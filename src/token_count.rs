//! BPE token counting (bd-cv653.7.1).
//!
//! Real O200k (OpenAI) + Cl100k (Anthropic-approx) BPE counting via
//! tiktoken-rs replaces the bytes/4 heuristic on the estimation path.
//! Hybrid accounting is preserved: measured API usage still wins when
//! present; BPE replaces ONLY the heuristic path. Rounded-up bytes/4 is
//! the approximate fallback when `bpe-tokens` is off (minimal builds).
//!
//! Table selection: anthropic → Cl100k-class, everything else → O200k
//! (documented approximation for non-OpenAI providers — Cl100k and O200k
//! diverge mostly on code-token frequencies, so O200k is the safer default
//! for OpenAI-compatible hosts).

/// Token table families.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenTable {
    O200k,
    Cl100k,
}

impl TokenTable {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::O200k => "o200k",
            Self::Cl100k => "cl100k",
        }
    }
}

/// Pick the table for a provider id (anthropic → Cl100k-class; everything
/// else → O200k, the documented approximation).
#[must_use]
pub const fn table_for_provider(provider: &str) -> TokenTable {
    if provider.eq_ignore_ascii_case("anthropic") {
        TokenTable::Cl100k
    } else {
        TokenTable::O200k
    }
}

/// Pick a table for the model that will read the complete request, including
/// messages originally produced by other providers. Gateway-qualified IDs
/// retain their model family; unknown deployments use the provider's existing
/// approximation. These tables estimate text, not provider framing or images.
#[must_use]
pub fn table_for_model(provider: &str, model_id: &str) -> TokenTable {
    let model = model_id.rsplit('/').next().unwrap_or(model_id);
    let model = model.to_ascii_lowercase();
    if model == "gpt-4"
        || model.starts_with("gpt-4-")
        || model == "gpt-3.5-turbo"
        || model.starts_with("gpt-3.5-turbo-")
        || model == "gpt-35-turbo"
        || model.starts_with("gpt-35-turbo-")
        || model.starts_with("claude-")
        || model.starts_with("anthropic.claude-")
        || model.contains(".anthropic.claude-")
    {
        TokenTable::Cl100k
    } else {
        table_for_provider(provider)
    }
}

/// Prewarm the active model's BPE table before serving latency-sensitive
/// session requests on an already-populated transcript.
pub fn prewarm_for_model(provider: &str, model_id: &str) {
    #[cfg(feature = "bpe-tokens")]
    {
        match table_for_model(provider, model_id) {
            TokenTable::O200k => {
                let _ = tiktoken_rs::o200k_base_singleton();
            }
            TokenTable::Cl100k => {
                let _ = tiktoken_rs::cl100k_base_singleton();
            }
        }
    }
    #[cfg(not(feature = "bpe-tokens"))]
    {
        let _ = (provider, model_id);
    }
}

/// Count text using the destination model's tokenizer approximation.
#[must_use]
pub fn count_tokens_for_model(text: &str, provider: &str, model_id: &str) -> u64 {
    active_counter().count(text, table_for_model(provider, model_id))
}

/// Counting surface (tests inject a deterministic stub).
pub trait TokenCounter: Send + Sync {
    fn count(&self, text: &str, table: TokenTable) -> u64;
}

/// Real BPE counting (feature `bpe-tokens`).
#[cfg(feature = "bpe-tokens")]
pub struct BpeCounter;

#[cfg(feature = "bpe-tokens")]
const BPE_CACHE_SLOTS: usize = 256;
#[cfg(feature = "bpe-tokens")]
const BPE_CACHE_MIN_BYTES: usize = 64;
#[cfg(feature = "bpe-tokens")]
const BPE_CACHE_MAX_BYTES: usize = 16 * 1024;
#[cfg(feature = "bpe-tokens")]
const BPE_CHUNK_TARGET_BYTES: usize = 512;
#[cfg(feature = "bpe-tokens")]
const BPE_LONG_RUN_PREFIX_FLUSH_BYTES: usize = 128;

#[cfg(feature = "bpe-tokens")]
struct CachedBpeChunk {
    hash: u64,
    table: TokenTable,
    text: Box<str>,
    tokens: u64,
}

#[cfg(feature = "bpe-tokens")]
std::thread_local! {
    static BPE_CHUNK_CACHE: std::cell::RefCell<Vec<Option<CachedBpeChunk>>> =
        std::cell::RefCell::new((0..BPE_CACHE_SLOTS).map(|_| None).collect());
}

/// Returns `true` when `prev` (at `i - 1`) and `next` (at `i`) form a proven
/// exact regex-piece boundary in both `cl100k_base` and `o200k_base`:
/// - `prev` is ASCII `[A-Za-z0-9]` (`\S`), so no earlier whitespace lookahead
///   (`\s+(?!\S)`) or end-of-input anchor (`\s++$`) can change in the left half.
/// - `next` is 7-bit ASCII non-alphanumeric other than `'`, so neither `\p{N}`,
///   `\p{L}\p{M}*`, nor English contractions (`'s`, `'ll`, ...) can continue
///   across `i`, and neither tokenizer pattern contains lookbehind.
#[cfg(feature = "bpe-tokens")]
#[inline]
const fn is_exact_bpe_split(prev: u8, next: u8) -> bool {
    prev.is_ascii_alphanumeric() && next < 0x80 && !next.is_ascii_alphanumeric() && next != b'\''
}

#[cfg(feature = "bpe-tokens")]
#[inline]
fn hash_bpe_chunk(chunk: &str, table: TokenTable) -> u64 {
    use std::hash::{DefaultHasher, Hash as _, Hasher as _};
    let mut hasher = DefaultHasher::new();
    table.as_str().hash(&mut hasher);
    chunk.hash(&mut hasher);
    hasher.finish()
}

#[cfg(feature = "bpe-tokens")]
fn count_bpe_chunk_cached(bpe: &tiktoken_rs::CoreBPE, chunk: &str, table: TokenTable) -> u64 {
    if chunk.is_empty() {
        return 0;
    }
    if !(BPE_CACHE_MIN_BYTES..=BPE_CACHE_MAX_BYTES).contains(&chunk.len()) {
        return bpe.count_ordinary(chunk) as u64;
    }
    let hash = hash_bpe_chunk(chunk, table);
    let slot_idx = usize::try_from(hash % (BPE_CACHE_SLOTS as u64)).unwrap_or(0);
    let cached = BPE_CHUNK_CACHE.with(|cache| {
        let borrow = cache.borrow();
        borrow
            .get(slot_idx)
            .and_then(Option::as_ref)
            .filter(|entry| entry.hash == hash && entry.table == table && &*entry.text == chunk)
            .map(|entry| entry.tokens)
    });
    if let Some(tokens) = cached {
        return tokens;
    }
    let tokens = bpe.count_ordinary(chunk) as u64;
    BPE_CHUNK_CACHE.with(|cache| {
        if let Ok(mut borrow) = cache.try_borrow_mut()
            && let Some(slot) = borrow.get_mut(slot_idx)
        {
            *slot = Some(CachedBpeChunk {
                hash,
                table,
                text: chunk.into(),
                tokens,
            });
        }
    });
    tokens
}

#[cfg(feature = "bpe-tokens")]
impl TokenCounter for BpeCounter {
    fn count(&self, text: &str, table: TokenTable) -> u64 {
        if text.is_empty() {
            return 0;
        }
        let bpe = match table {
            TokenTable::O200k => tiktoken_rs::o200k_base_singleton(),
            TokenTable::Cl100k => tiktoken_rs::cl100k_base_singleton(),
        };
        // These are message bodies, not a tokenizer's wire-format stream.
        // A literal <|endoftext|> in source, tool output, or a user message
        // must not collapse to one control token and understate the context
        // budget. The counting API also avoids retaining a token-ID vector
        // for every message during long-session compaction scans.
        if text.len() < BPE_CACHE_MIN_BYTES {
            return bpe.count_ordinary(text) as u64;
        }

        let bytes = text.as_bytes();
        let mut total = 0_u64;
        let mut start = 0_usize;
        let mut last_split = None::<usize>;

        for (pair_idx, pair) in bytes.windows(2).enumerate() {
            let &[prev, next] = pair else {
                continue;
            };
            let idx = pair_idx + 1;
            if is_exact_bpe_split(prev, next) {
                if idx - start >= BPE_CHUNK_TARGET_BYTES {
                    if let Some(chunk) = text.get(start..idx) {
                        total = total.saturating_add(count_bpe_chunk_cached(bpe, chunk, table));
                    }
                    start = idx;
                    last_split = None;
                } else {
                    last_split = Some(idx);
                }
            } else if let Some(split) = last_split
                && idx - split >= BPE_LONG_RUN_PREFIX_FLUSH_BYTES
            {
                if let Some(chunk) = text.get(start..split) {
                    total = total.saturating_add(count_bpe_chunk_cached(bpe, chunk, table));
                }
                start = split;
                last_split = None;
            }
        }

        if let Some(tail) = text.get(start..)
            && !tail.is_empty()
        {
            total = total.saturating_add(count_bpe_chunk_cached(bpe, tail, table));
        }
        total
    }
}

/// Rounded-up bytes/4 fallback for feature-off builds.
///
/// This is an approximation, not a guaranteed upper bound on BPE tokens.
/// Rounding each nonempty message up prevents short messages from becoming
/// invisible to the context budget. `div_ceil` avoids addition overflow.
pub struct HeuristicCounter;

impl TokenCounter for HeuristicCounter {
    fn count(&self, text: &str, _table: TokenTable) -> u64 {
        text.len().div_ceil(4) as u64
    }
}

/// The active counter for this build.
#[must_use]
pub fn active_counter() -> &'static dyn TokenCounter {
    #[cfg(feature = "bpe-tokens")]
    {
        static COUNTER: BpeCounter = BpeCounter;
        &COUNTER
    }
    #[cfg(not(feature = "bpe-tokens"))]
    {
        static COUNTER: HeuristicCounter = HeuristicCounter;
        &COUNTER
    }
}

/// Count text with the active counter for a provider family.
#[must_use]
pub fn count_tokens(text: &str, provider: &str) -> u64 {
    active_counter().count(text, table_for_provider(provider))
}

/// Per-table counts for `pi token` output.
#[must_use]
pub fn count_all_tables(text: &str) -> Vec<(TokenTable, u64)> {
    [TokenTable::O200k, TokenTable::Cl100k]
        .into_iter()
        .map(|table| (table, active_counter().count(text, table)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_table_distinguishes_legacy_gpt_from_modern_and_gateway_models() {
        for model in [
            "gpt-4",
            "gpt-4-0613",
            "gpt-4-32k",
            "gpt-4-turbo-2024-04-09",
            "gpt-3.5-turbo",
            "gpt-35-turbo-0125",
            "openai/gpt-4-turbo",
            "gateway/openai/GPT-3.5-TURBO",
            "anthropic/claude-sonnet-4-6",
            "anthropic.claude-sonnet-4-6-v1:0",
            "us.anthropic.claude-sonnet-4-6-v1:0",
        ] {
            assert_eq!(
                table_for_model("gateway", model),
                TokenTable::Cl100k,
                "{model}"
            );
        }
        for model in [
            "gpt-4o",
            "gpt-4o-mini",
            "gpt-4.1",
            "gpt-4.5-preview",
            "gpt-5",
            "openai/gpt-4o-2024-08-06",
            "gemini-3-flash",
            "grok-4",
            "unknown-deployment",
            "",
        ] {
            assert_eq!(
                table_for_model("gateway", model),
                TokenTable::O200k,
                "{model}"
            );
        }
        assert_eq!(
            table_for_model("anthropic", "private-deployment"),
            TokenTable::Cl100k
        );
    }

    #[test]
    fn model_counting_uses_the_selected_table_for_plain_text() {
        let text = "お誕生日おめでとう <|endoftext|> source/path.rs";
        for (model, table) in [
            ("gpt-4-turbo", TokenTable::Cl100k),
            ("gpt-4o", TokenTable::O200k),
        ] {
            assert_eq!(
                count_tokens_for_model(text, "openai", model),
                active_counter().count(text, table),
            );
        }
    }

    #[test]
    fn table_selection() {
        assert_eq!(table_for_provider("anthropic"), TokenTable::Cl100k);
        assert_eq!(table_for_provider("Anthropic"), TokenTable::Cl100k);
        assert_eq!(table_for_provider("openai"), TokenTable::O200k);
        assert_eq!(table_for_provider("ollama"), TokenTable::O200k);
    }

    #[cfg(feature = "bpe-tokens")]
    #[test]
    fn bpe_counts_reference_vectors() {
        let bpe = BpeCounter;
        for table in [TokenTable::O200k, TokenTable::Cl100k] {
            assert_eq!(bpe.count("", table), 0);
            assert_eq!(bpe.count("hello world", table), 2);
        }
    }

    #[cfg(feature = "bpe-tokens")]
    #[test]
    fn bpe_counts_message_bodies_as_ordinary_text() {
        let repeated_markers = "<|endoftext|>".repeat(1_024);
        for table in [TokenTable::O200k, TokenTable::Cl100k] {
            let tokenizer = match table {
                TokenTable::O200k => tiktoken_rs::o200k_base_singleton(),
                TokenTable::Cl100k => tiktoken_rs::cl100k_base_singleton(),
            };
            for text in [
                "<|endoftext|>",
                "<|fim_prefix|>literal source<|fim_suffix|><|fim_middle|>",
                "fn main() { println!(\"<|endoftext|>\"); }",
                "工具返回：你好世界 🦀\n<|endoftext|>",
                repeated_markers.as_str(),
            ] {
                assert_eq!(
                    BpeCounter.count(text, table),
                    tokenizer.encode_ordinary(text).len() as u64,
                    "literal counting mismatch for {table:?}"
                );
            }
            // The old special-token path reports exactly 1,024 tokens here,
            // even though the provider receives the full literal spellings.
            assert!(
                BpeCounter.count(&repeated_markers, table)
                    > tokenizer
                        .encode_with_special_tokens(&repeated_markers)
                        .len() as u64
            );
        }
    }

    #[test]
    fn heuristic_counts_partial_quarters_without_losing_messages() {
        let counter = HeuristicCounter;
        for table in [TokenTable::O200k, TokenTable::Cl100k] {
            for (text, expected) in [
                ("", 0),
                ("a", 1),
                ("ab", 1),
                ("abc", 1),
                ("abcd", 1),
                ("abcde", 2),
                ("abcdefgh", 2),
                ("abcdefghi", 3),
                ("中", 1),
                ("🦀", 1),
                ("🦀a", 2),
            ] {
                assert_eq!(counter.count(text, table), expected, "{text:?}");
            }
            let tiny_messages: u64 = ["a", "b", "c"]
                .iter()
                .map(|text| counter.count(text, table))
                .sum();
            assert_eq!(tiny_messages, 3);
        }
    }

    #[test]
    fn public_counting_paths_use_the_active_counter() {
        let text = "source contains <|endoftext|> and 工具 🦀";
        assert_eq!(
            count_tokens(text, "anthropic"),
            active_counter().count(text, TokenTable::Cl100k)
        );
        assert_eq!(
            count_tokens(text, "openai"),
            active_counter().count(text, TokenTable::O200k)
        );
        assert_eq!(
            count_all_tables(text),
            vec![
                (TokenTable::O200k, count_tokens(text, "openai")),
                (TokenTable::Cl100k, count_tokens(text, "anthropic")),
            ]
        );
    }

    #[cfg(not(feature = "bpe-tokens"))]
    #[test]
    fn minimal_build_counts_nonempty_short_inputs() {
        assert_eq!(count_tokens("a", "openai"), 1);
        assert_eq!(count_tokens("中", "anthropic"), 1);
        assert_eq!(count_tokens("", "openai"), 0);
    }

    #[cfg(feature = "bpe-tokens")]
    #[test]
    fn counting_1mb_stays_linear_rather_than_merely_fast() {
        let text = "lorem ipsum dolor sit amet ".repeat(40_000); // ~1.08 MB
        let bpe = BpeCounter;
        let start = std::time::Instant::now();
        let count = bpe.count(&text, TokenTable::O200k);
        let elapsed = start.elapsed();
        eprintln!("1MB BPE count: {count} tokens in {elapsed:?}");
        assert!(count > 100_000);
        // A catastrophe guard, not a performance measurement. It exists to
        // catch tokenisation going super-linear, which on 1 MB would take
        // minutes rather than seconds; nothing in tests/perf covers
        // tokenisation, so this is the only guard there is.
        //
        // The bound was 2s and failed at 2.97s in a full `--lib` run on a host
        // at load average 50. That is a debug build sharing 14 cores with
        // ~10,000 other tests, so a two-second wall clock is a coin flip
        // rather than a property of the code. 30s keeps the regression it is
        // actually for and stops it reporting host load as a defect.
        assert!(
            elapsed < std::time::Duration::from_secs(30),
            "1MB count took {elapsed:?}; tokenisation is super-linear, not merely slow"
        );
    }

    #[cfg(feature = "bpe-tokens")]
    #[test]
    fn chunked_bpe_counter_matches_direct_tiktoken_exactly_across_edge_cases() {
        let long_x = "x".repeat(2_048);
        let long_y = "y".repeat(1_024);
        let tricky_cases = vec![
            format!("export-0-{long_x}"),
            format!("export-599-{long_x}"),
            format!("fork-target-0-{long_y}"),
            format!("fork-target-899-{long_y}"),
            "don't won't we'll they've I'm it's foo'r-bar foo're-baz 12345-abc   \r\n  \n\tend   "
                .repeat(12),
            "混合 Unicode 🦀 café naïve 's 999: ".repeat(30) + &long_y,
        ];
        for table in [TokenTable::O200k, TokenTable::Cl100k] {
            let tokenizer = match table {
                TokenTable::O200k => tiktoken_rs::o200k_base_singleton(),
                TokenTable::Cl100k => tiktoken_rs::cl100k_base_singleton(),
            };
            for sample in &tricky_cases {
                assert_eq!(
                    BpeCounter.count(sample, table),
                    tokenizer.count_ordinary(sample) as u64,
                    "exact parity mismatch for {table:?}"
                );
            }
        }
    }
}
