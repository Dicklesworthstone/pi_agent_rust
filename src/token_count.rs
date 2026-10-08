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

/// Counting surface (tests inject a deterministic stub).
pub trait TokenCounter: Send + Sync {
    fn count(&self, text: &str, table: TokenTable) -> u64;
}

/// Real BPE counting (feature `bpe-tokens`).
#[cfg(feature = "bpe-tokens")]
pub struct BpeCounter;

#[cfg(feature = "bpe-tokens")]
impl TokenCounter for BpeCounter {
    fn count(&self, text: &str, table: TokenTable) -> u64 {
        let bpe = match table {
            TokenTable::O200k => tiktoken_rs::o200k_base_singleton(),
            TokenTable::Cl100k => tiktoken_rs::cl100k_base_singleton(),
        };
        // These are message bodies, not a tokenizer's wire-format stream.
        // A literal <|endoftext|> in source, tool output, or a user message
        // must not collapse to one control token and understate the context
        // budget. The counting API also avoids retaining a token-ID vector
        // for every message during long-session compaction scans.
        bpe.count_ordinary(text) as u64
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
                    > tokenizer.encode_with_special_tokens(&repeated_markers).len() as u64
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
}
