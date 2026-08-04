//! Background version check — queries GitHub releases for newer versions.
//!
//! Checks are non-blocking, cached for 24 hours, and configurable via
//! `check_for_updates` in settings.json.

use crate::error::{Error, Result};
use crate::http::client::Client;
use semver::{BuildMetadata, Version};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Current crate version (from Cargo.toml).
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long to cache the version check result (24 hours).
const CACHE_TTL_SECS: u64 = 24 * 60 * 60;
const RELEASE_CHECK_TIMEOUT: Duration = Duration::from_secs(10);
const RELEASES_URL: &str =
    "https://api.github.com/repos/Dicklesworthstone/pi_agent_rust/releases/latest";

/// Result of a version check.
#[derive(Debug, Clone)]
pub enum VersionCheckResult {
    /// A newer version is available.
    UpdateAvailable { latest: String },
    /// Already on the latest (or newer) version.
    UpToDate,
    /// Check failed (network error, parse error, etc.) — fail silently.
    Failed,
}

/// Compare two semver-like version strings (e.g. "0.1.0" vs "0.2.0").
///
/// Returns `true` if `latest` is strictly newer than `current`.
#[must_use]
pub fn is_newer(current: &str, latest: &str) -> bool {
    match (parse_semver_like(current), parse_semver_like(latest)) {
        (Some(current), Some(latest)) => latest > current,
        _ => false,
    }
}

fn parse_semver_like(version: &str) -> Option<Version> {
    let version = version.strip_prefix('v').unwrap_or(version).trim();
    if version.is_empty() {
        return None;
    }
    if let Ok(parsed) = Version::parse(version) {
        return Some(strip_build_metadata(parsed));
    }

    let suffix_idx = version.find(['-', '+']).unwrap_or(version.len());
    let (core, suffix) = version.split_at(suffix_idx);
    if core.split('.').count() != 2 {
        return None;
    }

    Version::parse(&format!("{core}.0{suffix}"))
        .ok()
        .map(strip_build_metadata)
}

fn strip_build_metadata(mut version: Version) -> Version {
    version.build = BuildMetadata::EMPTY;
    version
}

/// Path to the version check cache file.
fn cache_path() -> PathBuf {
    let config_dir = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    config_dir.join("pi").join(".version_check_cache")
}

/// Read a cached version if the cache is fresh (within TTL).
#[must_use]
pub fn read_cached_version() -> Option<String> {
    read_cached_version_at(&cache_path())
}

fn read_cached_version_at(path: &Path) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?;
    let age = modified.elapsed().ok()?;
    if age.as_secs() > CACHE_TTL_SECS {
        return None;
    }
    let content = std::fs::read_to_string(path).ok()?;
    let version = content.trim().to_string();
    if version.is_empty() {
        return None;
    }
    Some(version)
}

/// Write a version to the cache file.
pub fn write_cached_version(version: &str) {
    write_cached_version_at(&cache_path(), version);
}

fn write_cached_version_at(path: &Path, version: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, version);
}

/// Refresh the cached latest-release version when the cache is missing, stale,
/// or malformed. Returns the resulting status after the refresh attempt.
pub async fn refresh_cache_if_stale(client: &Client) -> VersionCheckResult {
    refresh_cache_if_stale_at(&cache_path(), client, CURRENT_VERSION, RELEASES_URL).await
}

async fn refresh_cache_if_stale_at(
    path: &Path,
    client: &Client,
    current_version: &str,
    release_url: &str,
) -> VersionCheckResult {
    if let Some(latest) = read_cached_version_at(path)
        && parse_semver_like(&latest).is_some()
    {
        return version_status_for(current_version, latest);
    }

    match fetch_latest_release_version_from_url(client, release_url).await {
        Ok(latest) => {
            write_cached_version_at(path, &latest);
            version_status_for(current_version, latest)
        }
        Err(err) => {
            tracing::debug!(error = %err, "background version check failed");
            VersionCheckResult::Failed
        }
    }
}

fn version_status_for(current_version: &str, latest: String) -> VersionCheckResult {
    if is_newer(current_version, &latest) {
        VersionCheckResult::UpdateAvailable { latest }
    } else {
        VersionCheckResult::UpToDate
    }
}

async fn fetch_latest_release_version_from_url(
    client: &Client,
    release_url: &str,
) -> Result<String> {
    let response = client
        .get(release_url)
        .timeout(RELEASE_CHECK_TIMEOUT)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await?;
    if status != 200 {
        return Err(Error::api(format!(
            "GitHub release lookup failed with status {status}"
        )));
    }

    parse_github_release_version(&body)
        .ok_or_else(|| Error::api("GitHub release lookup response missing tag_name".to_string()))
}

/// Check the latest version from cache or return None if cache is stale/missing.
///
/// The actual HTTP check is performed separately (by the caller spawning
/// a background task with the HTTP client).
#[must_use]
pub fn check_cached() -> VersionCheckResult {
    check_cached_at(&cache_path(), CURRENT_VERSION)
}

fn check_cached_at(path: &Path, current_version: &str) -> VersionCheckResult {
    let Some(latest) = read_cached_version_at(path) else {
        return VersionCheckResult::Failed;
    };

    match (
        parse_semver_like(current_version),
        parse_semver_like(&latest),
    ) {
        (Some(current), Some(latest_version)) => {
            if latest_version > current {
                VersionCheckResult::UpdateAvailable { latest }
            } else {
                VersionCheckResult::UpToDate
            }
        }
        _ => VersionCheckResult::Failed,
    }
}

/// Parse the latest version from a GitHub releases API JSON response.
///
/// Expects the response from `https://api.github.com/repos/OWNER/REPO/releases/latest`.
#[must_use]
pub fn parse_github_release_version(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let tag = value.get("tag_name")?.as_str()?;
    // Strip leading 'v' if present
    let version = tag.strip_prefix('v').unwrap_or(tag);
    if version.trim().is_empty() {
        return None;
    }
    Some(version.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    fn spawn_release_server(status: u16, body: &'static str) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind release server");
        let addr = listener.local_addr().expect("release server addr");
        let body = body.to_string();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept release request");
            let mut request = [0u8; 2048];
            let _ = stream.read(&mut request);
            let status_text = match status {
                200 => "OK",
                404 => "Not Found",
                500 => "Internal Server Error",
                _ => "Test Response",
            };
            let response = format!(
                "HTTP/1.1 {status} {status_text}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .expect("write release response");
        });
        (format!("http://{addr}/releases/latest"), handle)
    }

    #[test]
    fn is_newer_basic() {
        assert!(is_newer("0.1.0", "0.2.0"));
        assert!(is_newer("0.1.0", "1.0.0"));
        assert!(is_newer("1.0.0", "1.0.1"));
    }

    #[test]
    fn is_newer_same_version() {
        assert!(!is_newer("1.0.0", "1.0.0"));
    }

    #[test]
    fn is_newer_current_is_newer() {
        assert!(!is_newer("2.0.0", "1.0.0"));
    }

    #[test]
    fn is_newer_with_v_prefix() {
        assert!(is_newer("v0.1.0", "v0.2.0"));
        assert!(is_newer("0.1.0", "v0.2.0"));
        assert!(is_newer("v0.1.0", "0.2.0"));
    }

    #[test]
    fn is_newer_with_prerelease() {
        assert!(is_newer("1.2.3-dev", "1.2.3"));
        assert!(is_newer("1.2.3-dev", "1.3.0"));
        assert!(!is_newer("1.2.3", "1.2.3-dev"));
    }

    #[test]
    fn is_newer_ignores_build_metadata() {
        assert!(!is_newer("1.2.3+build.1", "1.2.3+build.2"));
        assert!(!is_newer("1.2.3", "1.2.3+build.2"));
    }

    #[test]
    fn is_newer_invalid_versions() {
        assert!(!is_newer("not-a-version", "1.0.0"));
        assert!(!is_newer("1.0.0", "not-a-version"));
        assert!(!is_newer("", ""));
    }

    #[test]
    fn parse_github_release_version_valid() {
        let json = r#"{"tag_name": "v0.2.0", "name": "Release 0.2.0"}"#;
        assert_eq!(
            parse_github_release_version(json),
            Some("0.2.0".to_string())
        );
    }

    #[test]
    fn parse_github_release_version_no_v_prefix() {
        let json = r#"{"tag_name": "0.2.0"}"#;
        assert_eq!(
            parse_github_release_version(json),
            Some("0.2.0".to_string())
        );
    }

    #[test]
    fn parse_github_release_version_invalid_json() {
        assert_eq!(parse_github_release_version("not json"), None);
    }

    #[test]
    fn parse_github_release_version_missing_tag() {
        let json = r#"{"name": "Release"}"#;
        assert_eq!(parse_github_release_version(json), None);
    }

    #[test]
    fn parse_github_release_version_rejects_empty_tag() {
        assert_eq!(parse_github_release_version(r#"{"tag_name": ""}"#), None);
        assert_eq!(parse_github_release_version(r#"{"tag_name": "v"}"#), None);
    }

    #[test]
    fn cache_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache");

        write_cached_version_at(&path, "1.2.3");
        assert_eq!(read_cached_version_at(&path), Some("1.2.3".to_string()));
    }

    #[test]
    fn cache_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent");
        assert_eq!(read_cached_version_at(&path), None);
    }

    #[test]
    fn cache_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache");
        std::fs::write(&path, "").unwrap();
        assert_eq!(read_cached_version_at(&path), None);
    }

    #[test]
    fn check_cached_invalid_cached_version_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache");
        write_cached_version_at(&path, "not-a-version");
        assert!(matches!(
            check_cached_at(&path, "1.2.3"),
            VersionCheckResult::Failed
        ));
    }

    #[test]
    fn refresh_cache_if_stale_fetches_and_writes_latest_release() {
        asupersync::test_utils::run_test(|| async {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("cache");
            let (url, server) = spawn_release_server(200, r#"{"tag_name":"v9.9.9"}"#);

            let client = Client::new();
            let result = refresh_cache_if_stale_at(&path, &client, "1.0.0", &url).await;

            assert!(matches!(
                result,
                VersionCheckResult::UpdateAvailable { latest } if latest == "9.9.9"
            ));
            assert_eq!(read_cached_version_at(&path), Some("9.9.9".to_string()));
            server.join().expect("join release server");
        });
    }

    #[test]
    fn refresh_cache_if_stale_uses_fresh_cache_without_network() {
        asupersync::test_utils::run_test(|| async {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("cache");
            write_cached_version_at(&path, "1.2.3");

            let client = Client::new();
            let result = refresh_cache_if_stale_at(
                &path,
                &client,
                "1.0.0",
                "http://127.0.0.1:9/releases/latest",
            )
            .await;

            assert!(matches!(
                result,
                VersionCheckResult::UpdateAvailable { latest } if latest == "1.2.3"
            ));
            assert_eq!(read_cached_version_at(&path), Some("1.2.3".to_string()));
        });
    }

    #[test]
    fn refresh_cache_if_stale_replaces_malformed_cache() {
        asupersync::test_utils::run_test(|| async {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("cache");
            write_cached_version_at(&path, "definitely-not-a-version");
            let (url, server) = spawn_release_server(200, r#"{"tag_name":"v2.1.0"}"#);

            let client = Client::new();
            let result = refresh_cache_if_stale_at(&path, &client, "2.0.0", &url).await;

            assert!(matches!(
                result,
                VersionCheckResult::UpdateAvailable { latest } if latest == "2.1.0"
            ));
            assert_eq!(read_cached_version_at(&path), Some("2.1.0".to_string()));
            server.join().expect("join release server");
        });
    }

    #[test]
    fn refresh_cache_if_stale_fail_closed_on_invalid_release_payload() {
        asupersync::test_utils::run_test(|| async {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("cache");
            let (url, server) = spawn_release_server(200, r#"{"name":"missing tag"}"#);

            let client = Client::new();
            let result = refresh_cache_if_stale_at(&path, &client, "1.0.0", &url).await;

            assert!(matches!(result, VersionCheckResult::Failed));
            assert_eq!(read_cached_version_at(&path), None);
            server.join().expect("join release server");
        });
    }

    mod proptest_version_check {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            /// `is_newer` is irreflexive: no version is newer than itself.
            #[test]
            fn is_newer_irreflexive(
                major in 0..100u32,
                minor in 0..100u32,
                patch in 0..100u32
            ) {
                let v = format!("{major}.{minor}.{patch}");
                assert!(!is_newer(&v, &v));
            }

            /// `is_newer` is asymmetric: if a > b then !(b > a).
            #[test]
            fn is_newer_asymmetric(
                major in 0..50u32,
                minor in 0..50u32,
                patch in 0..50u32,
                bump in 1..10u32
            ) {
                let older = format!("{major}.{minor}.{patch}");
                let newer = format!("{major}.{minor}.{}", patch + bump);
                assert!(is_newer(&older, &newer));
                assert!(!is_newer(&newer, &older));
            }

            /// Leading 'v' prefix is stripped transparently.
            #[test]
            fn v_prefix_transparent(
                major in 0..100u32,
                minor in 0..100u32,
                patch in 0..100u32,
                bump in 1..10u32
            ) {
                let older = format!("{major}.{minor}.{patch}");
                let newer = format!("{major}.{minor}.{}", patch + bump);
                assert_eq!(
                    is_newer(&older, &newer),
                    is_newer(&format!("v{older}"), &format!("v{newer}"))
                );
            }

            /// A stable release must outrank the matching prerelease.
            #[test]
            fn release_outranks_prerelease(
                major in 0..100u32,
                minor in 0..100u32,
                patch in 0..100u32,
                suffix in "[a-z]{1,8}"
            ) {
                let plain = format!("{major}.{minor}.{patch}");
                let pre = format!("{major}.{minor}.{patch}-{suffix}");
                assert!(!is_newer(&plain, &pre));
                assert!(is_newer(&pre, &plain));
            }

            /// Build metadata must not change ordering.
            #[test]
            fn build_metadata_does_not_change_ordering(
                major in 0..100u32,
                minor in 0..100u32,
                patch in 0..100u32,
                build_a in "[a-z0-9]{1,8}",
                build_b in "[a-z0-9]{1,8}"
            ) {
                let with_a = format!("{major}.{minor}.{patch}+{build_a}");
                let with_b = format!("{major}.{minor}.{patch}+{build_b}");
                assert!(!is_newer(&with_a, &with_b));
                assert!(!is_newer(&with_b, &with_a));
            }

            /// Garbage strings never report newer.
            #[test]
            fn garbage_never_newer(s in "\\PC{1,30}") {
                assert!(!is_newer(&s, "1.0.0") || s.contains('.'));
                assert!(!is_newer("1.0.0", &s) || s.contains('.'));
            }

            /// `parse_github_release_version` extracts tag_name.
            #[test]
            fn parse_github_release_extracts_tag(ver in "[0-9]{1,3}\\.[0-9]{1,3}\\.[0-9]{1,3}") {
                let json = format!(r#"{{"tag_name": "v{ver}"}}"#);
                assert_eq!(parse_github_release_version(&json), Some(ver));
            }

            /// Missing `tag_name` returns None.
            #[test]
            fn parse_github_release_no_tag(key in "[a-z_]{1,10}") {
                prop_assume!(key != "tag_name");
                let json = format!(r#"{{"{key}": "v1.0.0"}}"#);
                assert_eq!(parse_github_release_version(&json), None);
            }

            /// Invalid JSON returns None.
            #[test]
            fn parse_github_release_invalid_json(s in "[^{}]{1,30}") {
                assert_eq!(parse_github_release_version(&s), None);
            }

            /// Cache round-trip preserves version string.
            #[test]
            fn cache_round_trip_preserves(ver in "[0-9]{1,3}\\.[0-9]{1,3}\\.[0-9]{1,3}") {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("cache");
                write_cached_version_at(&path, &ver);
                assert_eq!(read_cached_version_at(&path), Some(ver));
            }

            /// Major version bumps are always detected.
            #[test]
            fn major_bump_detected(
                major in 0..50u32,
                minor in 0..100u32,
                patch in 0..100u32,
                bump in 1..10u32
            ) {
                let older = format!("{major}.{minor}.{patch}");
                let newer = format!("{}.0.0", major + bump);
                assert!(is_newer(&older, &newer));
            }

            /// Two-component versions default patch to 0.
            #[test]
            fn two_component_version(
                major in 0..100u32,
                minor in 0..100u32,
                bump in 1..10u32
            ) {
                let v2 = format!("{major}.{minor}");
                let v3 = format!("{major}.{minor}.0");
                // Both should parse the same, so neither is newer
                assert!(!is_newer(&v2, &v3));
                assert!(!is_newer(&v3, &v2));
                // But a bumped version IS newer
                let bumped = format!("{major}.{}.0", minor + bump);
                assert!(is_newer(&v2, &bumped));
            }

            /// Strict patch bumps are transitive for well-formed versions.
            #[test]
            fn patch_bump_transitivity(
                major in 0..100u32,
                minor in 0..100u32,
                patch in 0..100u32,
                bump_a in 1..10u32,
                bump_b in 1..10u32
            ) {
                let base = format!("{major}.{minor}.{patch}");
                let mid = format!("{major}.{minor}.{}", patch + bump_a);
                let top = format!("{major}.{minor}.{}", patch + bump_a + bump_b);

                assert!(is_newer(&base, &mid));
                assert!(is_newer(&mid, &top));
                assert!(is_newer(&base, &top));
            }
        }
    }
}
