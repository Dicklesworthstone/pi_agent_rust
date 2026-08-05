//! Shared performance-build metadata helpers for benchmark tooling.
//!
//! These helpers keep profile and allocator reporting consistent across
//! benchmark binaries, regression tests, and shell harnesses.

use sha2::{Digest as _, Sha256};
use std::io::Read as _;
use std::path::Path;

/// Environment variable that overrides benchmark build-profile metadata.
pub const BENCH_BUILD_PROFILE_ENV: &str = "PI_BENCH_BUILD_PROFILE";

/// Environment variable that requests an allocator label for benchmark runs.
pub const BENCH_ALLOCATOR_ENV: &str = "PI_BENCH_ALLOCATOR";

/// Release binary-size budget (MB) shared by perf regression and budget gates.
pub const BINARY_SIZE_RELEASE_BUDGET_MB: f64 = 22.0;

/// Cargo profile family embedded by `build.rs` (`PROFILE`; custom release-derived
/// profiles are reported by Cargo as `release`).
pub const COMPILED_PROFILE_FAMILY: &str = env!("PI_BUILD_PROFILE_FAMILY");

/// Cargo optimization level embedded by `build.rs` (`OPT_LEVEL`).
pub const COMPILED_OPT_LEVEL: &str = env!("PI_BUILD_OPT_LEVEL");

/// Cargo debug-info switch embedded by `build.rs` (`DEBUG`).
pub const COMPILED_DEBUG: &str = env!("PI_BUILD_DEBUG");

/// Sorted, comma-separated package feature set embedded by `build.rs`.
pub const COMPILED_FEATURES_CSV: &str = env!("PI_BUILD_FEATURES");

/// Exact package feature set for the canonical shipping/system PiJS perf lane.
pub const CANONICAL_PIJS_PERF_FEATURES: &[&str] = &[
    "clipboard",
    "image",
    "image-resize",
    "sqlite-sessions",
    "wasm-host",
];

/// Versioned name for the authoritative Cargo build fingerprint contract.
pub const BUILD_FINGERPRINT_CONTRACT: &str = "cargo_build_fingerprint.v1";

/// Independent build assertions carried by benchmark provenance.
#[derive(Debug, Clone, Copy)]
pub struct BenchmarkBuildVerification {
    pub executable_profile: bool,
    pub build_fingerprint: bool,
    pub build_profile: bool,
}

/// Inputs covered by the benchmark provenance configuration hash.
#[derive(Debug, Clone, Copy)]
pub struct BenchmarkProvenance<'a> {
    pub source_commit: &'a str,
    pub source_dirty: bool,
    pub build_profile: &'a str,
    pub executable_build_profile: &'a str,
    pub verification: BenchmarkBuildVerification,
    pub build_fingerprint_contract: &'a str,
    pub compiled_profile_family: &'a str,
    pub compiled_opt_level: &'a str,
    pub compiled_debug: &'a str,
    pub compiled_features: &'a [&'a str],
    pub binary_path: &'a str,
    pub binary_sha256: &'a str,
    pub debug_assertions: bool,
}

/// Effective allocator compiled into the current binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocatorKind {
    /// The platform/system allocator.
    System,
    /// `tikv-jemallocator` via the `jemalloc` Cargo feature.
    Jemalloc,
}

impl AllocatorKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Jemalloc => "jemalloc",
        }
    }
}

/// Benchmark allocator selection metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllocatorSelection {
    /// Requested allocator token (normalized).
    pub requested: String,
    /// Source of `requested` (`env` or `default`).
    pub requested_source: &'static str,
    /// Effective allocator compiled into this binary.
    pub effective: AllocatorKind,
    /// Optional explanation when request/effective do not match.
    pub fallback_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestedAllocator {
    Auto,
    System,
    Jemalloc,
    Unknown,
}

/// Returns the allocator compiled into the current binary.
#[must_use]
pub const fn compiled_allocator() -> AllocatorKind {
    if cfg!(all(
        feature = "jemalloc",
        any(target_os = "linux", target_os = "macos")
    )) {
        AllocatorKind::Jemalloc
    } else {
        AllocatorKind::System
    }
}

/// Resolves benchmark allocator metadata from [`BENCH_ALLOCATOR_ENV`].
#[must_use]
pub fn resolve_bench_allocator() -> AllocatorSelection {
    let raw_value = std::env::var(BENCH_ALLOCATOR_ENV).ok();
    resolve_bench_allocator_from(raw_value.as_deref())
}

/// Resolves benchmark allocator metadata from an optional raw token.
#[must_use]
pub fn resolve_bench_allocator_from(raw_value: Option<&str>) -> AllocatorSelection {
    let requested_raw = raw_value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map_or_else(|| "auto".to_string(), str::to_ascii_lowercase);
    let requested_source = if raw_value
        .map(str::trim)
        .is_some_and(|value| !value.is_empty())
    {
        "env"
    } else {
        "default"
    };

    let requested_kind = match requested_raw.as_str() {
        "auto" | "default" => RequestedAllocator::Auto,
        "system" | "native" => RequestedAllocator::System,
        "jemalloc" | "je" => RequestedAllocator::Jemalloc,
        _ => RequestedAllocator::Unknown,
    };

    let effective = compiled_allocator();
    let fallback_reason = match requested_kind {
        RequestedAllocator::System if effective == AllocatorKind::Jemalloc => {
            Some("system requested but binary was built with --features jemalloc".to_string())
        }
        RequestedAllocator::Jemalloc if effective != AllocatorKind::Jemalloc => {
            Some("jemalloc requested but this target/build uses the system allocator".to_string())
        }
        RequestedAllocator::Unknown => Some(format!(
            "unknown allocator '{requested_raw}'; using compiled allocator '{}'",
            effective.as_str()
        )),
        RequestedAllocator::Auto | RequestedAllocator::System | RequestedAllocator::Jemalloc => {
            None
        }
    };

    let requested = match requested_kind {
        RequestedAllocator::System => "system".to_string(),
        RequestedAllocator::Jemalloc => "jemalloc".to_string(),
        RequestedAllocator::Auto => "auto".to_string(),
        RequestedAllocator::Unknown => requested_raw,
    };

    AllocatorSelection {
        requested,
        requested_source,
        effective,
        fallback_reason,
    }
}

/// Detects the benchmark build profile for reporting.
#[must_use]
pub fn detect_build_profile() -> String {
    let env_profile = std::env::var(BENCH_BUILD_PROFILE_ENV).ok();
    let current_exe = std::env::current_exe().ok();
    detect_build_profile_from(
        env_profile.as_deref(),
        current_exe.as_deref(),
        cfg!(debug_assertions),
    )
}

/// Detects build profile with injectable dependencies for tests.
#[must_use]
pub fn detect_build_profile_from(
    env_profile: Option<&str>,
    current_exe: Option<&Path>,
    debug_assertions: bool,
) -> String {
    if let Some(value) = env_profile.map(str::trim).filter(|value| !value.is_empty()) {
        return value.to_string();
    }

    if let Some(profile) = current_exe.and_then(profile_from_target_path) {
        return profile;
    }

    if debug_assertions {
        "debug".to_string()
    } else {
        "release".to_string()
    }
}

/// Returns the sorted package features compiled into this binary.
#[must_use]
pub fn compiled_feature_set() -> Vec<&'static str> {
    if COMPILED_FEATURES_CSV.is_empty() {
        Vec::new()
    } else {
        COMPILED_FEATURES_CSV.split(',').collect()
    }
}

/// Returns whether Cargo's authoritative build settings match the custom
/// `perf` profile fingerprint.
///
/// The profile's directory name is deliberately not trusted because Cargo
/// reports release-inheriting custom profiles through `PROFILE=release`.
#[must_use]
pub fn has_canonical_perf_build_fingerprint() -> bool {
    matches_canonical_perf_build_fingerprint(
        COMPILED_PROFILE_FAMILY,
        COMPILED_OPT_LEVEL,
        COMPILED_DEBUG,
    )
}

/// Checks an injected Cargo build fingerprint against the canonical `perf`
/// settings. This is public so evidence consumers and tests use one contract.
#[must_use]
pub fn matches_canonical_perf_build_fingerprint(
    profile_family: &str,
    opt_level: &str,
    debug: &str,
) -> bool {
    profile_family == "release" && opt_level == "3" && debug == "true"
}

/// Returns whether this binary has the exact package features used by the
/// canonical shipping/system PiJS performance lane.
#[must_use]
pub fn has_canonical_pijs_perf_features() -> bool {
    matches_canonical_pijs_perf_features(&compiled_feature_set())
}

/// Checks an injected sorted feature set against the canonical shipping/system
/// PiJS lane.
///
/// `image-resize` also enables the package's implicit `image` feature, so both
/// are intentionally present.
#[must_use]
pub fn matches_canonical_pijs_perf_features(features: &[&str]) -> bool {
    features == CANONICAL_PIJS_PERF_FEATURES
}

/// Computes the lowercase SHA-256 digest of a file without loading it all into
/// memory.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Hashes asserted build/source/binary provenance as compact canonical JSON.
///
/// Evidence producers and consumers share this helper so field omissions or
/// serialization-order drift fail closed.
#[must_use]
pub fn benchmark_provenance_config_hash(provenance: &BenchmarkProvenance<'_>) -> String {
    let canonical = serde_json::json!({
        "binary_path": provenance.binary_path,
        "binary_sha256": provenance.binary_sha256,
        "build_fingerprint_contract": provenance.build_fingerprint_contract,
        "build_fingerprint_verified": provenance.verification.build_fingerprint,
        "build_profile": provenance.build_profile,
        "build_profile_verified": provenance.verification.build_profile,
        "compiled_debug": provenance.compiled_debug,
        "compiled_features": provenance.compiled_features,
        "compiled_opt_level": provenance.compiled_opt_level,
        "compiled_profile_family": provenance.compiled_profile_family,
        "debug_assertions": provenance.debug_assertions,
        "executable_build_profile": provenance.executable_build_profile,
        "executable_profile_verified": provenance.verification.executable_profile,
        "source_commit": provenance.source_commit,
        "source_dirty": provenance.source_dirty,
    });
    let bytes = serde_json::to_vec(&canonical)
        .expect("benchmark provenance contains only JSON-serializable primitives");
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Attempts to derive the Cargo profile from an executable artifact layout.
///
/// This works with both the default `target/` directory and arbitrary
/// `CARGO_TARGET_DIR` values. Example/test artifacts live one level below the
/// profile in `examples/` or `deps/`; ordinary binaries live directly below it.
#[must_use]
pub fn profile_from_target_path(path: &Path) -> Option<String> {
    let artifact_parent = path.parent()?;
    let artifact_parent_name = artifact_parent.file_name()?.to_str()?;
    let profile_dir = if matches!(artifact_parent_name, "deps" | "examples") {
        artifact_parent.parent()?
    } else {
        artifact_parent
    };
    let candidate = profile_dir.file_name()?.to_str()?.trim();
    if candidate.is_empty() {
        return None;
    }

    Some(candidate.to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        AllocatorKind, BENCH_ALLOCATOR_ENV, BenchmarkBuildVerification, BenchmarkProvenance,
        benchmark_provenance_config_hash, detect_build_profile_from,
        matches_canonical_perf_build_fingerprint, matches_canonical_pijs_perf_features,
        profile_from_target_path, resolve_bench_allocator_from,
    };
    use std::path::Path;

    #[test]
    fn detect_build_profile_prefers_env_override() {
        let profile = detect_build_profile_from(Some("perf"), None, true);
        assert_eq!(profile, "perf");
    }

    #[test]
    fn detect_build_profile_from_target_path_detects_profile() {
        let path = Path::new("/tmp/repo/target/perf/pijs_workload");
        let profile = detect_build_profile_from(None, Some(path), true);
        assert_eq!(profile, "perf");
    }

    #[test]
    fn detect_build_profile_falls_back_to_debug_or_release() {
        assert_eq!(detect_build_profile_from(None, None, true), "debug");
        assert_eq!(detect_build_profile_from(None, None, false), "release");
    }

    #[test]
    fn profile_from_target_path_detects_release_deps_binary() {
        let path = Path::new("/tmp/repo/target/release/deps/pijs_workload-abc123");
        assert_eq!(profile_from_target_path(path).as_deref(), Some("release"));
    }

    #[test]
    fn profile_from_target_path_detects_perf_example_binary() {
        let path = Path::new("/tmp/repo/target/perf/examples/pijs_workload");
        assert_eq!(profile_from_target_path(path).as_deref(), Some("perf"));
    }

    #[test]
    fn profile_from_target_path_detects_cross_target_perf_example_binary() {
        let path =
            Path::new("/tmp/repo/target/x86_64-unknown-linux-gnu/perf/examples/pijs_workload");
        assert_eq!(profile_from_target_path(path).as_deref(), Some("perf"));
    }

    #[test]
    fn profile_from_target_path_does_not_misclassify_moved_binary_as_perf() {
        let path = Path::new("/tmp/repo/pijs_workload");
        let derived = profile_from_target_path(path);
        assert_eq!(derived.as_deref(), Some("repo"));
        assert_ne!(derived.as_deref(), Some("perf"));
    }

    #[test]
    fn profile_from_target_path_uses_direct_artifact_parent_as_profile_hint() {
        let path = Path::new("/tmp/repo/bin/pijs_workload");
        assert_eq!(profile_from_target_path(path).as_deref(), Some("bin"));
    }

    #[test]
    fn profile_from_target_path_supports_arbitrary_cargo_target_dir() {
        let path = Path::new("/tmp/pi-build/perf/examples/pijs_workload");
        assert_eq!(profile_from_target_path(path).as_deref(), Some("perf"));
    }

    #[test]
    fn canonical_perf_fingerprint_distinguishes_perf_from_release() {
        assert!(matches_canonical_perf_build_fingerprint(
            "release", "3", "true"
        ));
        assert!(!matches_canonical_perf_build_fingerprint(
            "release", "z", "false"
        ));
        assert!(!matches_canonical_perf_build_fingerprint(
            "release", "3", "false"
        ));
        assert!(!matches_canonical_perf_build_fingerprint(
            "perf", "3", "true"
        ));
    }

    #[test]
    fn canonical_pijs_features_include_transitively_enabled_image_feature() {
        let canonical = [
            "clipboard",
            "image",
            "image-resize",
            "sqlite-sessions",
            "wasm-host",
        ];
        assert!(matches_canonical_pijs_perf_features(&canonical));

        let missing_implicit_image = ["clipboard", "image-resize", "sqlite-sessions", "wasm-host"];
        assert!(!matches_canonical_pijs_perf_features(
            &missing_implicit_image
        ));
    }

    #[test]
    fn benchmark_provenance_hash_binds_every_asserted_field() {
        let features = ["clipboard", "image"];
        let canonical = BenchmarkProvenance {
            source_commit: "0123456789abcdef0123456789abcdef01234567",
            source_dirty: false,
            build_profile: "perf",
            executable_build_profile: "perf",
            verification: BenchmarkBuildVerification {
                executable_profile: true,
                build_fingerprint: true,
                build_profile: true,
            },
            build_fingerprint_contract: "cargo_build_fingerprint.v1",
            compiled_profile_family: "release",
            compiled_opt_level: "3",
            compiled_debug: "true",
            compiled_features: &features,
            binary_path: "/tmp/pi-build/perf/examples/pijs_workload",
            binary_sha256: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            debug_assertions: false,
        };
        let first = benchmark_provenance_config_hash(&canonical);
        assert_eq!(first, benchmark_provenance_config_hash(&canonical));

        let dirty = BenchmarkProvenance {
            source_dirty: true,
            ..canonical
        };
        assert_ne!(first, benchmark_provenance_config_hash(&dirty));
        assert_eq!(first.len(), 64);
    }

    #[test]
    fn allocator_unknown_token_fails_closed_to_compiled_allocator() {
        let resolved = resolve_bench_allocator_from(Some("weird"));
        assert_eq!(resolved.requested, "weird");
        assert_eq!(resolved.requested_source, "env");
        assert_eq!(resolved.effective, super::compiled_allocator());
        assert!(resolved.fallback_reason.is_some());
    }

    #[test]
    fn allocator_auto_defaults_to_compiled_allocator() {
        let resolved = resolve_bench_allocator_from(None);
        assert_eq!(resolved.requested, "auto");
        assert_eq!(resolved.requested_source, "default");
        assert_eq!(resolved.effective, super::compiled_allocator());
        assert!(resolved.fallback_reason.is_none());
    }

    #[test]
    fn allocator_jemalloc_request_reports_compile_time_mismatch() {
        let resolved = resolve_bench_allocator_from(Some("jemalloc"));
        assert_eq!(resolved.requested, "jemalloc");
        if super::compiled_allocator() == AllocatorKind::Jemalloc {
            assert_eq!(resolved.effective, AllocatorKind::Jemalloc);
            assert!(resolved.fallback_reason.is_none());
        } else {
            assert_eq!(resolved.effective, AllocatorKind::System);
            assert!(
                resolved.fallback_reason.is_some(),
                "{BENCH_ALLOCATOR_ENV}=jemalloc should report fallback without compiled jemalloc"
            );
        }
    }

    #[test]
    fn allocator_system_request_reports_compile_time_mismatch() {
        let resolved = resolve_bench_allocator_from(Some("system"));
        assert_eq!(resolved.requested, "system");
        if super::compiled_allocator() == AllocatorKind::Jemalloc {
            assert_eq!(resolved.effective, AllocatorKind::Jemalloc);
            assert!(resolved.fallback_reason.is_some());
        } else {
            assert_eq!(resolved.effective, AllocatorKind::System);
            assert!(resolved.fallback_reason.is_none());
        }
    }

    // ── Property tests ──

    mod proptest_perf_build {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn resolve_allocator_effective_is_always_compiled(
                raw_value in prop::option::of("[a-z]{0,20}"),
            ) {
                let resolved = resolve_bench_allocator_from(raw_value.as_deref());
                assert!(
                    resolved.effective == super::super::compiled_allocator(),
                    "effective allocator must always be compiled allocator"
                );
            }

            #[test]
            fn resolve_allocator_known_tokens_have_no_unknown_fallback(
                token in prop::sample::select(vec![
                    "auto", "default", "system", "native", "jemalloc", "je",
                ]),
            ) {
                let resolved = resolve_bench_allocator_from(Some(token));
                // Known tokens never produce "unknown allocator" fallback
                if let Some(reason) = &resolved.fallback_reason {
                    assert!(
                        !reason.starts_with("unknown allocator"),
                        "known token '{token}' should not produce unknown fallback: {reason}"
                    );
                }
            }

            #[test]
            fn resolve_allocator_unknown_tokens_always_have_fallback(
                token in "[a-z]{3,10}".prop_filter(
                    "must not be known",
                    |t| !matches!(t.as_str(), "auto" | "default" | "system" | "native" | "jemalloc" | "je"),
                ),
            ) {
                let resolved = resolve_bench_allocator_from(Some(&token));
                assert!(
                    resolved.fallback_reason.is_some(),
                    "unknown token '{token}' must produce a fallback reason"
                );
                assert!(
                    resolved.requested == token,
                    "unknown token should be passed through as-is"
                );
            }

            #[test]
            fn resolve_allocator_empty_or_whitespace_defaults_to_auto(
                value in prop::sample::select(vec!["", " ", "  ", "\t"]),
            ) {
                let resolved = resolve_bench_allocator_from(Some(value));
                assert!(
                    resolved.requested == "auto",
                    "empty/whitespace should default to 'auto', got '{}'",
                    resolved.requested,
                );
                assert_eq!(resolved.requested_source, "default");
            }

            #[test]
            fn resolve_allocator_none_defaults_to_auto(_dummy in Just(())) {
                let resolved = resolve_bench_allocator_from(None);
                assert_eq!(resolved.requested, "auto");
                assert_eq!(resolved.requested_source, "default");
                assert!(resolved.fallback_reason.is_none());
            }

            #[test]
            fn profile_from_target_path_uses_artifact_parent_for_custom_target_dirs(
                dir in "[a-z]{1,10}",
                binary in "[a-z_]{1,10}",
            ) {
                let path_str = format!("/{dir}/{binary}");
                let path = Path::new(&path_str);
                assert!(
                    profile_from_target_path(path).as_deref() == Some(dir.as_str()),
                    "direct artifact should use its parent as profile: {path_str}"
                );
            }

            #[test]
            fn profile_from_target_path_extracts_profile(
                profile in "[a-z]{3,10}",
                binary in "[a-z_]{3,10}",
            ) {
                let path_str = format!("/repo/target/{profile}/{binary}");
                let path = Path::new(&path_str);
                let result = profile_from_target_path(path);
                assert!(
                    result == Some(profile.clone()),
                    "expected Some(\"{profile}\"), got {result:?} for path {path_str}"
                );
            }

            #[test]
            fn detect_build_profile_env_overrides_all(
                env_val in "[a-z]{1,15}",
            ) {
                let result = detect_build_profile_from(
                    Some(&env_val),
                    Some(Path::new("/target/release/bin")),
                    true,
                );
                assert!(
                    result == env_val,
                    "env override should take priority: expected '{env_val}', got '{result}'"
                );
            }

            #[test]
            fn allocator_kind_as_str_is_stable(
                kind in prop::sample::select(vec![
                    AllocatorKind::System,
                    AllocatorKind::Jemalloc,
                ]),
            ) {
                let s1 = kind.as_str();
                let s2 = kind.as_str();
                assert!(s1 == s2, "as_str must be deterministic");
                assert!(
                    s1 == "system" || s1 == "jemalloc",
                    "as_str must return known value: {s1}"
                );
            }
        }
    }
}
