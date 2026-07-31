#![no_main]

//! Fuzz harness for tool path normalization helpers.
//!
//! Focuses on traversal-ish, Unicode-heavy, and malformed path inputs while
//! asserting stable normalization invariants.

use libfuzzer_sys::fuzz_target;
use pi::fuzz_exports::{fuzz_normalize_dot_segments, fuzz_resolve_path};
use std::path::{Component, Path, PathBuf};

const MAX_INPUT_BYTES: usize = 4096;
const MAX_INPUT_CHARS: usize = 1024;

fn lossy_limited(input: &[u8]) -> String {
    String::from_utf8_lossy(input)
        .chars()
        .take(MAX_INPUT_CHARS)
        .collect()
}

fn assert_no_curdir(path: &Path) {
    assert!(
        path.components()
            .all(|component| !matches!(component, Component::CurDir))
    );
}

fn has_parent_dir(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, Component::ParentDir))
}

fuzz_target!(|data: &[u8]| {
    if data.is_empty() || data.len() > MAX_INPUT_BYTES {
        return;
    }

    let raw = lossy_limited(data);
    let cwd = PathBuf::from("/tmp/pi-agent-rust-fuzz-root");

    let resolved = fuzz_resolve_path(&raw, &cwd);
    let normalized_once = fuzz_normalize_dot_segments(&resolved);
    let normalized_twice = fuzz_normalize_dot_segments(&normalized_once);

    // Normalization must be idempotent.
    assert_eq!(normalized_once, normalized_twice);
    // `.` components should always be eliminated.
    assert_no_curdir(&normalized_once);

    let is_tilde = raw == "~" || raw.starts_with("~/");
    if Path::new(&raw).is_absolute() {
        assert!(resolved.is_absolute());
        assert!(normalized_once.is_absolute());
    } else if !is_tilde {
        // Resolution is purely lexical: the input is joined onto `cwd` and its
        // dot segments are collapsed, so `..` segments legitimately climb above
        // `cwd` (`".."` resolves to `/tmp`). Keeping reads and writes inside the
        // working directory is a scope-enforcement concern handled separately by
        // `enforce_read_scope`, not a normalization invariant. What resolution
        // must guarantee is that joining onto an absolute `cwd` always yields an
        // absolute, traversal-free path, and that inputs which never climb stay
        // under `cwd`.
        assert!(resolved.is_absolute());
        assert!(!has_parent_dir(&resolved));
        if !has_parent_dir(Path::new(&raw)) {
            assert!(resolved.starts_with(&cwd));
        }
    }

    // Also exercise prefixed relative wrapping commonly used by tools.
    let wrapped = format!("sandbox/{raw}");
    let wrapped_resolved = fuzz_resolve_path(&wrapped, &cwd);
    let wrapped_normalized = fuzz_normalize_dot_segments(&wrapped_resolved);
    assert_eq!(
        wrapped_normalized,
        fuzz_normalize_dot_segments(&wrapped_normalized)
    );
    assert_no_curdir(&wrapped_normalized);
});
