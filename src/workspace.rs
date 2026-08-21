//! Multi-root workspace state and the unified path-confinement helper
//! (bd-cv653.3.12).
//!
//! A session operates on a *primary* working directory plus an optional set
//! of *additional roots* added via `--add-dir` at launch or `/add-dir`
//! mid-session. Every confinement decision — built-in tool path checks and
//! the extension filesystem connector scope check — routes through
//! [`ensure_canonical_path_allowed`] so the two surfaces cannot drift.
//!
//! `WorkspaceHandle` clones share one live root set (interior `Arc<RwLock>`),
//! so `/remove-dir` revokes access immediately for every holder: tools and
//! the extension FS connector snapshot at execution time and never cache a
//! stale copy. The [`WorkspaceHandle::default`] identity handle preserves
//! legacy single-cwd behavior byte-for-byte (see [`WorkspaceHandle::snapshot_or`]).

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use crate::error::{Error, Result};
use crate::extensions::safe_canonicalize;

/// Shared workspace root set (multi-root confinement, bd-cv653.3.12).
///
/// `primary` is the session cwd (`None` for the identity default handle);
/// `additional` holds canonicalized extra roots and is shared across clones.
#[derive(Debug, Default)]
pub struct WorkspaceHandle {
    primary: Option<PathBuf>,
    additional: Arc<RwLock<Vec<PathBuf>>>,
}

impl Clone for WorkspaceHandle {
    fn clone(&self) -> Self {
        Self {
            primary: self.primary.clone(),
            additional: Arc::clone(&self.additional),
        }
    }
}

impl PartialEq for WorkspaceHandle {
    fn eq(&self, other: &Self) -> bool {
        self.primary == other.primary && self.roots() == other.roots()
    }
}

impl WorkspaceHandle {
    /// The single-root handle for a session cwd.
    #[must_use]
    pub fn single(cwd: &Path) -> Self {
        Self {
            primary: Some(cwd.to_path_buf()),
            additional: Arc::new(RwLock::new(Vec::new())),
        }
    }
    /// Add a root (`--add-dir` / `/add-dir` land here). The path is
    /// canonicalized before storage so symlink-resolved confinement checks
    /// compare like against like; duplicates (of the primary or an existing
    /// additional root) are ignored. Validate user input with
    /// [`validate_new_root`] first when it must fail loudly.
    pub fn add_root(&mut self, root: PathBuf) {
        let canonical = safe_canonicalize(&root);
        if self
            .primary
            .as_ref()
            .is_some_and(|primary| primary == &canonical)
        {
            return;
        }
        let mut guard = self
            .additional
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !guard.contains(&canonical) {
            guard.push(canonical);
        }
    }

    /// Remove an additional root (lexical or canonical form accepted;
    /// comparison happens on canonical forms). The primary root can never be
    /// removed. Returns whether anything was removed.
    pub fn remove_root(&mut self, root: &Path) -> bool {
        let canonical = safe_canonicalize(root);
        let mut guard = self
            .additional
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let before = guard.len();
        guard.retain(|existing| existing != &canonical);
        before != guard.len()
    }

    /// All configured roots: primary first (when set), then canonical
    /// additional roots in insertion order.
    #[must_use]
    pub fn roots(&self) -> Vec<PathBuf> {
        let mut all = Vec::new();
        if let Some(primary) = &self.primary {
            all.push(primary.clone());
        }
        all.extend(self.additional_roots());
        all
    }

    /// Canonical additional roots in insertion order (primary excluded).
    #[must_use]
    pub fn additional_roots(&self) -> Vec<PathBuf> {
        self.additional
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone())
    }

    /// Whether `path` sits under any configured root. An identity handle
    /// (no primary, no additional roots — the `Default`) allows everything:
    /// legacy behavior for unwired tools and tests.
    #[must_use]
    pub fn is_within(&self, path: &Path) -> bool {
        if self.primary.is_none() && self.additional_roots().is_empty() {
            return true;
        }
        let canonical = safe_canonicalize(path);
        if let Some(primary) = &self.primary {
            if canonical.starts_with(primary) {
                return true;
            }
        }
        self.additional_roots()
            .iter()
            .any(|root| canonical.starts_with(root))
    }

    /// Snapshot the root set, defaulting to a single root at `cwd` when no
    /// primary was configured (the `Default` identity handle). Tools call
    /// this at execution time so `/remove-dir` revokes immediately.
    #[must_use]
    pub fn snapshot_or(&self, cwd: &Path) -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            primary: self.primary.clone().unwrap_or_else(|| cwd.to_path_buf()),
            additional: self.additional_roots(),
        }
    }
}

/// An immutable root-set snapshot for one confinement check.
#[derive(Debug, Clone)]
pub struct WorkspaceSnapshot {
    primary: PathBuf,
    additional: Vec<PathBuf>,
}

impl WorkspaceSnapshot {
    /// The root list: primary first, then additional roots.
    #[must_use]
    pub fn all(&self) -> Vec<PathBuf> {
        let mut roots = Vec::with_capacity(1 + self.additional.len());
        roots.push(self.primary.clone());
        roots.extend(self.additional.iter().cloned());
        roots
    }

    /// The primary root.
    #[must_use]
    pub fn primary(&self) -> &Path {
        &self.primary
    }

    /// Canonical additional roots (primary excluded). Empty for single-root
    /// sessions — enforcement wrappers use this to keep legacy decisions and
    /// error messages verbatim.
    #[must_use]
    pub fn additional(&self) -> &[PathBuf] {
        &self.additional
    }

    /// Whether `canonical_path` sits inside any root of this snapshot.
    #[must_use]
    pub fn contains_canonical(&self, canonical_path: &Path) -> bool {
        canonical_path.starts_with(&self.primary)
            || self
                .additional
                .iter()
                .any(|root| canonical_path.starts_with(root))
    }
}

/// Validate and canonicalize a user-supplied additional root before
/// [`WorkspaceHandle::add_root`] (bd-cv653.3.12). Adding a root grants
/// read/write access, so the target must exist and be a directory; the
/// returned path is the canonical form to persist in session headers.
///
/// # Errors
/// Validation error naming the offending path when it is empty, missing, or
/// not a directory.
pub fn validate_new_root(root: &Path) -> Result<PathBuf> {
    if root.as_os_str().is_empty() {
        return Err(Error::validation(
            "cannot add an empty path as a workspace root",
        ));
    }
    if !root.is_dir() {
        return Err(Error::validation(format!(
            "workspace root must be an existing directory: {}",
            root.display()
        )));
    }
    Ok(safe_canonicalize(root))
}
/// THE unified confinement gate (bd-cv653.3.12): a canonical path must sit
/// under one of the allowed roots. Named refusal otherwise. Callers must
/// symlink-resolve (`safe_canonicalize`) both the path and each root before
/// calling so escapes via symlinks cannot pass the prefix test.
///
/// # Errors
/// Tool error naming the offending path when outside every root.
pub fn ensure_canonical_path_allowed(
    canonical_path: &Path,
    allowed_roots: &[PathBuf],
    action: &str,
) -> Result<()> {
    if any_root_contains(allowed_roots, canonical_path) {
        return Ok(());
    }
    let listed = allowed_roots
        .iter()
        .map(|root| root.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Err(Error::tool(
        action,
        format!(
            "refusing to {action} outside the allowed roots: {} (roots: {listed})",
            canonical_path.display()
        ),
    ))
}

/// The single containment decision (bd-cv653.3.12): whether a canonical
/// path sits under any of the canonical roots. Tool enforcement and the
/// extension FS connector both call this so their prefix semantics cannot
/// drift.
#[must_use]
pub fn any_root_contains(roots: &[PathBuf], canonical_path: &Path) -> bool {
    roots
        .iter()
        .any(|root| canonical_path.starts_with(root.as_path()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("pi-workspace-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("create fixture dir");
        base
    }

    #[test]
    fn add_canonicalizes_and_dedups() {
        let primary = dir("primary");
        let extra = dir("extra");
        let mut handle = WorkspaceHandle::single(&primary);

        handle.add_root(extra.clone());
        assert_eq!(handle.additional_roots().len(), 1);
        // Re-adding the same root (nested lexical form) is a no-op.
        handle.add_root(extra.join("."));
        assert_eq!(handle.additional_roots().len(), 1);
        // The primary never lands in additional.
        handle.add_root(primary.clone());
        assert_eq!(handle.additional_roots().len(), 1);
        assert_eq!(handle.roots().len(), 2);
    }

    #[test]
    fn add_rejects_missing_directory_fails_closed() {
        let primary = dir("reject-primary");
        let missing = std::env::temp_dir().join("pi-workspace-definitely-missing-xyz");
        // add_root itself is infallible (canonicalization degrades to
        // lexical), so fail-closed behavior lives in validate_new_root.
        let err = validate_new_root(&missing).expect_err("missing dir must fail");
        assert!(err.to_string().contains("must be an existing directory"));
    }
    #[test]
    fn remove_never_takes_primary_and_matches_canonically() {
        let primary = dir("rm-primary");
        let extra = dir("rm-extra");
        let mut handle = WorkspaceHandle::single(&primary);
        handle.add_root(extra.clone());

        assert!(handle.remove_root(&extra));
        assert!(handle.additional_roots().is_empty());
        // Dot-segment lexical form still matches canonically.
        handle.add_root(extra.clone());
        assert!(handle.remove_root(&extra.join("sub").join("..")));
        assert!(!handle.remove_root(&primary), "primary removal refused");
        assert!(handle.is_within(&primary));
    }

    #[test]
    fn containment_covers_both_roots_and_outside_denies() {
        let primary = dir("contain-primary");
        let extra = dir("contain-extra");
        let mut handle = WorkspaceHandle::single(&primary);
        handle.add_root(extra.clone());

        let inside_primary = safe_canonicalize(primary.join("a.txt").as_path());
        let inside_extra = safe_canonicalize(extra.join("b.txt").as_path());
        let outside = safe_canonicalize(std::env::temp_dir().as_path());
        assert!(handle.is_within(&inside_primary));
        assert!(handle.is_within(&inside_extra));
        assert!(!handle.is_within(&outside));

        handle.remove_root(&extra);
        assert!(!handle.is_within(&inside_extra));

        let snapshot = handle.snapshot_or(&primary);
        let err = ensure_canonical_path_allowed(&outside, &snapshot.all(), "read")
            .expect_err("outside denied");
        assert!(
            err.to_string()
                .contains("refusing to read outside the allowed roots"),
            "{err}"
        );
        ensure_canonical_path_allowed(&inside_primary, &snapshot.all(), "read")
            .expect("inside primary allowed");
    }

    #[test]
    fn identity_default_preserves_legacy_behavior() {
        let cwd = dir("fallback-primary");
        let handle = WorkspaceHandle::default();
        assert!(handle.is_within(&std::env::temp_dir()));
        let snapshot = handle.snapshot_or(&cwd);
        assert_eq!(snapshot.primary(), cwd.as_path());
        assert!(snapshot.additional().is_empty());
        assert_eq!(snapshot.all(), vec![cwd.clone()]);
    }

    #[test]
    fn clones_share_root_set_for_immediate_revocation() {
        let primary = dir("shared-primary");
        let extra = dir("shared-extra");
        let mut handle = WorkspaceHandle::single(&primary);
        handle.add_root(extra.clone());

        let cloned = handle.clone();
        let inside_extra = safe_canonicalize(extra.join("f.txt").as_path());
        assert!(cloned.is_within(&inside_extra));

        assert!(handle.remove_root(&extra));
        assert!(!cloned.is_within(&inside_extra), "clone sees revocation");
    }

    #[test]
    fn validate_new_root_fails_closed() {
        let primary = dir("validate-primary");
        let err = validate_new_root(&primary.join("missing")).expect_err("missing denied");
        assert!(err.to_string().contains("must be an existing directory"));
        assert!(
            validate_new_root(Path::new("")).is_err(),
            "empty path denied"
        );
        assert_eq!(
            validate_new_root(&primary).expect("valid dir"),
            safe_canonicalize(&primary)
        );
    }
}
