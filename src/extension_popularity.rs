//! Popularity signal snapshotting for extension candidates.
//!
//! This module is intentionally "evidence-first":
//! - Fetch concrete metrics (GitHub stars/downloads/etc).
//! - Normalize missing/unavailable metrics to `null` (never `0`).
//! - Persist evidence onto the canonical candidate pool JSON so scoring can be auditable.

use crate::error::{Error, Result};
use crate::http::client::Client;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::time::Duration;

const POPULARITY_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CandidatePool {
    #[serde(rename = "$schema")]
    pub schema: String,
    pub generated_at: String,
    pub source_inputs: SourceInputs,
    pub total_candidates: u64,
    pub items: Vec<CandidateItem>,
    pub alias_notes: Vec<AliasNote>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SourceInputs {
    pub artifact_provenance: String,
    pub artifact_root: String,
    pub extra_npm_packages: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AliasNote {
    pub note: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CandidateItem {
    pub id: String,
    pub name: String,
    pub source_tier: String,
    pub status: String,
    pub license: String,
    pub retrieved: Option<String>,
    pub artifact_path: Option<String>,
    pub checksum: Option<Sha256Checksum>,
    pub source: CandidateSource,
    pub repository_url: Option<String>,
    #[serde(default)]
    pub popularity: PopularityEvidence,
    pub aliases: Vec<String>,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Sha256Checksum {
    pub sha256: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CandidateSource {
    Git {
        repo: String,
        #[serde(default)]
        path: Option<String>,
    },
    Npm {
        package: String,
        version: String,
        url: String,
    },
    Url {
        url: String,
    },
}

/// Popularity evidence schema.
///
/// This is the machine-joinable surface used by scoring (see `docs/EXTENSION_POPULARITY_CRITERIA.md`).
/// When a metric is unknown/unavailable, it should be persisted as explicit `null`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct PopularityEvidence {
    pub snapshot_at: Option<String>,

    // GitHub
    pub github_repo: Option<String>,
    pub github_stars: Option<u64>,
    pub github_forks: Option<u64>,
    pub github_watchers: Option<u64>,
    pub github_open_issues: Option<u64>,
    pub github_last_commit: Option<String>,

    // npm
    pub npm_downloads_weekly: Option<u64>,
    pub npm_downloads_monthly: Option<u64>,
    pub npm_last_publish: Option<String>,
    pub npm_dependents: Option<u64>,

    // Marketplace (OpenClaw / ClawHub) - not currently populated by the candidate pool.
    pub marketplace_rank: Option<u32>,
    pub marketplace_installs_monthly: Option<u64>,
    pub marketplace_featured: Option<bool>,

    // Mentions / references - not currently populated by the candidate pool.
    pub mentions_count: Option<u32>,
    pub mentions_sources: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GitHubRepoRef {
    pub owner: String,
    pub repo: String,
}

impl GitHubRepoRef {
    #[must_use]
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubRepoMetrics {
    pub full_name: String,
    pub stars: u64,
    pub forks: u64,
    pub watchers: Option<u64>,
    pub open_issues: u64,
    pub pushed_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NpmDownloads {
    pub weekly: Option<u64>,
    pub monthly: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NpmRegistryMeta {
    pub latest_version: Option<String>,
    pub last_publish: Option<String>,
    pub repository_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitHubRepoCandidate {
    Repo(GitHubRepoRef),
    /// A malformed GitHub URL that only included a single path segment, e.g. `https://github.com/foo-bar`.
    Slug(String),
}

/// Best-effort parse of a GitHub repository reference from a URL-like string.
///
/// Supports:
/// - `https://github.com/owner/repo`
/// - `git+https://github.com/owner/repo.git`
/// - `git@github.com:owner/repo.git`
/// - `github.com/owner/repo`
///
/// For malformed single-segment URLs (e.g. `https://github.com/foo-bar`) returns a `Slug` candidate.
#[must_use]
pub fn github_repo_candidate_from_url(input: &str) -> Option<GitHubRepoCandidate> {
    let raw = input.trim();
    if raw.is_empty() {
        return None;
    }

    let raw = raw.strip_prefix("git+").unwrap_or(raw);

    if let Some(rest) = raw.strip_prefix("git@") {
        // SCP-like: git@github.com:owner/repo(.git)
        let (host, path) = rest.split_once(':')?;
        if !host.eq_ignore_ascii_case("github.com") {
            return None;
        }
        return parse_owner_repo_from_path(path).map(GitHubRepoCandidate::Repo);
    }

    let url_str = if raw.contains("://") {
        raw.to_string()
    } else {
        format!("https://{raw}")
    };

    let Ok(url) = url::Url::parse(&url_str) else {
        return None;
    };
    if url.host_str()? != "github.com" {
        return None;
    }

    let mut segments = url.path_segments()?.filter(|seg| !seg.is_empty());
    let ownerish = segments.next()?.to_string();
    let repo = segments.next().map(ToString::to_string);

    match repo {
        Some(ref repo) => parse_owner_repo(&ownerish, repo).map(GitHubRepoCandidate::Repo),
        None => Some(GitHubRepoCandidate::Slug(ownerish)),
    }
}

#[must_use]
pub fn github_repo_guesses_from_slug(slug: &str) -> Vec<GitHubRepoRef> {
    let slug = slug.trim().trim_matches('/');
    if slug.is_empty() {
        return Vec::new();
    }

    let mut seen = HashSet::<GitHubRepoRef>::new();
    let mut out = Vec::new();

    // Common case for our third-party imports: `owner-pi-foo` should be `owner/pi-foo`.
    if let Some((owner, suffix)) = slug.split_once("-pi-") {
        let repo = format!("pi-{suffix}");
        if let Some(r) = parse_owner_repo(owner, &repo)
            && seen.insert(r.clone())
        {
            out.push(r);
        }
    }

    // Try first hyphen split: `owner-rest...` -> `owner/rest...`
    if let Some((owner, repo)) = slug.split_once('-')
        && let Some(r) = parse_owner_repo(owner, repo)
        && seen.insert(r.clone())
    {
        out.push(r);
    }

    // Try last hyphen split: `owner...-repo` -> `owner.../repo`
    if let Some((owner, repo)) = slug.rsplit_once('-')
        && let Some(r) = parse_owner_repo(owner, repo)
        && seen.insert(r.clone())
    {
        out.push(r);
    }

    out
}

pub fn parse_github_repo_response(text: &str) -> Result<GitHubRepoMetrics> {
    #[derive(Debug, Deserialize)]
    struct RepoResponse {
        full_name: String,
        stargazers_count: u64,
        forks_count: u64,
        #[serde(default)]
        subscribers_count: Option<u64>,
        open_issues_count: u64,
        #[serde(default)]
        pushed_at: Option<String>,
    }

    let parsed: RepoResponse = serde_json::from_str(text)
        .map_err(|err| Error::api(format!("GitHub repo response parse error: {err}")))?;

    Ok(GitHubRepoMetrics {
        full_name: parsed.full_name,
        stars: parsed.stargazers_count,
        forks: parsed.forks_count,
        watchers: parsed.subscribers_count,
        open_issues: parsed.open_issues_count,
        pushed_at: parsed.pushed_at,
    })
}

pub async fn fetch_github_repo_metrics_optional(
    client: &Client,
    token: &str,
    repo: &GitHubRepoRef,
) -> Result<Option<GitHubRepoMetrics>> {
    let url = format!("https://api.github.com/repos/{}/{}", repo.owner, repo.repo);
    let response = client
        .get(&url)
        .timeout(POPULARITY_REQUEST_TIMEOUT)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await?;

    let status = response.status();
    let text = response.text().await?;

    match status {
        200 => Ok(Some(parse_github_repo_response(&text)?)),
        404 => Ok(None),
        other => Err(Error::api(format!("GitHub API error {other}: {text}"))),
    }
}

pub fn parse_npm_downloads_response(text: &str) -> Result<Option<u64>> {
    #[derive(Debug, Deserialize)]
    struct DownloadsResponse {
        #[serde(default)]
        downloads: Option<u64>,
        #[serde(default)]
        error: Option<String>,
    }

    let parsed: DownloadsResponse = serde_json::from_str(text)
        .map_err(|err| Error::api(format!("npm downloads response parse error: {err}")))?;

    if parsed.error.is_some() {
        return Ok(None);
    }

    Ok(parsed.downloads)
}

pub async fn fetch_npm_downloads(client: &Client, package: &str) -> Result<NpmDownloads> {
    async fn fetch_range(client: &Client, package: &str, range: &str) -> Result<Option<u64>> {
        let encoded = url::form_urlencoded::byte_serialize(package.as_bytes()).collect::<String>();
        let url = format!("https://api.npmjs.org/downloads/point/{range}/{encoded}");
        let response = client
            .get(&url)
            .timeout(POPULARITY_REQUEST_TIMEOUT)
            .send()
            .await?;
        let text = response.text().await?;
        parse_npm_downloads_response(&text)
    }

    let weekly = fetch_range(client, package, "last-week").await?;
    let monthly = fetch_range(client, package, "last-month").await?;

    Ok(NpmDownloads { weekly, monthly })
}

pub fn parse_npm_registry_response(text: &str) -> Result<NpmRegistryMeta> {
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|err| Error::api(format!("npm registry response parse error: {err}")))?;

    let latest_version = value
        .get("dist-tags")
        .and_then(|tags| tags.get("latest"))
        .and_then(|v| v.as_str())
        .map(ToString::to_string);

    let last_publish = latest_version
        .as_deref()
        .and_then(|latest| value.get("time").and_then(|t| t.get(latest)))
        .and_then(|v| v.as_str())
        .map(ToString::to_string);

    let repository_url = match value.get("repository") {
        Some(serde_json::Value::String(url)) => Some(url.clone()),
        Some(serde_json::Value::Object(obj)) => obj
            .get("url")
            .and_then(|url| url.as_str())
            .map(ToString::to_string),
        _ => None,
    };

    Ok(NpmRegistryMeta {
        latest_version,
        last_publish,
        repository_url,
    })
}

pub async fn fetch_npm_registry_meta(
    client: &Client,
    package: &str,
) -> Result<Option<NpmRegistryMeta>> {
    let encoded = url::form_urlencoded::byte_serialize(package.as_bytes()).collect::<String>();
    let url = format!("https://registry.npmjs.org/{encoded}");
    let response = client
        .get(&url)
        .timeout(POPULARITY_REQUEST_TIMEOUT)
        .send()
        .await?;
    let status = response.status();
    let text = response.text().await?;

    match status {
        200 => Ok(Some(parse_npm_registry_response(&text)?)),
        404 => Ok(None),
        other => Err(Error::api(format!("npm registry error {other}: {text}"))),
    }
}

fn parse_owner_repo(owner: &str, repo: &str) -> Option<GitHubRepoRef> {
    let owner = owner.trim().trim_matches('/').to_string();
    let repo = repo
        .trim()
        .trim_matches('/')
        .trim_end_matches(".git")
        .to_string();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(GitHubRepoRef { owner, repo })
}

fn parse_owner_repo_from_path(path: &str) -> Option<GitHubRepoRef> {
    let path = path.trim().trim_matches('/');
    let mut parts = path.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    parse_owner_repo(owner, repo)
}

/// Fetch all referenced GitHub repos (deduped) and return a `full_name -> metrics` map.
pub async fn snapshot_github_repos(
    client: &Client,
    token: &str,
    repos: &[GitHubRepoRef],
) -> Result<HashMap<String, GitHubRepoMetrics>> {
    let mut out = HashMap::new();
    for repo in repos {
        if let Some(metrics) = fetch_github_repo_metrics_optional(client, token, repo).await? {
            out.insert(repo.full_name(), metrics);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ====================================================================
    // GitHubRepoRef
    // ====================================================================

    #[test]
    fn github_repo_ref_full_name() {
        let r = GitHubRepoRef {
            owner: "anthropics".to_string(),
            repo: "claude-code".to_string(),
        };
        assert_eq!(r.full_name(), "anthropics/claude-code");
    }

    // ====================================================================
    // github_repo_candidate_from_url
    // ====================================================================

    #[test]
    fn url_https_standard() {
        let c = github_repo_candidate_from_url("https://github.com/owner/repo").unwrap();
        assert_eq!(
            c,
            GitHubRepoCandidate::Repo(GitHubRepoRef {
                owner: "owner".to_string(),
                repo: "repo".to_string()
            })
        );
    }

    #[test]
    fn url_https_with_dot_git() {
        let c = github_repo_candidate_from_url("https://github.com/owner/repo.git").unwrap();
        assert_eq!(
            c,
            GitHubRepoCandidate::Repo(GitHubRepoRef {
                owner: "owner".to_string(),
                repo: "repo".to_string()
            })
        );
    }

    #[test]
    fn url_git_plus_https() {
        let c = github_repo_candidate_from_url("git+https://github.com/owner/repo.git").unwrap();
        assert_eq!(
            c,
            GitHubRepoCandidate::Repo(GitHubRepoRef {
                owner: "owner".to_string(),
                repo: "repo".to_string()
            })
        );
    }

    #[test]
    fn url_git_at_scp() {
        let c = github_repo_candidate_from_url("git@github.com:owner/repo.git").unwrap();
        assert_eq!(
            c,
            GitHubRepoCandidate::Repo(GitHubRepoRef {
                owner: "owner".to_string(),
                repo: "repo".to_string()
            })
        );
    }

    #[test]
    fn url_bare_domain() {
        let c = github_repo_candidate_from_url("github.com/owner/repo").unwrap();
        assert_eq!(
            c,
            GitHubRepoCandidate::Repo(GitHubRepoRef {
                owner: "owner".to_string(),
                repo: "repo".to_string()
            })
        );
    }

    #[test]
    fn url_single_segment_returns_slug() {
        let c = github_repo_candidate_from_url("https://github.com/foo-bar").unwrap();
        assert_eq!(c, GitHubRepoCandidate::Slug("foo-bar".to_string()));
    }

    #[test]
    fn url_empty_string_returns_none() {
        assert!(github_repo_candidate_from_url("").is_none());
    }

    #[test]
    fn url_whitespace_only_returns_none() {
        assert!(github_repo_candidate_from_url("   ").is_none());
    }

    #[test]
    fn url_non_github_returns_none() {
        assert!(github_repo_candidate_from_url("https://gitlab.com/owner/repo").is_none());
    }

    #[test]
    fn url_git_at_non_github_returns_none() {
        assert!(github_repo_candidate_from_url("git@gitlab.com:owner/repo.git").is_none());
    }

    #[test]
    fn url_with_trailing_path() {
        let c = github_repo_candidate_from_url("https://github.com/owner/repo/tree/main").unwrap();
        assert_eq!(
            c,
            GitHubRepoCandidate::Repo(GitHubRepoRef {
                owner: "owner".to_string(),
                repo: "repo".to_string()
            })
        );
    }

    #[test]
    fn url_with_leading_trailing_whitespace() {
        let c = github_repo_candidate_from_url("  https://github.com/owner/repo  ").unwrap();
        assert_eq!(
            c,
            GitHubRepoCandidate::Repo(GitHubRepoRef {
                owner: "owner".to_string(),
                repo: "repo".to_string()
            })
        );
    }

    // ====================================================================
    // github_repo_guesses_from_slug
    // ====================================================================

    #[test]
    fn slug_guess_pi_pattern() {
        let guesses = github_repo_guesses_from_slug("owner-pi-foo");
        assert!(
            guesses
                .iter()
                .any(|r| r.owner == "owner" && r.repo == "pi-foo")
        );
    }

    #[test]
    fn slug_guess_simple_hyphen() {
        let guesses = github_repo_guesses_from_slug("alice-myrepo");
        assert!(
            guesses
                .iter()
                .any(|r| r.owner == "alice" && r.repo == "myrepo")
        );
    }

    #[test]
    fn slug_guess_empty_returns_empty() {
        assert!(github_repo_guesses_from_slug("").is_empty());
    }

    #[test]
    fn slug_guess_whitespace_returns_empty() {
        assert!(github_repo_guesses_from_slug("   ").is_empty());
    }

    #[test]
    fn slug_guess_no_hyphen_returns_empty() {
        assert!(github_repo_guesses_from_slug("nohyphen").is_empty());
    }

    #[test]
    fn slug_guess_multiple_hyphens_gives_multiple_guesses() {
        let guesses = github_repo_guesses_from_slug("a-b-c");
        assert!(!guesses.is_empty());
        // Should contain at least first-split ("a"/"b-c") and last-split ("a-b"/"c").
        assert!(guesses.iter().any(|r| r.owner == "a" && r.repo == "b-c"));
        assert!(guesses.iter().any(|r| r.owner == "a-b" && r.repo == "c"));
    }

    // ====================================================================
    // parse_github_repo_response
    // ====================================================================

    #[test]
    fn parse_github_repo_response_full() {
        let json = r#"{
            "full_name": "anthropics/claude-code",
            "stargazers_count": 42000,
            "forks_count": 1500,
            "subscribers_count": 800,
            "open_issues_count": 123,
            "pushed_at": "2026-02-01T12:00:00Z"
        }"#;
        let metrics = parse_github_repo_response(json).unwrap();
        assert_eq!(metrics.full_name, "anthropics/claude-code");
        assert_eq!(metrics.stars, 42000);
        assert_eq!(metrics.forks, 1500);
        assert_eq!(metrics.watchers, Some(800));
        assert_eq!(metrics.open_issues, 123);
        assert_eq!(metrics.pushed_at, Some("2026-02-01T12:00:00Z".to_string()));
    }

    #[test]
    fn parse_github_repo_response_missing_optional_fields() {
        let json = r#"{
            "full_name": "owner/repo",
            "stargazers_count": 10,
            "forks_count": 2,
            "open_issues_count": 0
        }"#;
        let metrics = parse_github_repo_response(json).unwrap();
        assert_eq!(metrics.stars, 10);
        assert_eq!(metrics.watchers, None);
        assert_eq!(metrics.pushed_at, None);
    }

    #[test]
    fn parse_github_repo_response_invalid_json() {
        assert!(parse_github_repo_response("{broken}").is_err());
    }

    // ====================================================================
    // parse_npm_downloads_response
    // ====================================================================

    #[test]
    fn parse_npm_downloads_response_with_count() {
        let json = r#"{"downloads": 50000}"#;
        assert_eq!(parse_npm_downloads_response(json).unwrap(), Some(50000));
    }

    #[test]
    fn parse_npm_downloads_response_with_error() {
        let json = r#"{"error": "package not found"}"#;
        assert_eq!(parse_npm_downloads_response(json).unwrap(), None);
    }

    #[test]
    fn parse_npm_downloads_response_null_downloads() {
        let json = r#"{"downloads": null}"#;
        assert_eq!(parse_npm_downloads_response(json).unwrap(), None);
    }

    #[test]
    fn parse_npm_downloads_response_zero() {
        let json = r#"{"downloads": 0}"#;
        assert_eq!(parse_npm_downloads_response(json).unwrap(), Some(0));
    }

    #[test]
    fn parse_npm_downloads_response_invalid_json() {
        assert!(parse_npm_downloads_response("{bad").is_err());
    }

    // ====================================================================
    // parse_npm_registry_response
    // ====================================================================

    #[test]
    fn parse_npm_registry_response_full() {
        let json = r#"{
            "dist-tags": {"latest": "3.2.1"},
            "time": {"3.2.1": "2026-01-15T10:00:00Z"},
            "repository": {"type": "git", "url": "https://github.com/owner/repo.git"}
        }"#;
        let meta = parse_npm_registry_response(json).unwrap();
        assert_eq!(meta.latest_version, Some("3.2.1".to_string()));
        assert_eq!(meta.last_publish, Some("2026-01-15T10:00:00Z".to_string()));
        assert_eq!(
            meta.repository_url,
            Some("https://github.com/owner/repo.git".to_string())
        );
    }

    #[test]
    fn parse_npm_registry_response_string_repository() {
        let json = r#"{
            "dist-tags": {"latest": "1.0.0"},
            "time": {"1.0.0": "2026-01-01T00:00:00Z"},
            "repository": "https://github.com/owner/repo"
        }"#;
        let meta = parse_npm_registry_response(json).unwrap();
        assert_eq!(
            meta.repository_url,
            Some("https://github.com/owner/repo".to_string())
        );
    }

    #[test]
    fn parse_npm_registry_response_no_dist_tags() {
        let json = "{}";
        let meta = parse_npm_registry_response(json).unwrap();
        assert_eq!(meta.latest_version, None);
        assert_eq!(meta.last_publish, None);
        assert_eq!(meta.repository_url, None);
    }

    #[test]
    fn parse_npm_registry_response_invalid_json() {
        assert!(parse_npm_registry_response("{broken").is_err());
    }

    // ====================================================================
    // PopularityEvidence serde round-trip
    // ====================================================================

    #[test]
    fn popularity_evidence_default_serializes_all_none() {
        let pe = PopularityEvidence::default();
        let json = serde_json::to_value(&pe).unwrap();
        assert!(json["github_stars"].is_null());
        assert!(json["npm_downloads_weekly"].is_null());
        assert!(json["marketplace_rank"].is_null());
    }

    #[test]
    fn popularity_evidence_round_trip() {
        let pe = PopularityEvidence {
            snapshot_at: Some("2026-02-06T12:00:00Z".to_string()),
            github_stars: Some(42000),
            github_forks: Some(1500),
            npm_downloads_weekly: Some(100_000),
            npm_downloads_monthly: Some(400_000),
            ..Default::default()
        };
        let json = serde_json::to_string(&pe).unwrap();
        let pe2: PopularityEvidence = serde_json::from_str(&json).unwrap();
        assert_eq!(pe2.github_stars, Some(42000));
        assert_eq!(pe2.npm_downloads_weekly, Some(100_000));
        assert_eq!(pe2.github_watchers, None);
    }

    // ====================================================================
    // CandidateSource serde (tagged enum variants)
    // ====================================================================

    #[test]
    fn candidate_source_git_round_trip() {
        let src = CandidateSource::Git {
            repo: "https://github.com/owner/repo.git".to_string(),
            path: Some("packages/core".to_string()),
        };
        let json = serde_json::to_string(&src).unwrap();
        assert!(json.contains(r#""type":"git"#));
        let deserialized: CandidateSource = serde_json::from_str(&json).unwrap();
        match deserialized {
            CandidateSource::Git { repo, path } => {
                assert_eq!(repo, "https://github.com/owner/repo.git");
                assert_eq!(path, Some("packages/core".to_string()));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn candidate_source_npm_round_trip() {
        let src = CandidateSource::Npm {
            package: "@scope/pkg".to_string(),
            version: "1.2.3".to_string(),
            url: "https://registry.npmjs.org/@scope/pkg/-/pkg-1.2.3.tgz".to_string(),
        };
        let json = serde_json::to_string(&src).unwrap();
        assert!(json.contains(r#""type":"npm"#));
        let deserialized: CandidateSource = serde_json::from_str(&json).unwrap();
        match deserialized {
            CandidateSource::Npm {
                package,
                version,
                url,
            } => {
                assert_eq!(package, "@scope/pkg");
                assert_eq!(version, "1.2.3");
                assert!(url.contains("registry.npmjs.org"));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn candidate_source_url_round_trip() {
        let src = CandidateSource::Url {
            url: "https://example.com/ext.tgz".to_string(),
        };
        let json = serde_json::to_string(&src).unwrap();
        assert!(json.contains(r#""type":"url"#));
        let deserialized: CandidateSource = serde_json::from_str(&json).unwrap();
        match deserialized {
            CandidateSource::Url { url } => {
                assert_eq!(url, "https://example.com/ext.tgz");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn candidate_source_git_no_path() {
        let src = CandidateSource::Git {
            repo: "https://github.com/owner/repo".to_string(),
            path: None,
        };
        let json = serde_json::to_string(&src).unwrap();
        let deserialized: CandidateSource = serde_json::from_str(&json).unwrap();
        match deserialized {
            CandidateSource::Git { path, .. } => {
                assert_eq!(path, None);
            }
            _ => panic!(),
        }
    }

    // ====================================================================
    // parse_owner_repo edge cases (via public API)
    // ====================================================================

    #[test]
    fn url_with_trailing_slash() {
        let c = github_repo_candidate_from_url("https://github.com/owner/repo/").unwrap();
        assert_eq!(
            c,
            GitHubRepoCandidate::Repo(GitHubRepoRef {
                owner: "owner".to_string(),
                repo: "repo".to_string()
            })
        );
    }

    // ====================================================================
    // NpmDownloads / NpmRegistryMeta / GitHubRepoMetrics equality
    // ====================================================================

    #[test]
    fn npm_downloads_equality() {
        let a = NpmDownloads {
            weekly: Some(100),
            monthly: Some(400),
        };
        let b = NpmDownloads {
            weekly: Some(100),
            monthly: Some(400),
        };
        assert_eq!(a, b);
    }

    #[test]
    fn github_repo_metrics_equality() {
        let a = GitHubRepoMetrics {
            full_name: "o/r".to_string(),
            stars: 10,
            forks: 5,
            watchers: None,
            open_issues: 0,
            pushed_at: None,
        };
        let b = a.clone();
        assert_eq!(a, b);
    }

    mod proptest_extension_popularity {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            /// `github_repo_candidate_from_url` never panics on arbitrary input.
            #[test]
            fn github_url_never_panics(s in "(?s).{0,200}") {
                let _ = github_repo_candidate_from_url(&s);
            }

            /// Valid `https://github.com/owner/repo` URLs always parse to `Repo`.
            #[test]
            fn valid_github_url_parses_to_repo(
                owner in "[a-zA-Z0-9][a-zA-Z0-9_-]{0,20}",
                repo in "[a-zA-Z0-9][a-zA-Z0-9_-]{0,20}"
            ) {
                let url = format!("https://github.com/{owner}/{repo}");
                let result = github_repo_candidate_from_url(&url);
                assert!(
                    matches!(result, Some(GitHubRepoCandidate::Repo(_))),
                    "expected Repo for {url}, got {result:?}"
                );
            }

            /// `.git` suffix is stripped — with and without `.git` parse identically.
            #[test]
            fn git_suffix_stripped(
                owner in "[a-zA-Z0-9][a-zA-Z0-9_-]{0,10}",
                repo in "[a-zA-Z0-9][a-zA-Z0-9_-]{0,10}"
            ) {
                let with_git = format!("https://github.com/{owner}/{repo}.git");
                let without_git = format!("https://github.com/{owner}/{repo}");
                assert_eq!(
                    github_repo_candidate_from_url(&with_git),
                    github_repo_candidate_from_url(&without_git)
                );
            }

            /// Whitespace-padded URLs parse identically to trimmed.
            #[test]
            fn whitespace_padded_url(
                owner in "[a-zA-Z0-9]{1,10}",
                repo in "[a-zA-Z0-9]{1,10}",
                spaces in "[ \\t]{0,5}"
            ) {
                let clean = format!("https://github.com/{owner}/{repo}");
                let padded = format!("{spaces}{clean}{spaces}");
                assert_eq!(
                    github_repo_candidate_from_url(&clean),
                    github_repo_candidate_from_url(&padded)
                );
            }

            /// Empty/whitespace input returns `None`.
            #[test]
            fn empty_input_returns_none(ws in "[ \\t\\n]{0,10}") {
                assert!(github_repo_candidate_from_url(&ws).is_none());
            }

            /// Non-github.com URLs return `None`.
            #[test]
            fn non_github_returns_none(
                host in "[a-z]{3,10}\\.(com|org|io)",
                path in "[a-z]{1,10}/[a-z]{1,10}"
            ) {
                // Skip if we accidentally generated github.com
                let url = format!("https://{host}/{path}");
                if host != "github.com" {
                    assert!(github_repo_candidate_from_url(&url).is_none());
                }
            }

            /// `full_name` always has format `owner/repo`.
            #[test]
            fn full_name_format(
                owner in "[a-zA-Z0-9]{1,15}",
                repo in "[a-zA-Z0-9]{1,15}"
            ) {
                let r = GitHubRepoRef {
                    owner: owner.clone(),
                    repo: repo.clone(),
                };
                let full = r.full_name();
                assert_eq!(full, format!("{owner}/{repo}"));
                assert!(full.contains('/'));
            }

            /// `github_repo_guesses_from_slug` never panics.
            #[test]
            fn slug_guesses_never_panics(s in ".{0,100}") {
                let _ = github_repo_guesses_from_slug(&s);
            }

            /// Slug guesses all have non-empty owner and repo.
            #[test]
            fn slug_guesses_fields_nonempty(slug in "[a-zA-Z0-9_-]{1,30}") {
                for guess in github_repo_guesses_from_slug(&slug) {
                    assert!(!guess.owner.is_empty());
                    assert!(!guess.repo.is_empty());
                }
            }

            /// Slugs without hyphens produce no guesses.
            #[test]
            fn slug_no_hyphen_empty(slug in "[a-zA-Z0-9]{1,20}") {
                assert!(
                    github_repo_guesses_from_slug(&slug).is_empty(),
                    "expected no guesses for hyphenless slug: {slug}"
                );
            }

            /// `parse_github_repo_response` preserves numeric fields.
            #[test]
            fn github_response_preserves_values(
                stars in 0u64..10_000_000,
                forks in 0u64..1_000_000,
                issues in 0u64..100_000
            ) {
                let json = format!(
                    r#"{{"full_name":"o/r","stargazers_count":{stars},"forks_count":{forks},"open_issues_count":{issues}}}"#
                );
                let m = parse_github_repo_response(&json).unwrap();
                assert_eq!(m.stars, stars);
                assert_eq!(m.forks, forks);
                assert_eq!(m.open_issues, issues);
            }

            /// `parse_github_repo_response` fails on invalid JSON.
            #[test]
            fn github_response_invalid_json(s in "[a-z]{5,20}") {
                assert!(parse_github_repo_response(&s).is_err());
            }

            /// `parse_npm_downloads_response` returns `None` when error field is present.
            #[test]
            fn npm_downloads_error_returns_none(msg in "[a-z ]{1,30}") {
                let json = format!(r#"{{"error":"{msg}","downloads":42}}"#);
                assert_eq!(parse_npm_downloads_response(&json).unwrap(), None);
            }

            /// `parse_npm_downloads_response` preserves download count.
            #[test]
            fn npm_downloads_value_preserved(n in 0u64..100_000_000) {
                let json = format!(r#"{{"downloads":{n}}}"#);
                assert_eq!(parse_npm_downloads_response(&json).unwrap(), Some(n));
            }

            /// `parse_npm_downloads_response` fails on invalid JSON.
            #[test]
            fn npm_downloads_invalid_json(s in "[a-z]{5,20}") {
                assert!(parse_npm_downloads_response(&s).is_err());
            }

            /// `parse_npm_registry_response` extracts repository URL from string form.
            #[test]
            fn npm_registry_string_repo_url(url in "https://[a-z]{3,10}\\.com/[a-z]{1,10}") {
                let json = format!(r#"{{"repository":"{url}"}}"#);
                let meta = parse_npm_registry_response(&json).unwrap();
                assert_eq!(meta.repository_url.as_deref(), Some(url.as_str()));
            }

            /// `parse_npm_registry_response` extracts repository URL from object form.
            #[test]
            fn npm_registry_object_repo_url(url in "https://[a-z]{3,10}\\.com/[a-z]{1,10}") {
                let json = format!(r#"{{"repository":{{"type":"git","url":"{url}"}}}}"#);
                let meta = parse_npm_registry_response(&json).unwrap();
                assert_eq!(meta.repository_url.as_deref(), Some(url.as_str()));
            }

            /// `parse_npm_registry_response` fails on invalid JSON.
            #[test]
            fn npm_registry_invalid_json(s in "[a-z]{5,20}") {
                assert!(parse_npm_registry_response(&s).is_err());
            }

            /// `PopularityEvidence` serde roundtrip.
            #[test]
            fn popularity_evidence_serde_roundtrip(
                stars in prop::option::of(0u64..1_000_000),
                forks in prop::option::of(0u64..100_000),
                weekly in prop::option::of(0u64..10_000_000)
            ) {
                let ev = PopularityEvidence {
                    github_stars: stars,
                    github_forks: forks,
                    npm_downloads_weekly: weekly,
                    ..PopularityEvidence::default()
                };
                let json = serde_json::to_string(&ev).unwrap();
                let back: PopularityEvidence = serde_json::from_str(&json).unwrap();
                assert_eq!(back.github_stars, stars);
                assert_eq!(back.github_forks, forks);
                assert_eq!(back.npm_downloads_weekly, weekly);
            }
        }
    }
}
