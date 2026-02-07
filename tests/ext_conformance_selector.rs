//! Deterministic extension selector for the Rust-N/A pool (bd-2fps.1.1).
//!
//! Loads the validated manifest and conformance events, identifies extensions
//! with no Rust evidence (`overall_status` == "N/A"), and selects a
//! deterministic random subset given a seed, sample size, and optional filters.
#![allow(clippy::needless_raw_string_hashes)]

use std::path::Path;

// ---------------------------------------------------------------------------
// SplitMix64 PRNG — deterministic, no external deps
// ---------------------------------------------------------------------------

struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    const fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Returns a value in [0, n) using rejection sampling to avoid modulo bias.
    fn next_bounded(&mut self, n: u64) -> u64 {
        debug_assert!(n > 0);
        // Fast path for powers of two
        if n.is_power_of_two() {
            return self.next_u64() & (n - 1);
        }
        let threshold = n.wrapping_neg() % n; // (2^64 - n) % n
        loop {
            let r = self.next_u64();
            if r >= threshold {
                return r % n;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Deserialize)]
struct ManifestEntry {
    id: String,
    source_tier: String,
    conformance_tier: u32,
    #[allow(dead_code)]
    entry_path: String,
}

#[derive(Debug, serde::Deserialize)]
struct ValidatedManifest {
    extensions: Vec<ManifestEntry>,
}

#[derive(Debug, serde::Deserialize)]
struct ConformanceEvent {
    extension_id: String,
    overall_status: String,
}

// ---------------------------------------------------------------------------
// Filter
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
struct SelectionFilter {
    /// Inclusive conformance tier range (e.g., (1, 3) selects tiers 1, 2, 3).
    tier_range: Option<(u32, u32)>,
    /// Restrict to a specific source category (e.g., "community").
    source_category: Option<String>,
}

// ---------------------------------------------------------------------------
// Selector
// ---------------------------------------------------------------------------

struct ExtensionSelector {
    /// Sorted (by id) list of eligible extensions in the N/A pool.
    eligible: Vec<ManifestEntry>,
}

impl ExtensionSelector {
    /// Build the selector from the manifest and conformance events JSONL.
    fn from_files(manifest_path: &Path, events_path: &Path) -> Self {
        // Load manifest
        let manifest_data =
            std::fs::read_to_string(manifest_path).expect("read validated manifest");
        let manifest: ValidatedManifest =
            serde_json::from_str(&manifest_data).expect("parse manifest");

        // Load conformance events → set of IDs with Rust evidence
        let events_data = std::fs::read_to_string(events_path).expect("read conformance events");
        let mut passed_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        for line in events_data.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(evt) = serde_json::from_str::<ConformanceEvent>(line) {
                if evt.overall_status != "N/A" {
                    passed_ids.insert(evt.extension_id);
                }
            }
        }

        // Filter manifest to N/A pool (no Rust evidence)
        let mut eligible: Vec<ManifestEntry> = manifest
            .extensions
            .into_iter()
            .filter(|e| !passed_ids.contains(&e.id))
            .collect();

        // Sort by id for determinism
        eligible.sort_by(|a, b| a.id.cmp(&b.id));

        Self { eligible }
    }

    /// Build from pre-loaded data (for unit testing without filesystem).
    fn from_entries(entries: Vec<ManifestEntry>, passed_ids: &[&str]) -> Self {
        let passed: std::collections::HashSet<&str> = passed_ids.iter().copied().collect();
        let mut eligible: Vec<ManifestEntry> = entries
            .into_iter()
            .filter(|e| !passed.contains(e.id.as_str()))
            .collect();
        eligible.sort_by(|a, b| a.id.cmp(&b.id));
        Self { eligible }
    }

    /// Total number of eligible extensions in the N/A pool.
    fn pool_size(&self) -> usize {
        self.eligible.len()
    }

    /// Select a deterministic random subset.
    ///
    /// Returns an ordered list of extension IDs. The order is stable for the
    /// same (seed, `sample_size`, filter) triple.
    fn select(&self, seed: u64, sample_size: usize, filter: &SelectionFilter) -> Vec<String> {
        // Apply filter
        let filtered: Vec<&ManifestEntry> = self
            .eligible
            .iter()
            .filter(|e| {
                if let Some((min, max)) = filter.tier_range {
                    if e.conformance_tier < min || e.conformance_tier > max {
                        return false;
                    }
                }
                if let Some(ref cat) = filter.source_category {
                    if &e.source_tier != cat {
                        return false;
                    }
                }
                true
            })
            .collect();

        if filtered.is_empty() {
            return vec![];
        }

        let n = filtered.len();
        let actual_size = sample_size.min(n);

        // Fisher-Yates partial shuffle using deterministic PRNG
        let mut indices: Vec<usize> = (0..n).collect();
        let mut rng = SplitMix64::new(seed);
        for i in 0..actual_size {
            let span = u64::try_from(n - i).expect("shuffle span must fit u64");
            let offset = usize::try_from(rng.next_bounded(span)).expect("offset must fit usize");
            let j = i + offset;
            indices.swap(i, j);
        }

        // Return the first `actual_size` IDs in shuffled order
        indices[..actual_size]
            .iter()
            .map(|&idx| filtered[idx].id.clone())
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Helper: format selection for logging
// ---------------------------------------------------------------------------

fn format_selection_log(
    seed: u64,
    sample_size: usize,
    filter: &SelectionFilter,
    selected: &[String],
) -> String {
    serde_json::json!({
        "schema": "pi.ext.trial_selection.v1",
        "seed": seed,
        "requested_sample_size": sample_size,
        "actual_sample_size": selected.len(),
        "filter": {
            "tier_range": filter.tier_range,
            "source_category": filter.source_category,
        },
        "selected_ids": selected,
    })
    .to_string()
}

// ===========================================================================
// Tests
// ===========================================================================

fn make_entry(id: &str, source_tier: &str, conformance_tier: u32) -> ManifestEntry {
    ManifestEntry {
        id: id.to_string(),
        source_tier: source_tier.to_string(),
        conformance_tier,
        entry_path: format!("artifacts/{id}/index.ts"),
    }
}

// ---------------------------------------------------------------------------
// Deterministic stability: same seed → same selection
// ---------------------------------------------------------------------------

#[test]
fn selector_deterministic_stability() {
    let entries = vec![
        make_entry("alpha", "community", 1),
        make_entry("bravo", "community", 2),
        make_entry("charlie", "npm-registry", 1),
        make_entry("delta", "npm-registry", 3),
        make_entry("echo", "official", 1),
        make_entry("foxtrot", "community", 2),
        make_entry("golf", "third-party", 4),
        make_entry("hotel", "community", 1),
        make_entry("india", "npm-registry", 2),
        make_entry("juliet", "official", 3),
    ];

    let selector = ExtensionSelector::from_entries(entries, &["echo", "juliet"]);
    assert_eq!(selector.pool_size(), 8); // 10 - 2 passed

    let filter = SelectionFilter::default();
    let run1 = selector.select(42, 4, &filter);
    let run2 = selector.select(42, 4, &filter);
    let run3 = selector.select(42, 4, &filter);

    assert_eq!(run1, run2, "same seed must produce identical results");
    assert_eq!(run2, run3, "same seed must produce identical results");
    assert_eq!(run1.len(), 4);
}

// ---------------------------------------------------------------------------
// Different seeds produce different selections
// ---------------------------------------------------------------------------

#[test]
fn selector_different_seeds() {
    let entries: Vec<_> = (0..50)
        .map(|i| make_entry(&format!("ext-{i:03}"), "community", (i % 5) + 1))
        .collect();

    let selector = ExtensionSelector::from_entries(entries, &[]);
    let filter = SelectionFilter::default();

    let a = selector.select(1, 10, &filter);
    let b = selector.select(2, 10, &filter);
    let c = selector.select(999, 10, &filter);

    // Very unlikely all three are identical with different seeds
    assert!(
        a != b || b != c,
        "different seeds should produce different selections"
    );
}

// ---------------------------------------------------------------------------
// Tier filter
// ---------------------------------------------------------------------------

#[test]
fn selector_filter_by_tier() {
    let entries = vec![
        make_entry("t1-a", "community", 1),
        make_entry("t1-b", "community", 1),
        make_entry("t2-a", "community", 2),
        make_entry("t3-a", "community", 3),
        make_entry("t4-a", "community", 4),
        make_entry("t5-a", "community", 5),
    ];

    let selector = ExtensionSelector::from_entries(entries, &[]);

    // Only tiers 1-2
    let filter = SelectionFilter {
        tier_range: Some((1, 2)),
        ..Default::default()
    };
    let selected = selector.select(42, 100, &filter);
    assert_eq!(selected.len(), 3); // t1-a, t1-b, t2-a

    // Only tier 3
    let filter = SelectionFilter {
        tier_range: Some((3, 3)),
        ..Default::default()
    };
    let selected = selector.select(42, 100, &filter);
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0], "t3-a");
}

// ---------------------------------------------------------------------------
// Source category filter
// ---------------------------------------------------------------------------

#[test]
fn selector_filter_by_source() {
    let entries = vec![
        make_entry("com-a", "community", 1),
        make_entry("com-b", "community", 2),
        make_entry("npm-a", "npm-registry", 1),
        make_entry("npm-b", "npm-registry", 3),
        make_entry("tp-a", "third-party", 2),
    ];

    let selector = ExtensionSelector::from_entries(entries, &[]);

    let filter = SelectionFilter {
        source_category: Some("community".to_string()),
        ..Default::default()
    };
    let selected = selector.select(42, 100, &filter);
    assert_eq!(selected.len(), 2);
    for id in &selected {
        assert!(id.starts_with("com-"), "expected community: {id}");
    }

    let filter = SelectionFilter {
        source_category: Some("npm-registry".to_string()),
        ..Default::default()
    };
    let selected = selector.select(42, 100, &filter);
    assert_eq!(selected.len(), 2);
    for id in &selected {
        assert!(id.starts_with("npm-"), "expected npm: {id}");
    }
}

// ---------------------------------------------------------------------------
// Combined filter (tier + source)
// ---------------------------------------------------------------------------

#[test]
fn selector_combined_filter() {
    let entries = vec![
        make_entry("com-t1", "community", 1),
        make_entry("com-t2", "community", 2),
        make_entry("com-t3", "community", 3),
        make_entry("npm-t1", "npm-registry", 1),
        make_entry("npm-t2", "npm-registry", 2),
    ];

    let selector = ExtensionSelector::from_entries(entries, &[]);

    let filter = SelectionFilter {
        tier_range: Some((1, 2)),
        source_category: Some("community".to_string()),
    };
    let selected = selector.select(42, 100, &filter);
    assert_eq!(selected.len(), 2);
    for id in &selected {
        assert!(id.starts_with("com-"), "expected community: {id}");
    }
}

// ---------------------------------------------------------------------------
// Sample size capped at pool size
// ---------------------------------------------------------------------------

#[test]
fn selector_sample_capped() {
    let entries = vec![
        make_entry("a", "community", 1),
        make_entry("b", "community", 1),
        make_entry("c", "community", 1),
    ];

    let selector = ExtensionSelector::from_entries(entries, &[]);
    let filter = SelectionFilter::default();
    let selected = selector.select(42, 100, &filter);
    assert_eq!(selected.len(), 3);
}

// ---------------------------------------------------------------------------
// Empty pool
// ---------------------------------------------------------------------------

#[test]
fn selector_empty_pool() {
    let entries = vec![make_entry("a", "community", 1)];
    let selector = ExtensionSelector::from_entries(entries, &["a"]);
    assert_eq!(selector.pool_size(), 0);
    let selected = selector.select(42, 10, &SelectionFilter::default());
    assert!(selected.is_empty());
}

// ---------------------------------------------------------------------------
// Empty filter results
// ---------------------------------------------------------------------------

#[test]
fn selector_no_match_filter() {
    let entries = vec![
        make_entry("a", "community", 1),
        make_entry("b", "community", 2),
    ];
    let selector = ExtensionSelector::from_entries(entries, &[]);
    let filter = SelectionFilter {
        source_category: Some("nonexistent".to_string()),
        ..Default::default()
    };
    let selected = selector.select(42, 10, &filter);
    assert!(selected.is_empty());
}

// ---------------------------------------------------------------------------
// All IDs unique in selection
// ---------------------------------------------------------------------------

#[test]
fn selector_no_duplicates() {
    let entries: Vec<_> = (0..100)
        .map(|i| make_entry(&format!("ext-{i:03}"), "community", (i % 5) + 1))
        .collect();

    let selector = ExtensionSelector::from_entries(entries, &[]);
    let filter = SelectionFilter::default();
    let selected = selector.select(12345, 50, &filter);

    let unique: std::collections::HashSet<&String> = selected.iter().collect();
    assert_eq!(
        unique.len(),
        selected.len(),
        "selection must not contain duplicates"
    );
}

// ---------------------------------------------------------------------------
// Selection log format
// ---------------------------------------------------------------------------

#[test]
fn selector_log_format() {
    let entries = vec![
        make_entry("a", "community", 1),
        make_entry("b", "community", 2),
    ];
    let selector = ExtensionSelector::from_entries(entries, &[]);
    let filter = SelectionFilter {
        tier_range: Some((1, 2)),
        ..Default::default()
    };
    let selected = selector.select(42, 2, &filter);
    let log = format_selection_log(42, 2, &filter, &selected);

    let parsed: serde_json::Value = serde_json::from_str(&log).expect("valid JSON");
    assert_eq!(parsed["schema"], "pi.ext.trial_selection.v1");
    assert_eq!(parsed["seed"], 42);
    assert_eq!(parsed["requested_sample_size"], 2);
    assert_eq!(parsed["actual_sample_size"], 2);
    assert!(parsed["selected_ids"].is_array());
}

// ---------------------------------------------------------------------------
// Integration: load real files (if they exist)
// ---------------------------------------------------------------------------

#[test]
fn selector_from_real_files() {
    let manifest_path = Path::new("tests/ext_conformance/VALIDATED_MANIFEST.json");
    let events_path = Path::new("tests/ext_conformance/reports/conformance_events.jsonl");

    if !manifest_path.exists() || !events_path.exists() {
        eprintln!("Skipping real file test: conformance files not found");
        return;
    }

    let selector = ExtensionSelector::from_files(manifest_path, events_path);

    // We know from the summary: 158 N/A + 60 PASS = 218 total
    assert!(selector.pool_size() > 0, "N/A pool should not be empty");

    // Select 10 with default filter
    let filter = SelectionFilter::default();
    let selected = selector.select(42, 10, &filter);

    // Verify stability
    let selected2 = selector.select(42, 10, &filter);
    assert_eq!(selected, selected2, "same seed must be stable");

    // Log the selection
    let log = format_selection_log(42, 10, &filter, &selected);
    eprintln!("Selection log:\n{log}");

    // Verify all selected are unique
    let unique: std::collections::HashSet<&String> = selected.iter().collect();
    assert_eq!(unique.len(), selected.len());

    // Verify community filter works
    let community_filter = SelectionFilter {
        source_category: Some("community".to_string()),
        ..Default::default()
    };
    let community = selector.select(42, 5, &community_filter);
    assert!(!community.is_empty(), "community pool should not be empty");
}

// ---------------------------------------------------------------------------
// PRNG distribution sanity (statistical)
// ---------------------------------------------------------------------------

#[test]
fn prng_distribution_sanity() {
    let mut rng = SplitMix64::new(0);
    let n: u32 = 10_000;
    let buckets: usize = 10;
    let mut counts = vec![0u32; buckets];
    let bucket_modulus = u64::try_from(buckets).expect("bucket count must fit u64");

    for _ in 0..n {
        let bucket =
            usize::try_from(rng.next_bounded(bucket_modulus)).expect("bucket index must fit usize");
        counts[bucket] += 1;
    }

    // Each bucket should have ~1000 hits. Allow 30% deviation.
    let expected = f64::from(n) / f64::from(u32::try_from(buckets).expect("buckets must fit u32"));
    for (i, &count) in counts.iter().enumerate() {
        let deviation = (f64::from(count) - expected).abs() / expected;
        assert!(
            deviation < 0.3,
            "bucket {i} has {count} (expected ~{expected:.0}), deviation {deviation:.2}"
        );
    }
}
