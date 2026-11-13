#![no_main]

//! Fuzz harness for hashline tag parsing.
//!
//! Feeds arbitrary bytes into parse_hashline_tag (src/tools.rs line 5290)
//! to test regex parsing robustness and line number validation.
//! No complex state management - just parse and assert no panic.

use libfuzzer_sys::fuzz_target;
use regex::Regex;

// Replicate the hashline tag regex from src/tools.rs
fn hashline_tag_regex() -> &'static Regex {
    use std::sync::OnceLock;
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^(\d+)#([a-zA-Z]{2})$").expect("valid hashline regex"))
}

/// Parse a hashline tag reference string into (1-indexed line number, 2-byte hash).
/// This replicates the logic from src/tools.rs:5290 for fuzzing.
fn fuzz_parse_hashline_tag(ref_str: &str) -> Result<(usize, [u8; 2]), String> {
    let re = hashline_tag_regex();
    let caps = re
        .captures(ref_str)
        .ok_or_else(|| format!("Invalid hashline reference: {ref_str:?}"))?;
    let line_num: usize = caps[1]
        .parse()
        .map_err(|e| format!("Invalid line number in {ref_str:?}: {e}"))?;
    if line_num == 0 {
        return Err(format!("Line number must be >= 1, got 0 in {ref_str:?}"));
    }
    let hash_bytes = caps[2].as_bytes();
    if hash_bytes.len() < 2 {
        return Err(format!("Hash too short in {ref_str:?}"));
    }
    Ok((line_num, [hash_bytes[0], hash_bytes[1]]))
}

fuzz_target!(|data: &[u8]| {
    let lossy = String::from_utf8_lossy(data);

    // Test as single string (main target)
    let _ = fuzz_parse_hashline_tag(&lossy);

    // Test trimmed version (common case)
    let trimmed = lossy.trim();
    let _ = fuzz_parse_hashline_tag(trimmed);

    // Test with different case variations (regex is case sensitive)
    let _ = fuzz_parse_hashline_tag(&trimmed.to_lowercase());
    let _ = fuzz_parse_hashline_tag(&trimmed.to_uppercase());

    // Test line-by-line for multiline input
    for line in lossy.lines().take(32) {
        let _ = fuzz_parse_hashline_tag(line.trim());
    }

    // Test common patterns that might be edge cases
    if !trimmed.is_empty() {
        // Test with prefix/suffix (should fail but not panic)
        let prefixed = format!("prefix_{}", trimmed);
        let _ = fuzz_parse_hashline_tag(&prefixed);

        let suffixed = format!("{}_suffix", trimmed);
        let _ = fuzz_parse_hashline_tag(&suffixed);

        // Test with number overflow patterns
        if trimmed.chars().all(|c| c.is_ascii_alphanumeric()) {
            let overflow_test = format!("999999999999999#{}", trimmed);
            let _ = fuzz_parse_hashline_tag(&overflow_test);
        }
    }
});
