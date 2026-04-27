//! Shared performance-build metadata helpers for benchmark tooling.
//!
//! These helpers keep profile and allocator reporting consistent across
//! benchmark binaries, regression tests, and shell harnesses.

use std::path::Path;

/// Environment variable that overrides benchmark build-profile metadata.
pub const BENCH_BUILD_PROFILE_ENV: &str = "PI_BENCH_BUILD_PROFILE";

/// Environment variable that requests an allocator label for benchmark runs.
pub const BENCH_ALLOCATOR_ENV: &str = "PI_BENCH_ALLOCATOR";

/// Release binary-size budget (MB) shared by perf regression and budget gates.
pub const BINARY_SIZE_RELEASE_BUDGET_MB: f64 = 22.0;

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
///
/// Note: jemalloc is suppressed on FreeBSD even when the feature is on,
/// because FreeBSD's libc malloc is itself jemalloc-derived and a second
/// instance corrupts heap state. See the cfg in `lib.rs` for details.
#[must_use]
pub const fn compiled_allocator() -> AllocatorKind {
    if cfg!(all(
        feature = "jemalloc",
        not(target_env = "msvc"),
        not(target_os = "freebsd")
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
            Some("jemalloc requested but binary was built without --features jemalloc".to_string())
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

/// Attempts to derive Cargo profile from a binary path under `target/`.
#[must_use]
pub fn profile_from_target_path(path: &Path) -> Option<String> {
    let components: Vec<String> = path
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();

    let target_idx = components
        .iter()
        .rposition(|component| component == "target")?;
    let tail = components.get(target_idx + 1..)?;
    if tail.len() < 2 {
        return None;
    }

    let profile_idx = if tail.len() >= 3 && tail[tail.len() - 2] == "deps" {
        tail.len().checked_sub(3)?
    } else {
        tail.len().checked_sub(2)?
    };

    let candidate = tail.get(profile_idx)?.trim();
    if candidate.is_empty() {
        return None;
    }

    Some(candidate.to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        AllocatorKind, BENCH_ALLOCATOR_ENV, detect_build_profile_from, profile_from_target_path,
        resolve_bench_allocator_from,
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
    fn profile_from_target_path_returns_none_outside_target() {
        let path = Path::new("/tmp/repo/bin/pijs_workload");
        assert_eq!(profile_from_target_path(path), None);
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
        if cfg!(feature = "jemalloc") {
            assert_eq!(resolved.effective, AllocatorKind::Jemalloc);
            assert!(resolved.fallback_reason.is_none());
        } else {
            assert_eq!(resolved.effective, AllocatorKind::System);
            assert!(
                resolved.fallback_reason.is_some(),
                "{BENCH_ALLOCATOR_ENV}=jemalloc should report fallback without feature"
            );
        }
    }

    #[test]
    fn allocator_system_request_reports_compile_time_mismatch() {
        let resolved = resolve_bench_allocator_from(Some("system"));
        assert_eq!(resolved.requested, "system");
        if cfg!(feature = "jemalloc") {
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
                assert!(resolved.requested_source == "default");
            }

            #[test]
            fn resolve_allocator_none_defaults_to_auto(_dummy in Just(())) {
                let resolved = resolve_bench_allocator_from(None);
                assert!(resolved.requested == "auto");
                assert!(resolved.requested_source == "default");
                assert!(resolved.fallback_reason.is_none());
            }

            #[test]
            fn profile_from_target_path_requires_target_dir(
                dir in "[a-z]{1,10}",
                binary in "[a-z_]{1,10}",
            ) {
                // Paths without "target" component always return None
                let path_str = format!("/{dir}/{binary}");
                let path = Path::new(&path_str);
                assert!(
                    profile_from_target_path(path).is_none(),
                    "path without 'target' should return None: {path_str}"
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
