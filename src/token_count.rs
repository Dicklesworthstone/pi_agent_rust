//! BPE token counting (bd-cv653.7.1).
//!
//! Real O200k (OpenAI) + Cl100k (Anthropic-approx) BPE counting via
//! tiktoken-rs replaces the chars/4 heuristic on the estimation path.
//! Hybrid accounting is preserved: measured API usage still wins when
//! present; BPE replaces ONLY the heuristic path; chars/4 stays as the
//! final fallback when the `bpe-tokens` feature is off (minimal builds).
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

/// Compressed table loader (bd-w8q6u): tiktoken-rs embeds the raw
/// `.tiktoken` text (o200k 3.6 MiB + cl100k 1.7 MiB) via `include_str!`,
/// which lands uncompressed in `__TEXT,__const`. We vendor the same tables
/// gzip-compressed (2.4 MiB total) and inflate once at first use, cutting
/// the bpe-tokens feature's binary cost roughly in half. Table bytes are
/// byte-identical to tiktoken-rs 0.12.0 `assets/`; the pattern strings and
/// special-token maps mirror `tiktoken_ext::openai_public` for that version.
#[cfg(feature = "bpe-tokens")]
mod compressed_tables {
    use std::io::Read;
    use std::sync::OnceLock;

    // tiktoken-rs's `HashMap` alias is rustc-hash's FxHashMap; match it.
    use rustc_hash::FxHashMap as HashMap;
    use tiktoken_rs::CoreBPE;

    const O200K_GZ: &[u8] = include_bytes!("../assets/tokenizer/o200k_base.tiktoken.gz");
    const CL100K_GZ: &[u8] = include_bytes!("../assets/tokenizer/cl100k_base.tiktoken.gz");

    /// cl100k pattern (tiktoken-rs 0.12.0 `openai_public::cl100k_base`).
    const CL100K_PAT: &str = "'(?i:[sdmt]|ll|ve|re)|[^\\r\\n\\p{L}\\p{N}]?+\\p{L}++|\\p{N}{1,3}+| ?[^\\s\\p{L}\\p{N}]++[\\r\\n]*+|\\s++$|\\s*[\\r\\n]|\\s+(?!\\S)|\\s";

    static O200K: OnceLock<CoreBPE> = OnceLock::new();
    static CL100K: OnceLock<CoreBPE> = OnceLock::new();

    fn inflate(gz: &[u8]) -> String {
        let mut decoder = flate2::read::GzDecoder::new(gz);
        let mut text = String::new();
        decoder
            .read_to_string(&mut text)
            .expect("vendored tokenizer table is valid gzip");
        text
    }

    fn parse_ranks(text: &str) -> HashMap<Vec<u8>, u32> {
        use base64::Engine;
        text.lines()
            .filter_map(|line| {
                let mut parts = line.split(' ');
                let raw = parts.next()?;
                let token = base64::engine::general_purpose::STANDARD.decode(raw).ok()?;
                let rank: u32 = parts.next()?.parse().ok()?;
                Some((token, rank))
            })
            .collect()
    }

    fn build(text: &str, pattern: &str, specials: &[(&str, u32)]) -> CoreBPE {
        let encoder = parse_ranks(text);
        let special_tokens_encoder: HashMap<String, u32> = specials
            .iter()
            .map(|(token, rank)| ((*token).to_string(), *rank))
            .collect();
        CoreBPE::new(encoder, special_tokens_encoder, pattern)
            .expect("vendored tokenizer table parses")
    }

    pub fn o200k() -> &'static CoreBPE {
        O200K.get_or_init(|| {
            build(
                &inflate(O200K_GZ),
                tiktoken_rs::O200K_BASE_PAT_STR,
                &[("<|endoftext|>", 199_999), ("<|endofprompt|>", 200_018)],
            )
        })
    }

    pub fn cl100k() -> &'static CoreBPE {
        CL100K.get_or_init(|| {
            build(
                &inflate(CL100K_GZ),
                CL100K_PAT,
                &[
                    ("<|endoftext|>", 100_257),
                    ("<|fim_prefix|>", 100_258),
                    ("<|fim_middle|>", 100_259),
                    ("<|fim_suffix|>", 100_260),
                    ("<|endofprompt|>", 100_276),
                ],
            )
        })
    }
}

#[cfg(feature = "bpe-tokens")]
impl TokenCounter for BpeCounter {
    fn count(&self, text: &str, table: TokenTable) -> u64 {
        let bpe = match table {
            TokenTable::O200k => compressed_tables::o200k(),
            TokenTable::Cl100k => compressed_tables::cl100k(),
        };
        bpe.encode_with_special_tokens(text).len() as u64
    }
}

/// chars/4 fallback (feature-off builds and the final fallback).
pub struct HeuristicCounter;

impl TokenCounter for HeuristicCounter {
    fn count(&self, text: &str, _table: TokenTable) -> u64 {
        (text.len() / 4) as u64
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
        // Reference vectors (tiktoken oracle):
        // "hello world" is 2 tokens in both families.
        let bpe = BpeCounter;
        assert_eq!(bpe.count("hello world", TokenTable::O200k), 2);
        assert_eq!(bpe.count("hello world", TokenTable::Cl100k), 2);
        // Code-heavy text counts materially above the chars/4 heuristic
        // on symbol-dense input (the motivation for the swap).
        let code = "fn main() { println!(\"{}\", 1 + 1); }";
        let bpe_count = bpe.count(code, TokenTable::O200k);
        let heuristic = HeuristicCounter.count(code, TokenTable::O200k);
        assert!(bpe_count > 0);
        assert!(bpe_count != heuristic, "BPE should diverge from chars/4");
    }

    #[test]
    fn heuristic_counts_quarters() {
        let counter = HeuristicCounter;
        assert_eq!(counter.count("abcd", TokenTable::O200k), 1);
        assert_eq!(counter.count("abcdefgh", TokenTable::Cl100k), 2);
    }

    #[cfg(feature = "bpe-tokens")]
    #[test]
    fn counting_1mb_fixture_is_fast() {
        let text = "lorem ipsum dolor sit amet ".repeat(40_000); // ~1.08 MB
        let bpe = BpeCounter;
        let start = std::time::Instant::now();
        let count = bpe.count(&text, TokenTable::O200k);
        let elapsed = start.elapsed();
        eprintln!("1MB BPE count: {count} tokens in {elapsed:?}");
        assert!(count > 100_000);
        // Debug-build bound (release is ~10x faster); still well under one
        // network round trip for a provider call.
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "1MB count took {elapsed:?} (must be negligible vs network latency)"
        );
    }
}
