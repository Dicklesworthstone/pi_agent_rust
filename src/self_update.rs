//! Verified in-place self-updater for Pi (bd-cv653.7.10).
//!
//! Provides `pi self-update [--version <tag>] [--check]` with:
//! - Package manager detection (Homebrew, APT, Pacman, Nix, Cargo) with refusal & remediation
//! - SHA-256 checksum verification against `SHA256SUMS` (fail-closed)
//! - Multi-lane artifact resolution (DSR bare-binary naming and release archives)
//! - Atomic swap with rollback on failed post-update smoke test
//! - Idempotent no-op when already on the target version

use std::collections::HashMap;
use std::env;
use std::fs::{self, File, Permissions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::http::client::Client;
use crate::version_check::{CURRENT_VERSION, is_newer};

const RELEASES_API_BASE: &str =
    "https://api.github.com/repos/Dicklesworthstone/pi_agent_rust/releases";
const RELEASES_DOWNLOAD_BASE: &str =
    "https://github.com/Dicklesworthstone/pi_agent_rust/releases/download";

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
        } else if path_str.starts_with("/usr/bin/")
            || path_str.starts_with("/bin/")
            || path_str.starts_with("/usr/local/bin/dpkg")
        {
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

        // 4. Archive format naming
        if self.exe_ext.is_empty() {
            candidates.push(format!("pi-{}.tar.gz", self.asset_platform));
        } else {
            candidates.push(format!("pi-{}.zip", self.asset_platform));
        }

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
        }
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
            .client
            .get(&api_url)
            .header("User-Agent", "pi-agent-rust-self-updater")
            .header("Accept", "application/vnd.github.v3+json")
            .send()
            .await
            .map_err(|e| Error::Validation(format!("Failed to fetch release manifest: {e}")))?;

        if !(200..300).contains(&response.status()) {
            return Err(Error::Validation(format!(
                "Release manifest request failed with status: {}",
                response.status()
            )));
        }

        let body = response
            .text()
            .await
            .map_err(|e| Error::Validation(format!("Failed to read release manifest body: {e}")))?;

        let val: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| Error::Validation(format!("Invalid release JSON response: {e}")))?;

        let tag = val
            .get("tag_name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::Validation("tag_name missing in release response".to_string()))?;

        Ok(tag.trim_start_matches('v').to_string())
    }

    /// Fetch `SHA256SUMS` from the release assets.
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

        let response = self
            .client
            .get(&sums_url)
            .header("User-Agent", "pi-agent-rust-self-updater")
            .send()
            .await
            .map_err(|e| {
                Error::Validation(format!("Failed to fetch SHA256SUMS from {sums_url}: {e}"))
            })?;

        if !(200..300).contains(&response.status()) {
            return Err(Error::Validation(format!(
                "SHA256SUMS download failed with HTTP status {}",
                response.status()
            )));
        }

        let body = response
            .text()
            .await
            .map_err(|e| Error::Validation(format!("Failed to read SHA256SUMS: {e}")))?;

        Ok(ChecksumMap::parse(&body))
    }

    async fn try_download_candidate(
        &self,
        base: &str,
        tag: &str,
        candidate: &str,
        checksums: &ChecksumMap,
    ) -> Result<Option<Vec<u8>>> {
        let url = format!("{base}/{tag}/{candidate}");
        let response = self
            .client
            .get(&url)
            .header("User-Agent", "pi-agent-rust-self-updater")
            .send()
            .await;

        let Ok(res) = response else {
            return Ok(None);
        };

        if !(200..300).contains(&res.status()) {
            return Ok(None);
        }

        let Ok(bytes) = res.bytes_limited(64 * 1024 * 1024).await else {
            return Ok(None);
        };

        checksums.verify_bytes(candidate, &bytes)?;
        Ok(Some(bytes))
    }

    /// Download and verify binary artifact bytes.
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

        if current_ver == target_version {
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
    fn test_platform_detection_and_candidates() {
        if let Some(plat) = PlatformInfo::current() {
            let candidates = plat.candidate_asset_names("0.2.0");
            assert!(!candidates.is_empty());
            assert!(candidates.iter().any(|c| c.contains("pi")));
        }
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
