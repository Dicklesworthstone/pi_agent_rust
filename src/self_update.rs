//! Verified in-place self-updater for Pi (bd-cv653.7.10).
//!
//! Provides `pi self-update [--version <tag>] [--check]` with:
//! - Package manager detection (Homebrew, APT, Pacman, Nix, Cargo) with refusal & remediation
//! - Pinned Minisign authentication of `SHA256SUMS` and the selected binary
//! - SHA-256 verification against the authenticated manifest (fail-closed)
//! - Multi-lane artifact resolution (DSR bare-binary naming and release archives)
//! - Atomic swap with rollback on failed post-update smoke test
//! - Idempotent no-op when already on the target version

use std::collections::HashMap;
use std::env;
use std::fs::{self, File};
// Only the Unix arm sets a mode on the staged binary.
#[cfg(unix)]
use std::fs::Permissions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use blake2::Blake2b512;
use ring::signature::{ED25519, UnparsedPublicKey};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::http::client::Client;
use crate::version_check::{CURRENT_VERSION, is_newer};

const RELEASES_API_BASE: &str =
    "https://api.github.com/repos/Dicklesworthstone/pi_agent_rust/releases";
const RELEASES_DOWNLOAD_BASE: &str =
    "https://github.com/Dicklesworthstone/pi_agent_rust/releases/download";

/// Redirect hops followed per request. GitHub serves a release asset through
/// one hop (`github.com/.../releases/download/...` -> a signed
/// `release-assets.githubusercontent.com` / `objects.githubusercontent.com`
/// URL); a renamed repository adds one more.
const MAX_REDIRECTS: usize = 5;

/// Largest release binary the updater downloads. The whole body is held in
/// memory and checksummed before it replaces the executable, so this bounds
/// memory. v0.8.0's raw binaries are 52 to 78 MB; the earlier 64 MiB limit
/// refused the Linux x86_64, macOS Intel and Windows ones, so `pi self-update`
/// could not install them.
const MAX_RELEASE_BINARY_BYTES: usize = 256 * 1024 * 1024;

const MAX_RELEASE_MANIFEST_BYTES: usize = 1024 * 1024;
const MAX_CHECKSUM_BYTES: usize = 1024 * 1024;
const MAX_SIGNATURE_BYTES: usize = 16 * 1024;
const RELEASE_PUBLIC_KEY: &str = include_str!("../minisign.pub");

fn decode_minisign_field<const N: usize>(encoded: &str, field: &str) -> Result<[u8; N]> {
    BASE64_STANDARD
        .decode(encoded)
        .map_err(|_| Error::Validation(format!("Invalid Minisign {field} encoding (fail-closed)")))?
        .try_into()
        .map_err(|_| Error::Validation(format!("Invalid Minisign {field} length (fail-closed)")))
}

/// Verify the modern Minisign format with established cryptographic primitives.
/// Format authority: <https://jedisct1.github.io/minisign/#signature-format>.
/// ED signs BLAKE2b-512(message), then a second Ed25519 signature authenticates
/// the raw 64-byte signature and the exact trusted comment without its prefix.
/// Legacy non-prehashed signatures are deliberately refused, as by minisign -H.
fn verify_minisign(bytes: &[u8], signature: &[u8], public_key: &str) -> Result<String> {
    if signature.len() > MAX_SIGNATURE_BYTES {
        return Err(Error::Validation(
            "Minisign signature exceeds size limit (fail-closed)".to_string(),
        ));
    }
    let mut key_lines = public_key.lines();
    if !key_lines
        .next()
        .is_some_and(|line| line.starts_with("untrusted comment: "))
    {
        return Err(Error::Validation(
            "Invalid pinned Minisign public key (fail-closed)".to_string(),
        ));
    }
    let key = decode_minisign_field::<42>(key_lines.next().unwrap_or_default(), "public key")?;
    if key_lines.next().is_some() || &key[..2] != b"Ed" {
        return Err(Error::Validation(
            "Invalid pinned Minisign public key (fail-closed)".to_string(),
        ));
    }
    let signature = std::str::from_utf8(signature).map_err(|_| {
        Error::Validation("Minisign signature is not UTF-8 (fail-closed)".to_string())
    })?;
    let mut lines = signature.lines();
    if !lines
        .next()
        .is_some_and(|line| line.starts_with("untrusted comment: "))
    {
        return Err(Error::Validation(
            "Missing Minisign untrusted comment (fail-closed)".to_string(),
        ));
    }
    let packet = decode_minisign_field::<74>(lines.next().unwrap_or_default(), "signature")?;
    if &packet[..2] != b"ED" {
        return Err(Error::Validation(
            "Minisign signature must use prehashed ED format (fail-closed)".to_string(),
        ));
    }
    if packet[2..10] != key[2..10] {
        return Err(Error::Validation(
            "Minisign signature does not match the pinned release key (fail-closed)".to_string(),
        ));
    }
    let comment = lines
        .next()
        .and_then(|line| line.strip_prefix("trusted comment: "))
        .ok_or_else(|| {
            Error::Validation("Missing Minisign trusted comment (fail-closed)".to_string())
        })?;
    if comment.contains(['\r', '\0']) {
        return Err(Error::Validation(
            "Invalid Minisign trusted comment (fail-closed)".to_string(),
        ));
    }
    let global =
        decode_minisign_field::<64>(lines.next().unwrap_or_default(), "comment signature")?;
    if lines.next().is_some() {
        return Err(Error::Validation(
            "Unexpected trailing Minisign data (fail-closed)".to_string(),
        ));
    }
    let verifier = UnparsedPublicKey::new(&ED25519, &key[10..]);
    verifier
        .verify(Blake2b512::digest(bytes).as_slice(), &packet[10..])
        .map_err(|_| {
            Error::Validation(
                "Minisign release signature verification failed (fail-closed)".to_string(),
            )
        })?;
    let mut authenticated_comment = Vec::with_capacity(64 + comment.len());
    authenticated_comment.extend_from_slice(&packet[10..]);
    authenticated_comment.extend_from_slice(comment.as_bytes());
    verifier
        .verify(&authenticated_comment, &global)
        .map_err(|_| {
            Error::Validation(
                "Minisign trusted comment signature verification failed (fail-closed)".to_string(),
            )
        })?;
    Ok(comment.to_string())
}

/// DSR signs this exact context for aggregate checksums and additional raw
/// binaries. Bind the requested tag and artifact role as well as its bytes so
/// an older authentic release cannot be relabeled as the requested version.
/// Producer contract: `doodlestein_self_releaser/dsr` at commit
/// `fd7e39d809ef413bf6ec3b55dc0a3d2ee13313d9`, strict release signing path.
fn verify_release_signature(
    bytes: &[u8],
    signature: &[u8],
    public_key: &str,
    tag: &str,
    role: &str,
) -> Result<()> {
    let comment = verify_minisign(bytes, signature, public_key)?;
    let digest = crate::package_manager::hex_encode(&Sha256::digest(bytes));
    let expected = format!("dsr strict release pi_agent_rust {tag} {role} {digest}");
    if comment != expected {
        return Err(Error::Validation(format!(
            "Minisign signature is not for pi_agent_rust release {tag} ({role}) (fail-closed)"
        )));
    }
    Ok(())
}

/// True when the response declares a `Content-Length` above `limit`, so an
/// oversized binary is refused before any of it is read.
fn declared_length_exceeds(headers: &[(String, String)], limit: usize) -> bool {
    headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("content-length")
            && value
                .trim()
                .parse::<u64>()
                .is_ok_and(|length| length > limit as u64)
    })
}

/// Resolve a redirect `Location` against the URL that returned it.
///
/// Accepts absolute `http(s)://` URLs, scheme-relative `//host/...`,
/// absolute paths and relative paths. Refuses any other scheme and any hop
/// from `https` to plain `http`, so a redirect can never downgrade the
/// transport the checksums and binaries arrive over.
pub fn resolve_redirect(current: &str, location: &str) -> Result<String> {
    let location = location.trim();
    if location.is_empty() {
        return Err(Error::Validation(
            "redirect without a Location header".to_string(),
        ));
    }
    let (current_scheme, current_rest) = split_scheme(current).ok_or_else(|| {
        Error::Validation(format!(
            "cannot follow a redirect from non-http URL {current}"
        ))
    })?;
    let authority_end = current_rest
        .find(['/', '?', '#'])
        .unwrap_or(current_rest.len());
    let authority = &current_rest[..authority_end];

    let next = if let Some((scheme, rest)) = split_scheme(location) {
        format!("{scheme}://{rest}")
    } else if let Some(rest) = location.strip_prefix("//") {
        format!("{current_scheme}://{rest}")
    } else if location.starts_with('/') {
        format!("{current_scheme}://{authority}{location}")
    } else if location
        .split(['/', '?', '#'])
        .next()
        .is_some_and(|first| first.contains(':'))
    {
        return Err(Error::Validation(format!(
            "refusing redirect to unsupported URL scheme: {location}"
        )));
    } else {
        let path = &current_rest[authority_end..];
        let path = path.split(['?', '#']).next().unwrap_or("");
        let dir = path.rfind('/').map_or("/", |idx| &path[..=idx]);
        format!("{current_scheme}://{authority}{dir}{location}")
    };
    // A fragment is never sent to the server.
    let next = next.split('#').next().unwrap_or_default().to_string();

    let (next_scheme, next_rest) = split_scheme(&next).ok_or_else(|| {
        Error::Validation(format!("refusing redirect to unsupported URL: {location}"))
    })?;
    if next_rest.is_empty() || next_rest.starts_with('/') {
        return Err(Error::Validation(format!(
            "refusing redirect without a host: {location}"
        )));
    }
    if current_scheme == "https" && next_scheme != "https" {
        return Err(Error::Validation(format!(
            "refusing redirect that downgrades https to {next_scheme}: {location}"
        )));
    }
    Ok(next)
}

/// Split `scheme://rest` for the two schemes the updater speaks, lowercasing
/// the scheme. Returns `None` for anything else.
fn split_scheme(url: &str) -> Option<(&'static str, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme.eq_ignore_ascii_case("https") {
        Some(("https", rest))
    } else if scheme.eq_ignore_ascii_case("http") {
        Some(("http", rest))
    } else {
        None
    }
}

const fn is_redirect_status(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// Outcome of a GET that follows redirects: a policy refusal is a hard
/// error, while a transport failure is returned inside `Ok` so that callers
/// probing several candidate assets can move on to the next one.
type Fetched = std::result::Result<crate::http::client::Response, String>;

/// Known package managers that might manage the `pi` binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageManager {
    Homebrew,
    Apt,
    Pacman,
    Nix,
    Cargo,
    Manual,
}

impl PackageManager {
    /// Detect if the binary at `exe_path` appears to be managed by a package manager.
    pub fn detect(exe_path: &Path) -> Self {
        let path_str = exe_path.to_string_lossy();
        if path_str.contains("/Cellar/")
            || path_str.contains("/opt/homebrew/")
            || path_str.contains("/usr/local/Cellar/")
        {
            Self::Homebrew
        } else if path_str.contains("/nix/store/") {
            Self::Nix
        } else if path_str.contains("/.cargo/bin/") {
            Self::Cargo
        } else if (path_str.starts_with("/usr/bin/") || path_str.starts_with("/bin/"))
            && Path::new("/var/lib/dpkg/info").is_dir()
            && std::process::Command::new("dpkg")
                .args(["-S", &path_str])
                .output()
                .is_ok_and(|out| out.status.success())
        {
            // A binary in /usr/bin is APT-managed only when dpkg actually
            // owns it; a manual `sudo cp` install there must stay Manual or
            // the suggested `apt install --only-upgrade` can never work.
            Self::Apt
        } else {
            Self::Manual
        }
    }

    /// Suggested upgrade command if managed externally.
    pub const fn upgrade_command(&self) -> Option<&'static str> {
        match self {
            Self::Homebrew => Some("brew upgrade pi"),
            Self::Apt => Some("sudo apt update && sudo apt install --only-upgrade pi-agent-rust"),
            Self::Pacman => Some("sudo pacman -Syu pi-agent-rust"),
            Self::Nix => Some("nix-channel --update && nix-env -u pi"),
            Self::Cargo => {
                Some("cargo install --git https://github.com/Dicklesworthstone/pi_agent_rust pi")
            }
            Self::Manual => None,
        }
    }
}

/// Information about the current platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformInfo {
    pub os: &'static str,
    pub arch: &'static str,
    pub asset_platform: &'static str,
    pub target_triple: &'static str,
    pub exe_ext: &'static str,
}

impl PlatformInfo {
    /// Detect the current runtime platform.
    pub fn current() -> Option<Self> {
        let os = env::consts::OS;
        let arch = env::consts::ARCH;

        let (asset_platform, target_triple, exe_ext) = match (os, arch) {
            ("macos", "aarch64") => ("darwin-arm64", "aarch64-apple-darwin", ""),
            ("macos", "x86_64") => ("darwin-amd64", "x86_64-apple-darwin", ""),
            ("linux", "x86_64") => ("linux-amd64", "x86_64-unknown-linux-gnu", ""),
            ("linux", "aarch64") => ("linux-arm64", "aarch64-unknown-linux-gnu", ""),
            ("windows", "x86_64") => ("windows-amd64", "x86_64-pc-windows-msvc", ".exe"),
            _ => return None,
        };

        Some(Self {
            os,
            arch,
            asset_platform,
            target_triple,
            exe_ext,
        })
    }

    /// Generate candidate asset filenames in order of preference.
    pub fn candidate_asset_names(&self, version: &str) -> Vec<String> {
        let mut candidates = Vec::new();

        // 1. DSR bare-binary naming (e.g. pi_darwin_arm64, pi_linux_amd64)
        let dsr_platform = self.asset_platform.replace('-', "_");
        candidates.push(format!("pi_{dsr_platform}{}", self.exe_ext));

        // 2. Bare binary name
        candidates.push(format!("pi{}", self.exe_ext));

        // 3. Target triple naming (e.g. pi-v0.1.0-aarch64-apple-darwin)
        candidates.push(format!(
            "pi-{version}-{}{}",
            self.target_triple, self.exe_ext
        ));
        candidates.push(format!("pi-{}{}", self.target_triple, self.exe_ext));

        // Archives (pi-<platform>.tar.xz/.zip) are deliberately NOT
        // candidates: nothing here extracts them, so an archive "install"
        // could only produce a broken binary and a rollback. Releases must
        // carry raw per-platform binaries under the names above (the
        // release pipeline uploads them alongside the archives).

        candidates
    }
}

/// Checksum map parsed from `SHA256SUMS`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChecksumMap {
    pub entries: HashMap<String, String>,
}

fn parse_checksum_line(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let mut parts = line.split_whitespace();
    if let (Some(hash), Some(filename)) = (parts.next(), parts.next()) {
        let clean_filename = filename.trim_start_matches('*');
        Some((clean_filename.to_string(), hash.to_lowercase()))
    } else {
        None
    }
}

impl ChecksumMap {
    /// Parse standard SHA256SUMS file content.
    pub fn parse(content: &str) -> Self {
        let entries = content.lines().filter_map(parse_checksum_line).collect();
        Self { entries }
    }

    /// Parse an authenticated release manifest without ambiguous duplicate rows.
    fn parse_release(content: &[u8]) -> Result<Self> {
        let content = std::str::from_utf8(content).map_err(|_| {
            Error::Validation("Authenticated SHA256SUMS is not UTF-8".to_string())
        })?;
        let mut entries = HashMap::new();
        for line in content.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut fields = line.split_whitespace();
            let hash = fields.next().unwrap_or_default();
            let filename = fields.next().unwrap_or_default().trim_start_matches('*');
            if hash.len() != 64
                || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                || filename.is_empty()
                || filename.contains(['/', '\\', '\0'])
                || matches!(filename, "." | "..")
                || fields.next().is_some()
            {
                return Err(Error::Validation(
                    "Invalid SHA256SUMS entry (fail-closed)".to_string(),
                ));
            }
            if entries
                .insert(filename.to_string(), hash.to_ascii_lowercase())
                .is_some()
            {
                return Err(Error::Validation(format!(
                    "Duplicate SHA256SUMS entry for {filename} (fail-closed)"
                )));
            }
        }
        if entries.is_empty() {
            return Err(Error::Validation(
                "SHA256SUMS contains no release checksums (fail-closed)".to_string(),
            ));
        }
        Ok(Self { entries })
    }

    /// Look up expected checksum for a candidate asset.
    pub fn get_hash(&self, asset_name: &str) -> Option<&str> {
        self.entries.get(asset_name).map(String::as_str)
    }

    /// Verify a byte slice against expected hash.
    pub fn verify_bytes(&self, asset_name: &str, bytes: &[u8]) -> Result<()> {
        let expected = self.get_hash(asset_name).ok_or_else(|| {
            Error::Validation(format!(
                "No checksum found for {asset_name} in SHA256SUMS (fail-closed)"
            ))
        })?;

        let actual_hash = crate::package_manager::hex_encode(&Sha256::digest(bytes)).to_lowercase();
        if actual_hash != expected {
            return Err(Error::Validation(format!(
                "Checksum mismatch for {asset_name}: expected {expected}, got {actual_hash} (fail-closed)"
            )));
        }

        Ok(())
    }
}

/// Options configuring a self-update operation.
#[derive(Debug, Clone, Default)]
pub struct SelfUpdateOptions {
    pub version: Option<String>,
    pub check: bool,
    pub custom_manifest_url: Option<String>,
    pub custom_download_base: Option<String>,
}

/// Result of a self-update execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelfUpdateStatus {
    AlreadyUpToDate {
        current_version: String,
    },
    CheckResult {
        current_version: String,
        latest_version: String,
        is_newer: bool,
        manager: PackageManager,
    },
    ManagedExternally {
        manager: PackageManager,
        upgrade_command: String,
    },
    Updated {
        previous_version: String,
        new_version: String,
        backup_path: PathBuf,
    },
}

/// In-place self-updater engine.
pub struct SelfUpdater {
    client: Client,
    // Only unit tests may supply a signing fixture. Production always embeds
    // the repository key; download URLs never control signature authority.
    #[cfg(test)]
    test_public_key: Option<String>,
}

impl Default for SelfUpdater {
    fn default() -> Self {
        Self::new()
    }
}

impl SelfUpdater {
    pub fn new() -> Self {
        Self {
            client: Client::new(),
            #[cfg(test)]
            test_public_key: None,
        }
    }

    fn release_public_key(&self) -> &str {
        #[cfg(test)]
        if let Some(key) = &self.test_public_key {
            return key;
        }
        RELEASE_PUBLIC_KEY
    }

    /// GET `url`, following up to [`MAX_REDIRECTS`] redirects.
    ///
    /// Only `User-Agent` and `Accept` are sent, and both are safe to repeat
    /// on another host. Redirect policy violations (a downgrade to `http`,
    /// an unsupported scheme, a missing `Location`, too many hops) are
    /// returned as `Err`; transport failures as `Ok(Err(_))`.
    async fn get_following_redirects(&self, url: &str, accept: Option<&str>) -> Result<Fetched> {
        let mut current = url.to_string();
        for _ in 0..=MAX_REDIRECTS {
            let mut request = self
                .client
                .get(&current)
                .header("User-Agent", "pi-agent-rust-self-updater");
            if let Some(accept) = accept {
                request = request.header("Accept", accept);
            }
            let response = match request.send().await {
                Ok(response) => response,
                Err(err) => return Ok(Err(format!("request to {current} failed: {err}"))),
            };
            if !is_redirect_status(response.status()) {
                return Ok(Ok(response));
            }
            let location = response
                .headers()
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("location"))
                .map(|(_, value)| value.clone())
                .unwrap_or_default();
            current = resolve_redirect(&current, &location)?;
        }
        Err(Error::Validation(format!(
            "too many redirects (more than {MAX_REDIRECTS}) fetching {url}"
        )))
    }

    /// Required release metadata never falls back after an absent, oversized,
    /// truncated, or unsuccessful response. Bound decoded bodies as well as
    /// declared lengths, including chunked responses without Content-Length.
    async fn fetch_required_bytes(&self, url: &str, label: &str, limit: usize) -> Result<Vec<u8>> {
        let response = self
            .get_following_redirects(url, None)
            .await?
            .map_err(|error| {
                Error::Validation(format!("Failed to fetch {label}: {error} (fail-closed)"))
            })?;
        if !(200..300).contains(&response.status()) {
            return Err(Error::Validation(format!(
                "{label} download failed with HTTP status {} (fail-closed)",
                response.status()
            )));
        }
        if declared_length_exceeds(response.headers(), limit) {
            return Err(Error::Validation(format!(
                "{label} exceeds its {limit}-byte size limit (fail-closed)"
            )));
        }
        response.bytes_limited(limit).await.map_err(|error| {
            Error::Validation(format!("Failed to read bounded {label}: {error} (fail-closed)"))
        })
    }

    /// Fetch latest release tag name from GitHub.
    pub async fn fetch_latest_version(&self, manifest_url: Option<&str>) -> Result<String> {
        let url = manifest_url.unwrap_or(RELEASES_API_BASE);
        let api_url = if url.ends_with("/latest") || url.contains("/releases/") {
            url.to_string()
        } else {
            format!("{url}/latest")
        };

        let response = self
            .get_following_redirects(&api_url, Some("application/vnd.github.v3+json"))
            .await?
            .map_err(|e| Error::Validation(format!("Failed to fetch release manifest: {e}")))?;

        if !(200..300).contains(&response.status()) {
            return Err(Error::Validation(format!(
                "Release manifest request failed with status: {}",
                response.status()
            )));
        }

        if declared_length_exceeds(response.headers(), MAX_RELEASE_MANIFEST_BYTES) {
            return Err(Error::Validation(
                "Release manifest exceeds its size limit".to_string(),
            ));
        }
        let body = response
            .bytes_limited(MAX_RELEASE_MANIFEST_BYTES)
            .await
            .map_err(|e| Error::Validation(format!("Failed to read release manifest body: {e}")))?;

        let val: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|e| Error::Validation(format!("Invalid release JSON response: {e}")))?;

        let tag = val
            .get("tag_name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::Validation("tag_name missing in release response".to_string()))?;

        Ok(tag.trim_start_matches('v').to_string())
    }

    /// Authenticate `SHA256SUMS` with the embedded release key before parsing it.
    pub async fn fetch_checksums(
        &self,
        version: &str,
        custom_base: Option<&str>,
    ) -> Result<ChecksumMap> {
        let base = custom_base.unwrap_or(RELEASES_DOWNLOAD_BASE);
        let tag = if version.starts_with('v') {
            version.to_string()
        } else {
            format!("v{version}")
        };
        let sums_url = format!("{base}/{tag}/SHA256SUMS");

        let body = self
            .fetch_required_bytes(&sums_url, "SHA256SUMS", MAX_CHECKSUM_BYTES)
            .await?;
        let signature = self
            .fetch_required_bytes(
                &format!("{sums_url}.minisig"),
                "SHA256SUMS.minisig",
                MAX_SIGNATURE_BYTES,
            )
            .await?;
        verify_release_signature(
            &body,
            &signature,
            self.release_public_key(),
            &tag,
            "aggregate",
        )?;
        ChecksumMap::parse_release(&body)
    }

    async fn try_download_candidate(
        &self,
        base: &str,
        tag: &str,
        candidate: &str,
        checksums: &ChecksumMap,
    ) -> Result<Option<Vec<u8>>> {
        let url = format!("{base}/{tag}/{candidate}");
        let Ok(res) = self.get_following_redirects(&url, None).await? else {
            return Ok(None);
        };

        if !(200..300).contains(&res.status()) {
            return Ok(None);
        }

        if declared_length_exceeds(res.headers(), MAX_RELEASE_BINARY_BYTES) {
            return Ok(None);
        }

        let Ok(bytes) = res.bytes_limited(MAX_RELEASE_BINARY_BYTES).await else {
            return Ok(None);
        };

        checksums.verify_bytes(candidate, &bytes)?;
        let signature = self
            .fetch_required_bytes(
                &format!("{url}.minisig"),
                &format!("{candidate}.minisig"),
                MAX_SIGNATURE_BYTES,
            )
            .await?;
        verify_release_signature(
            &bytes,
            &signature,
            self.release_public_key(),
            tag,
            "additional",
        )?;
        Ok(Some(bytes))
    }

    /// Download a binary and verify both its SHA-256 and pinned Minisign signature.
    /// A present candidate with a missing or invalid signature is a hard refusal.
    pub async fn download_and_verify(
        &self,
        platform: &PlatformInfo,
        version: &str,
        checksums: &ChecksumMap,
        custom_base: Option<&str>,
    ) -> Result<(String, Vec<u8>)> {
        let base = custom_base.unwrap_or(RELEASES_DOWNLOAD_BASE);
        let tag = if version.starts_with('v') {
            version.to_string()
        } else {
            format!("v{version}")
        };

        let candidates = platform.candidate_asset_names(version);

        for candidate in candidates {
            if let Some(bytes) = self
                .try_download_candidate(base, &tag, &candidate, checksums)
                .await?
            {
                return Ok((candidate, bytes));
            }
        }

        Err(Error::Validation(format!(
            "No compatible binary candidate found for platform {} in release {tag}",
            platform.asset_platform
        )))
    }

    /// Perform atomic binary swap on the current executable.
    pub fn perform_atomic_swap(exe_path: &Path, new_binary_bytes: &[u8]) -> Result<PathBuf> {
        let parent_dir = exe_path.parent().unwrap_or_else(|| Path::new("."));
        let pid = std::process::id();
        let tmp_path = parent_dir.join(format!(".pi-update-tmp.{pid}"));
        let backup_path = parent_dir.join(format!(".pi-update-backup.{pid}"));

        // 1. Write new binary to tmp file
        {
            let mut tmp_file = File::create(&tmp_path).map_err(|e| {
                Error::Io(Box::new(std::io::Error::other(format!(
                    "Failed to create temporary update file: {e}"
                ))))
            })?;
            tmp_file.write_all(new_binary_bytes).map_err(|e| {
                Error::Io(Box::new(std::io::Error::other(format!(
                    "Failed to write update bytes: {e}"
                ))))
            })?;
            tmp_file.flush().map_err(|e| {
                Error::Io(Box::new(std::io::Error::other(format!(
                    "Failed to flush update file: {e}"
                ))))
            })?;
        }

        // Set executable permissions on Unix
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = Permissions::from_mode(0o755);
            let _ = fs::set_permissions(&tmp_path, perms);
        }

        // 2. Rename existing executable to backup
        if let Err(e) = fs::rename(exe_path, &backup_path) {
            let _ = fs::remove_file(&tmp_path);
            return Err(Error::Io(Box::new(std::io::Error::other(format!(
                "Failed to backup existing binary {}: {e}",
                exe_path.display()
            )))));
        }

        // 3. Rename tmp to executable
        if let Err(e) = fs::rename(&tmp_path, exe_path) {
            // Restore backup
            let _ = fs::rename(&backup_path, exe_path);
            let _ = fs::remove_file(&tmp_path);
            return Err(Error::Io(Box::new(std::io::Error::other(format!(
                "Failed to install new binary {}: {e}",
                exe_path.display()
            )))));
        }

        // 4. Run smoke test
        let smoke_check = Command::new(exe_path).arg("--version").output();
        let smoke_ok = match smoke_check {
            Ok(output) => output.status.success(),
            Err(_) => false,
        };

        if !smoke_ok {
            // Rollback immediately
            let _ = fs::rename(&backup_path, exe_path);
            return Err(Error::Validation(
                "Post-update smoke test (--version) failed; rolled back to previous binary"
                    .to_string(),
            ));
        }

        Ok(backup_path)
    }

    /// Execute the complete self-update workflow.
    pub async fn run(&self, options: &SelfUpdateOptions) -> Result<SelfUpdateStatus> {
        let current_exe = env::current_exe().map_err(|e| {
            Error::Io(Box::new(std::io::Error::other(format!(
                "Failed to locate current executable path: {e}"
            ))))
        })?;

        // Package manager check
        let manager = PackageManager::detect(&current_exe);
        if manager != PackageManager::Manual
            && !options.check
            && let Some(cmd) = manager.upgrade_command()
        {
            return Ok(SelfUpdateStatus::ManagedExternally {
                manager,
                upgrade_command: cmd.to_string(),
            });
        }

        let target_version = match &options.version {
            Some(v) => v.trim_start_matches('v').to_string(),
            None => {
                self.fetch_latest_version(options.custom_manifest_url.as_deref())
                    .await?
            }
        };

        let current_ver = CURRENT_VERSION.trim_start_matches('v');

        if options.check {
            return Ok(SelfUpdateStatus::CheckResult {
                current_version: current_ver.to_string(),
                latest_version: target_version.clone(),
                is_newer: is_newer(current_ver, &target_version),
                manager,
            });
        }

        // The latest-release API is discovery metadata, not a signed freshness
        // guarantee. Replayed metadata must not downgrade an automatic update;
        // an explicit --version remains an intentional version selection.
        if current_ver == target_version
            || (options.version.is_none() && !is_newer(current_ver, &target_version))
        {
            return Ok(SelfUpdateStatus::AlreadyUpToDate {
                current_version: current_ver.to_string(),
            });
        }

        let platform = PlatformInfo::current().ok_or_else(|| {
            Error::Validation(format!(
                "Unsupported operating system or architecture: {} {}",
                env::consts::OS,
                env::consts::ARCH
            ))
        })?;

        let checksums = self
            .fetch_checksums(&target_version, options.custom_download_base.as_deref())
            .await?;

        let (_asset_name, bytes) = self
            .download_and_verify(
                &platform,
                &target_version,
                &checksums,
                options.custom_download_base.as_deref(),
            )
            .await?;

        let backup = Self::perform_atomic_swap(&current_exe, &bytes)?;

        Ok(SelfUpdateStatus::Updated {
            previous_version: current_ver.to_string(),
            new_version: target_version,
            backup_path: backup,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{Ed25519KeyPair, KeyPair as _};

    fn signing_key() -> Ed25519KeyPair {
        // This key is solely a deterministic test fixture, never a release key.
        Ed25519KeyPair::from_seed_unchecked(&[0x41; 32]).unwrap()
    }

    fn test_public_key() -> String {
        let mut packet = b"Edtest-key".to_vec();
        packet.extend_from_slice(signing_key().public_key().as_ref());
        format!(
            "untrusted comment: test fixture\n{}\n",
            BASE64_STANDARD.encode(packet)
        )
    }

    fn sign_fixture(bytes: &[u8], comment: &str) -> Vec<u8> {
        let key = signing_key();
        let signature = key.sign(Blake2b512::digest(bytes).as_slice());
        let mut packet = b"EDtest-key".to_vec();
        packet.extend_from_slice(signature.as_ref());
        let mut authenticated_comment = signature.as_ref().to_vec();
        authenticated_comment.extend_from_slice(comment.as_bytes());
        format!(
            "untrusted comment: test fixture\n{}\ntrusted comment: {comment}\n{}\n",
            BASE64_STANDARD.encode(packet),
            BASE64_STANDARD.encode(key.sign(&authenticated_comment).as_ref())
        )
        .into_bytes()
    }

    fn sign_release(bytes: &[u8], tag: &str, role: &str) -> Vec<u8> {
        let digest = crate::package_manager::hex_encode(&Sha256::digest(bytes));
        sign_fixture(
            bytes,
            &format!("dsr strict release pi_agent_rust {tag} {role} {digest}"),
        )
    }

    fn test_updater() -> SelfUpdater {
        SelfUpdater {
            client: Client::new(),
            test_public_key: Some(test_public_key()),
        }
    }

    fn run_async<T>(future: impl std::future::Future<Output = T>) -> T {
        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap()
            .block_on(future)
    }

    fn response(body: Vec<u8>) -> (String, std::sync::Arc<Vec<u8>>) {
        (
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close",
                body.len()
            ),
            std::sync::Arc::new(body),
        )
    }

    #[test]
    fn test_checksum_map_parser_and_verifier() {
        let sample_sums = r"
# SHA256SUMS for v0.2.0
e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  pi-v0.2.0-x86_64-unknown-linux-gnu
ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad *pi_darwin_arm64
";
        let map = ChecksumMap::parse(sample_sums);
        assert_eq!(
            map.get_hash("pi-v0.2.0-x86_64-unknown-linux-gnu"),
            Some("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
        );
        assert_eq!(
            map.get_hash("pi_darwin_arm64"),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );

        // Verify bytes for "abc" -> ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
        assert!(map.verify_bytes("pi_darwin_arm64", b"abc").is_ok());
        assert!(map.verify_bytes("pi_darwin_arm64", b"corrupted").is_err());
        assert!(map.verify_bytes("unknown_asset", b"abc").is_err());
    }

    #[test]
    // The suffix checks assert candidate NAMES, not filesystem paths; the
    // input is already lowercased.
    #[allow(clippy::case_sensitive_file_extension_comparisons)]
    fn test_platform_detection_and_candidates() {
        if let Some(plat) = PlatformInfo::current() {
            let candidates = plat.candidate_asset_names("0.2.0");
            assert!(!candidates.is_empty());
            assert!(candidates.iter().any(|c| c.contains("pi")));
            // No extractor exists: archive candidates would only ever
            // produce a failed install + rollback.
            assert!(
                candidates.iter().all(|c| {
                    let lower = c.to_ascii_lowercase();
                    !lower.ends_with(".tar.gz")
                        && !lower.ends_with(".tar.xz")
                        && !lower.ends_with(".zip")
                }),
                "{candidates:?}"
            );
        }
    }

    #[test]
    fn redirects_resolve_like_github_release_downloads() {
        let from = "https://github.com/o/r/releases/download/v1.0.0/SHA256SUMS";
        assert_eq!(
            resolve_redirect(
                from,
                "https://release-assets.githubusercontent.com/github-production-release-asset/1?sig=a%2Fb&se=2"
            )
            .unwrap(),
            "https://release-assets.githubusercontent.com/github-production-release-asset/1?sig=a%2Fb&se=2"
        );
        assert_eq!(
            resolve_redirect(from, "HTTPS://objects.githubusercontent.com/x").unwrap(),
            "https://objects.githubusercontent.com/x"
        );
        assert_eq!(
            resolve_redirect(from, "//objects.githubusercontent.com/y").unwrap(),
            "https://objects.githubusercontent.com/y"
        );
        assert_eq!(
            resolve_redirect(from, "/o/r2/releases/download/v1.0.0/SHA256SUMS").unwrap(),
            "https://github.com/o/r2/releases/download/v1.0.0/SHA256SUMS"
        );
        assert_eq!(
            resolve_redirect("https://h.example/a/b/c?q=1", "d?e=2").unwrap(),
            "https://h.example/a/b/d?e=2"
        );
        assert_eq!(
            resolve_redirect("http://127.0.0.1:8080/a", "https://h.example/b").unwrap(),
            "https://h.example/b"
        );
        assert_eq!(
            resolve_redirect("http://127.0.0.1:8080/a/b", "/c").unwrap(),
            "http://127.0.0.1:8080/c"
        );
        assert_eq!(
            resolve_redirect(from, "https://objects.githubusercontent.com/z?s=1#frag").unwrap(),
            "https://objects.githubusercontent.com/z?s=1"
        );
    }

    /// Serve one HTTP response on a loopback port: `head` (status line and
    /// headers, without the blank line) followed by `body`.
    fn serve_once(
        head: String,
        body: std::sync::Arc<Vec<u8>>,
    ) -> (std::net::SocketAddr, std::thread::JoinHandle<()>) {
        serve_responses(vec![(head, body)])
    }

    fn serve_responses(
        responses: Vec<(String, std::sync::Arc<Vec<u8>>)>,
    ) -> (std::net::SocketAddr, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let thread = std::thread::spawn(move || {
            for (head, body) in responses {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
                let mut socket = loop {
                    match listener.accept() {
                        Ok((socket, _)) => break socket,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                std::time::Instant::now() < deadline,
                                "timed out waiting for an expected updater request"
                            );
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                        Err(error) => panic!("accept updater request: {error}"),
                    }
                };
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                    .unwrap();
                socket
                    .set_write_timeout(Some(std::time::Duration::from_secs(10)))
                    .unwrap();
                let mut request = Vec::new();
                let mut byte = [0_u8; 1];
                while !request.ends_with(b"\r\n\r\n") {
                    if std::io::Read::read(&mut socket, &mut byte).unwrap_or(0) == 0 {
                        break;
                    }
                    request.push(byte[0]);
                }
                let _ = socket.write_all(format!("{head}\r\n\r\n").as_bytes());
                let _ = socket.write_all(&body);
            }
        });
        (address, thread)
    }

    fn download_candidate(
        address: std::net::SocketAddr,
        checksums: &ChecksumMap,
    ) -> Option<Vec<u8>> {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            test_updater()
                .try_download_candidate(
                    &format!("http://{address}"),
                    "v9.9.9",
                    "pi_linux_amd64",
                    checksums,
                )
                .await
                .unwrap()
        })
    }

    #[test]
    fn minisign_accepts_the_independent_upstream_prehashed_vector() {
        // From jedisct1/rust-minisign src/tests.rs::verify_prehashed_det,
        // source blob da7ede626f4b3c963544cf02319dc6ba90fb6326.
        // No test signer in this crate generated this compatibility vector.
        let key = concat!(
            "untrusted comment: upstream test key\n",
            "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3\n"
        );
        let signature = concat!(
            "untrusted comment: signature from minisign secret key\n",
            "RUQf6LRCGA9i559r3g7V1qNyJDApGip8MfqcadIgT9CuhV3EMhHoN1mGTkUidF/",
            "z7SrlQgXdy8ofjb7bNJJylDOocrCo8KLzZwo=\n",
            "trusted comment: timestamp:1556193335\tfile:test\n",
            "y/rUw2y8/hOUYjZU71eHp/Wo1KZ40fGy2VJEDl34XMJM+TX48Ss/17u3IvIfbVR1FkZZSNCisQbuQY+bHwhEBg==\n"
        );
        assert!(verify_minisign(b"test", signature.as_bytes(), key).is_ok());
        assert!(verify_minisign(b"changed", signature.as_bytes(), key).is_err());
        assert!(verify_minisign(b"test", signature.as_bytes(), RELEASE_PUBLIC_KEY).is_err());
    }

    #[test]
    fn minisign_requires_the_message_key_and_trusted_comment_signatures() {
        let bytes = b"signed binary";
        let public_key = test_public_key();
        let signature = sign_fixture(bytes, "release v9.9.9");
        assert!(verify_minisign(bytes, &signature, &public_key).is_ok());
        assert!(verify_minisign(b"changed binary", &signature, &public_key).is_err());
        assert!(verify_minisign(bytes, &signature, RELEASE_PUBLIC_KEY).is_err());

        let signature_text = String::from_utf8(signature.clone()).unwrap();
        let crlf = signature_text.replace('\n', "\r\n");
        assert!(verify_minisign(bytes, crlf.as_bytes(), &public_key).is_ok());
        let altered_comment = signature_text.replace("release v9.9.9", "release v8.8.8");
        assert!(verify_minisign(bytes, altered_comment.as_bytes(), &public_key).is_err());

        let mut lines: Vec<String> = signature_text.lines().map(str::to_string).collect();
        let mut packet = BASE64_STANDARD.decode(&lines[1]).unwrap();
        packet[2] ^= 1;
        lines[1] = BASE64_STANDARD.encode(&packet);
        assert!(verify_minisign(bytes, lines.join("\n").as_bytes(), &public_key).is_err());

        packet[2] ^= 1;
        packet[1] = b'd';
        lines[1] = BASE64_STANDARD.encode(&packet);
        let legacy = verify_minisign(bytes, lines.join("\n").as_bytes(), &public_key).unwrap_err();
        assert!(legacy.to_string().contains("prehashed ED"));

        let other = sign_fixture(b"different binary", "release v9.9.9");
        let other = String::from_utf8(other).unwrap();
        let mut lines: Vec<&str> = signature_text.lines().collect();
        lines[3] = other.lines().nth(3).unwrap();
        assert!(verify_minisign(bytes, lines.join("\n").as_bytes(), &public_key).is_err());
        assert!(verify_minisign(bytes, lines[..3].join("\n").as_bytes(), &public_key).is_err());
        assert!(verify_minisign(bytes, b"", &public_key).is_err());
        assert!(
            verify_minisign(bytes, b"untrusted comment: missing\nnot-base64\n", &public_key)
                .is_err()
        );
        assert!(
            verify_minisign(
                bytes,
                format!("{signature_text}extra\n").as_bytes(),
                &public_key,
            )
            .is_err()
        );
        assert!(
            verify_minisign(bytes, &vec![b'a'; MAX_SIGNATURE_BYTES + 1], &public_key).is_err()
        );

        // Matching the public key identifier cannot substitute a different key.
        let mut key_packet = BASE64_STANDARD
            .decode(public_key.lines().nth(1).unwrap())
            .unwrap();
        key_packet[10] ^= 1;
        let other_key = format!(
            "untrusted comment: different key, same id\n{}\n",
            BASE64_STANDARD.encode(key_packet)
        );
        assert!(verify_minisign(bytes, &signature, &other_key).is_err());
    }

    #[test]
    fn release_signatures_bind_the_requested_tool_tag_role_and_hash() {
        let bytes = b"release binary";
        let public_key = test_public_key();
        let signature = sign_release(bytes, "v9.9.9", "additional");
        assert!(
            verify_release_signature(bytes, &signature, &public_key, "v9.9.9", "additional")
                .is_ok()
        );
        assert!(
            verify_release_signature(bytes, &signature, &public_key, "v9.9.10", "additional")
                .is_err()
        );
        assert!(
            verify_release_signature(bytes, &signature, &public_key, "v9.9.9", "aggregate")
                .is_err()
        );
        let digest = crate::package_manager::hex_encode(&Sha256::digest(bytes));
        for comment in [
            format!("dsr strict release another_tool v9.9.9 additional {digest}"),
            format!(
                "dsr strict release pi_agent_rust v9.9.9 additional {}",
                "0".repeat(64)
            ),
            format!("dsr strict release pi_agent_rust v9.9.9 additional {digest} extra"),
        ] {
            let signature = sign_fixture(bytes, &comment);
            assert!(
                verify_release_signature(bytes, &signature, &public_key, "v9.9.9", "additional")
                    .is_err()
            );
        }
    }

    #[test]
    fn signed_checksum_parser_rejects_ambiguous_or_invalid_entries() {
        let digest = crate::package_manager::hex_encode(&Sha256::digest(b"binary"));
        let valid = format!("{digest}  pi_linux_amd64\n");
        let checksums = ChecksumMap::parse_release(valid.as_bytes()).unwrap();
        assert!(checksums.verify_bytes("pi_linux_amd64", b"binary").is_ok());
        for invalid in [
            format!("{valid}{valid}"),
            format!("{digest}  ../pi_linux_amd64\n"),
            format!("{digest}  pi_linux_amd64 extra\n"),
            "not-a-hash  pi_linux_amd64\n".to_string(),
            "# no entries\n".to_string(),
        ] {
            assert!(ChecksumMap::parse_release(invalid.as_bytes()).is_err());
        }
    }

    #[test]
    fn signed_release_download_follows_redirects_and_authenticates_both_files() {
        let bytes = b"release binary".to_vec();
        let digest = crate::package_manager::hex_encode(&Sha256::digest(&bytes));
        let sums = format!("{digest}  pi_linux_amd64\n").into_bytes();
        let sums_signature = sign_release(&sums, "v9.9.9", "aggregate");
        let binary_signature = sign_release(&bytes, "v9.9.9", "additional");
        let redirect = |location: &str| {
            (
                format!(
                    "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close"
                ),
                std::sync::Arc::new(Vec::new()),
            )
        };
        let (address, server) = serve_responses(vec![
            redirect("/objects/SHA256SUMS?sig=a%2Fb"),
            response(sums),
            redirect("/objects/SHA256SUMS.minisig?sig=c"),
            response(sums_signature),
            redirect("/objects/binary-hop"),
            redirect("/objects/binary?sig=d"),
            response(bytes.clone()),
            redirect("/objects/binary.minisig?sig=e"),
            response(binary_signature),
        ]);
        let platform = PlatformInfo {
            os: "linux",
            arch: "x86_64",
            asset_platform: "linux-amd64",
            target_triple: "x86_64-unknown-linux-gnu",
            exe_ext: "",
        };
        let (name, downloaded) = run_async(async {
            let updater = test_updater();
            let base = format!("http://{address}");
            let checksums = updater.fetch_checksums("9.9.9", Some(&base)).await.unwrap();
            updater
                .download_and_verify(&platform, "v9.9.9", &checksums, Some(&base))
                .await
                .unwrap()
        });
        server.join().unwrap();
        assert_eq!(name, "pi_linux_amd64");
        assert_eq!(downloaded, bytes);
    }

    #[test]
    fn missing_or_mismatched_binary_signatures_are_hard_refusals() {
        let bytes = b"release binary".to_vec();
        let digest = crate::package_manager::hex_encode(&Sha256::digest(&bytes));
        let checksums = ChecksumMap::parse(&format!("{digest}  pi_linux_amd64\n"));
        let platform = PlatformInfo {
            os: "linux",
            arch: "x86_64",
            asset_platform: "linux-amd64",
            target_triple: "x86_64-unknown-linux-gnu",
            exe_ext: "",
        };
        for signature_response in [
            (
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close".to_string(),
                std::sync::Arc::new(Vec::new()),
            ),
            response(sign_release(b"different binary", "v9.9.9", "additional")),
            response(sign_release(&bytes, "v8.8.8", "additional")),
            response(sign_release(&bytes, "v9.9.9", "aggregate")),
        ] {
            let (address, server) =
                serve_responses(vec![response(bytes.clone()), signature_response]);
            let result = run_async(test_updater().download_and_verify(
                &platform,
                "9.9.9",
                &checksums,
                Some(&format!("http://{address}")),
            ));
            server.join().unwrap();
            let error = result.unwrap_err().to_string();
            assert!(error.contains("minisig") || error.contains("Minisign"), "{error}");
            assert!(!error.contains("No compatible binary"), "{error}");
        }
    }

    #[test]
    fn checksum_download_authenticates_before_parsing_and_never_skips_signatures() {
        for signature_response in [
            (
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close".to_string(),
                std::sync::Arc::new(Vec::new()),
            ),
            response(sign_release(b"different checksum file", "v9.9.9", "aggregate")),
            response(sign_release(
                b"this is also an invalid checksum file",
                "v8.8.8",
                "aggregate",
            )),
        ] {
            let (address, server) = serve_responses(vec![
                response(b"this is also an invalid checksum file".to_vec()),
                signature_response,
            ]);
            let result = run_async(test_updater().fetch_checksums(
                "9.9.9",
                Some(&format!("http://{address}")),
            ));
            server.join().unwrap();
            let error = result.unwrap_err().to_string();
            assert!(error.contains("minisig") || error.contains("Minisign"), "{error}");
            assert!(!error.contains("Invalid SHA256SUMS entry"), "{error}");
        }
    }

    #[test]
    fn automatic_update_never_downgrades_from_replayed_latest_metadata() {
        assert!(is_newer("0.0.0", CURRENT_VERSION));
        let (head, body) = response(br#"{"tag_name":"v0.0.0"}"#.to_vec());
        let (address, server) = serve_once(head, body);
        let options = SelfUpdateOptions {
            custom_manifest_url: Some(format!("http://{address}/releases/latest")),
            custom_download_base: Some(format!("http://{address}/download")),
            ..SelfUpdateOptions::default()
        };
        let status = run_async(SelfUpdater::new().run(&options)).unwrap();
        server.join().unwrap();
        assert_eq!(
            status,
            SelfUpdateStatus::AlreadyUpToDate {
                current_version: CURRENT_VERSION.trim_start_matches('v').to_string(),
            }
        );
    }

    #[test]
    fn explicit_older_version_still_attempts_authenticated_download() {
        // Unsupported platforms are refused before any release download.
        if PlatformInfo::current().is_none() {
            return;
        }
        let (address, server) = serve_once(
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close".to_string(),
            std::sync::Arc::new(Vec::new()),
        );
        let options = SelfUpdateOptions {
            version: Some("0.0.0".to_string()),
            custom_download_base: Some(format!("http://{address}/download")),
            ..SelfUpdateOptions::default()
        };
        let error = run_async(SelfUpdater::new().run(&options))
            .expect_err("an explicit older version must still request its signed release")
            .to_string();
        server.join().unwrap();
        assert!(error.contains("SHA256SUMS"), "{error}");
    }

    #[test]
    fn release_metadata_bounds_declared_and_streamed_responses() {
        for (label, limit) in [
            ("SHA256SUMS", MAX_CHECKSUM_BYTES),
            ("SHA256SUMS.minisig", MAX_SIGNATURE_BYTES),
        ] {
            for declared in [false, true] {
                let head = if declared {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close",
                        limit + 1
                    )
                } else {
                    "HTTP/1.1 200 OK\r\nConnection: close".to_string()
                };
                let body = if declared {
                    Vec::new()
                } else {
                    vec![b'x'; limit + 1]
                };
                let (address, server) = serve_once(head, std::sync::Arc::new(body));
                let result = run_async(test_updater().fetch_required_bytes(
                    &format!("http://{address}/{label}"),
                    label,
                    limit,
                ));
                server.join().unwrap();
                assert!(result.is_err(), "{label}, declared={declared}");
            }
        }
    }

    #[test]
    fn release_binary_limit_admits_the_largest_published_binary() {
        // pi_linux_amd64 of v0.8.0 is 77,855,560 bytes, above the old 64 MiB.
        const LARGEST_V0_8_0_BINARY: usize = 77_855_560;
        assert!(LARGEST_V0_8_0_BINARY > 64 * 1024 * 1024);
        assert!(MAX_RELEASE_BINARY_BYTES >= 3 * LARGEST_V0_8_0_BINARY);
    }

    #[test]
    fn download_candidate_accepts_a_binary_over_64_mib() {
        let body = std::sync::Arc::new(vec![0x5a_u8; 65 * 1024 * 1024]);
        let digest = crate::package_manager::hex_encode(&Sha256::digest(body.as_slice()));
        let checksums = ChecksumMap::parse(&format!("{digest}  pi_linux_amd64\n"));
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close",
            body.len()
        );
        let signature = sign_release(body.as_slice(), "v9.9.9", "additional");
        let (address, server) = serve_responses(vec![
            (head, std::sync::Arc::clone(&body)),
            response(signature),
        ]);
        let downloaded = download_candidate(address, &checksums);
        server.join().unwrap();
        assert_eq!(downloaded.map(|bytes| bytes.len()), Some(body.len()));
    }

    #[test]
    fn download_candidate_refuses_a_declared_length_over_the_limit() {
        let checksums = ChecksumMap::parse(&format!("{}  pi_linux_amd64\n", "0".repeat(64)));
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close",
            MAX_RELEASE_BINARY_BYTES + 1
        );
        let (address, server) = serve_once(head, std::sync::Arc::new(vec![0_u8; 4096]));
        assert_eq!(download_candidate(address, &checksums), None);
        server.join().unwrap();
        assert!(!declared_length_exceeds(
            &[(
                "content-length".to_string(),
                MAX_RELEASE_BINARY_BYTES.to_string()
            )],
            MAX_RELEASE_BINARY_BYTES
        ));
    }

    #[test]
    fn redirects_never_downgrade_or_leave_http() {
        let from = "https://github.com/o/r/releases/download/v1.0.0/pi_linux_amd64";
        for location in [
            "http://objects.githubusercontent.com/x",
            "HTTP://objects.githubusercontent.com/x",
            "ftp://objects.githubusercontent.com/x",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "https:///no-host",
            "",
            "   ",
        ] {
            assert!(
                resolve_redirect(from, location).is_err(),
                "{location:?} must be refused"
            );
        }
        // Scheme-relative inherits https, so it cannot downgrade either.
        assert!(
            resolve_redirect(from, "//evil.example/x")
                .unwrap()
                .starts_with("https://")
        );
    }

    #[test]
    fn test_package_manager_detection() {
        assert_eq!(
            PackageManager::detect(Path::new("/opt/homebrew/bin/pi")),
            PackageManager::Homebrew
        );
        assert_eq!(
            PackageManager::detect(Path::new("/usr/local/Cellar/pi/0.1.0/bin/pi")),
            PackageManager::Homebrew
        );
        assert_eq!(
            PackageManager::detect(Path::new("/nix/store/xyz-pi/bin/pi")),
            PackageManager::Nix
        );
        assert_eq!(
            PackageManager::detect(Path::new("/home/user/.cargo/bin/pi")),
            PackageManager::Cargo
        );
        assert_eq!(
            PackageManager::detect(Path::new("/home/user/.local/bin/pi")),
            PackageManager::Manual
        );
    }
}
