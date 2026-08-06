//! Shared platform-identity utilities.
//!
//! Provides OS name, architecture, and client identity strings used by
//! provider request builders. Centralises the mapping from Rust's
//! `std::env::consts` values to the provider-expected naming conventions
//! (e.g. `darwin` instead of `macos`, `arm64` instead of `aarch64`).

use std::path::Path;

/// Crate version baked in at compile time.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Unix read permission bit within an owner/group/other mode class.
pub(crate) const UNIX_ACCESS_READ: u32 = 0o4;
/// Unix write permission bit within an owner/group/other mode class.
pub(crate) const UNIX_ACCESS_WRITE: u32 = 0o2;
/// Unix search/execute permission bit within an owner/group/other mode class.
pub(crate) const UNIX_ACCESS_SEARCH: u32 = 0o1;

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct EffectiveUnixIdentity {
    euid: u32,
    egid: u32,
    supplementary_groups: Vec<u32>,
}

#[cfg(unix)]
impl EffectiveUnixIdentity {
    fn current() -> std::io::Result<Self> {
        let supplementary_groups = rustix::process::getgroups()
            .map_err(std::io::Error::from)?
            .into_iter()
            .map(rustix::process::Gid::as_raw)
            .collect();
        Ok(Self {
            euid: rustix::process::geteuid().as_raw(),
            egid: rustix::process::getegid().as_raw(),
            supplementary_groups,
        })
    }

    fn mode_shift(&self, owner_uid: u32, owner_gid: u32) -> u32 {
        if self.euid == owner_uid {
            6
        } else if self.egid == owner_gid || self.supplementary_groups.contains(&owner_gid) {
            3
        } else {
            0
        }
    }
}

/// Effective Unix identity captured for one filesystem-policy operation.
///
/// Callers that validate many paths should create one context per operation so
/// supplementary groups are fetched once, without caching credentials across
/// later operations where the process identity may have changed.
#[derive(Debug, Clone)]
pub(crate) struct EffectiveModeAccessContext {
    #[cfg(unix)]
    identity: EffectiveUnixIdentity,
}

impl EffectiveModeAccessContext {
    pub(crate) fn current() -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            Ok(Self {
                identity: EffectiveUnixIdentity::current()?,
            })
        }

        #[cfg(not(unix))]
        {
            Ok(Self {})
        }
    }

    pub(crate) fn ensure(
        &self,
        metadata: &std::fs::Metadata,
        path: &Path,
        required: u32,
        operation: &str,
    ) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;

            if required & !0o7 != 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("invalid Unix access mask {required:#o}"),
                ));
            }

            let shift = self.identity.mode_shift(metadata.uid(), metadata.gid());
            let selected = (metadata.mode() >> shift) & 0o7;
            if selected & required != required {
                let class = match shift {
                    6 => "owner",
                    3 => "group",
                    _ => "other",
                };
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!(
                        "Permission denied: Unix {class} mode class {:04o} on {} does not allow {operation}",
                        metadata.mode() & 0o7777,
                        path.display()
                    ),
                ));
            }
        }

        #[cfg(not(unix))]
        {
            let _ = (metadata, path, required, operation);
        }

        Ok(())
    }
}

/// Enforce the Unix permission class selected for the effective process identity.
///
/// Unix selects exactly one class in owner, group, other order; permissions from
/// separate classes are never combined. UID 0 intentionally follows the same
/// selection here instead of applying the kernel's discretionary-access bypass.
/// That keeps tool/session/extension policy deterministic under privileged and
/// unprivileged test runners. The subsequent real filesystem operation remains
/// authoritative for ACLs, mount policy, and filesystem races.
pub(crate) fn ensure_effective_mode_access(
    metadata: &std::fs::Metadata,
    path: &Path,
    required: u32,
    operation: &str,
) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        if required & !0o7 != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid Unix access mask {required:#o}"),
            ));
        }

        let mode = metadata.mode();
        if [0, 3, 6]
            .into_iter()
            .all(|shift| ((mode >> shift) & 0o7) & required == required)
        {
            return Ok(());
        }
    }

    EffectiveModeAccessContext::current()?.ensure(metadata, path, required, operation)
}

// ---------------------------------------------------------------------------
// OS
// ---------------------------------------------------------------------------

/// Return the OS name in the format most providers expect.
///
/// Maps Rust's `std::env::consts::OS`:
///   - `"macos"` → `"darwin"`
///   - everything else passed through (`"linux"`, `"windows"`, …)
#[inline]
pub fn os_name() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Architecture
// ---------------------------------------------------------------------------

/// Return the architecture name in the format most providers expect.
///
/// Maps Rust's `std::env::consts::ARCH`:
///   - `"aarch64"` → `"arm64"`
///   - `"x86_64"`  → `"amd64"`
///   - everything else passed through
#[inline]
pub fn arch_name() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Composite helpers
// ---------------------------------------------------------------------------

/// `"{os}/{arch}"` — e.g. `"linux/amd64"`, `"darwin/arm64"`.
pub fn platform_tag() -> String {
    format!("{}/{}", os_name(), arch_name())
}

/// Canonical Pi User-Agent: `"pi_agent_rust/{version}"`.
pub fn pi_user_agent() -> String {
    format!("pi_agent_rust/{VERSION}")
}

/// Canonical Pi User-Agent with an additional component:
/// `"pi_agent_rust/{version} {extra}"`.
pub fn pi_user_agent_with(extra: &str) -> String {
    format!("pi_agent_rust/{VERSION} {extra}")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn identity(euid: u32, egid: u32, supplementary_groups: &[u32]) -> EffectiveUnixIdentity {
        EffectiveUnixIdentity {
            euid,
            egid,
            supplementary_groups: supplementary_groups.to_vec(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_permission_mode_class_selection_is_owner_then_group_then_other() {
        assert_eq!(identity(1000, 2000, &[3000]).mode_shift(1000, 3000), 6);
        assert_eq!(identity(1000, 2000, &[3000]).mode_shift(4000, 2000), 3);
        assert_eq!(identity(1000, 2000, &[3000]).mode_shift(4000, 3000), 3);
        assert_eq!(identity(1000, 2000, &[3000]).mode_shift(4000, 5000), 0);
    }

    #[cfg(unix)]
    #[test]
    fn unix_permission_root_uses_selected_mode_class_without_dac_bypass() {
        let root = identity(0, 0, &[0]);
        assert_eq!(root.mode_shift(0, 1234), 6, "root-owned path uses owner");
        assert_eq!(root.mode_shift(1234, 0), 3, "root-group path uses group");
        assert_eq!(
            root.mode_shift(1234, 5678),
            0,
            "unowned path uses other even for UID 0"
        );
    }

    #[test]
    fn os_name_not_empty() {
        assert!(!os_name().is_empty());
    }

    #[test]
    fn arch_name_not_empty() {
        assert!(!arch_name().is_empty());
    }

    #[test]
    fn platform_tag_has_slash() {
        let tag = platform_tag();
        assert!(tag.contains('/'), "expected OS/ARCH, got: {tag}");
    }

    #[test]
    fn pi_user_agent_contains_version() {
        let ua = pi_user_agent();
        assert!(ua.starts_with("pi_agent_rust/"), "ua: {ua}");
        assert!(ua.contains(VERSION), "ua should contain version");
    }

    #[test]
    fn pi_user_agent_with_appends() {
        let ua = pi_user_agent_with("Antigravity/1.2.3");
        assert!(ua.starts_with("pi_agent_rust/"));
        assert!(ua.ends_with("Antigravity/1.2.3"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_os_name() {
        assert_eq!(os_name(), "linux");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_maps_to_darwin() {
        assert_eq!(os_name(), "darwin");
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn x86_64_maps_to_amd64() {
        assert_eq!(arch_name(), "amd64");
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn aarch64_maps_to_arm64() {
        assert_eq!(arch_name(), "arm64");
    }
}
