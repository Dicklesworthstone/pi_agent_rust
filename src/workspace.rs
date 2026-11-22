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

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use crate::error::{Error, Result};
use crate::extensions::safe_canonicalize;
use crate::file_identity::FileIdentity;

/// Native child launch manifest; this is not project configuration or trust.
#[doc(hidden)]
pub const CHILD_WORKSPACE_ROOTS_SCHEMA: &str = "pi.child_workspace_roots.v1";
#[doc(hidden)]
pub const CHILD_WORKSPACE_ROOTS_MAX_ROOTS: usize = 128;
#[doc(hidden)]
pub const CHILD_WORKSPACE_ROOTS_MAX_BYTES: usize = 256 * 1024;

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ChildWorkspaceRoots {
    schema: String,
    roots: Vec<String>,
}

/// A live grant, pinned by snapshots so a removed and re-added path cannot
/// revive authority captured by an earlier child. Pointer identity cannot be
/// reused while a snapshot still owns its token; there is no wrapping counter.
#[derive(Debug, Clone)]
pub(crate) struct WorkspaceRootGrant {
    path: PathBuf,
    token: Arc<()>,
}

impl WorkspaceRootGrant {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    fn new(path: PathBuf) -> Self {
        Self {
            path,
            token: Arc::new(()),
        }
    }

    fn matches(&self, other: &Self) -> bool {
        self.path == other.path && Arc::ptr_eq(&self.token, &other.token)
    }
}

/// Shared workspace root set (multi-root confinement, bd-cv653.3.12).
///
/// `primary` is the session cwd (`None` for the identity default handle);
/// `additional` holds canonicalized extra roots and is shared across clones.
#[derive(Debug, Default)]
pub struct WorkspaceHandle {
    primary: Option<PathBuf>,
    additional: Arc<RwLock<Vec<WorkspaceRootGrant>>>,
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
    /// The single-root handle for a session cwd. The primary is stored in
    /// canonical form so every containment/dedup comparison is
    /// canonical-vs-canonical (on macOS `/var/...` vs `/private/var/...`
    /// would otherwise never match).
    #[must_use]
    pub fn single(cwd: &Path) -> Self {
        Self {
            primary: Some(safe_canonicalize(cwd)),
            additional: Arc::new(RwLock::new(Vec::new())),
        }
    }
    /// Add a root (`--add-dir` / `/add-dir` land here). The path is
    /// canonicalized before storage so symlink-resolved confinement checks
    /// compare like against like; duplicates (of the primary or an existing
    /// additional root) are ignored. Validate user input with
    /// [`validate_new_root`] first when it must fail loudly.
    pub fn add_root(&mut self, root: &Path) {
        let canonical = safe_canonicalize(root);
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
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !guard.iter().any(|grant| grant.path == canonical) {
            guard.push(WorkspaceRootGrant::new(canonical));
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
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = guard.len();
        guard.retain(|grant| grant.path != canonical);
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
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|grant| grant.path.clone())
            .collect()
    }

    pub(crate) fn additional_root_grants(&self) -> Vec<WorkspaceRootGrant> {
        self.additional
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn contains_additional_root_grants(&self, expected: &[WorkspaceRootGrant]) -> bool {
        let roots = self
            .additional
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        expected
            .iter()
            .all(|grant| roots.iter().any(|current| current.matches(grant)))
    }

    /// Install a native parent's bounded manifest only after every entry has
    /// passed validation. Root strings are already canonical: do not resolve
    /// them again during publication and accidentally adopt a replacement link.
    /// This grants path access only; it never loads instructions or trusts
    /// executable configuration from an additional directory.
    ///
    /// # Errors
    /// Refuses malformed, replaced, oversized or non-regular manifests and
    /// any missing, non-absolute or noncanonical root without changing state.
    #[doc(hidden)]
    pub fn load_child_workspace_roots(&mut self, path: &Path) -> Result<()> {
        let roots = read_child_workspace_roots(path)?;
        let mut guard = self
            .additional
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for root in roots {
            if self.primary.as_ref() != Some(&root) && !guard.iter().any(|grant| grant.path == root)
            {
                guard.push(WorkspaceRootGrant::new(root));
            }
        }
        Ok(())
    }

    /// Publish an already validated canonical root set without resolving paths
    /// again after persistence. A host may hold another mutable clone, so an
    /// SDK transaction must not overwrite root changes made while it saved.
    #[allow(clippy::needless_pass_by_ref_mut)]
    pub(crate) fn replace_additional_roots_if_unchanged(
        &mut self,
        expected: &[PathBuf],
        roots: Vec<PathBuf>,
    ) -> bool {
        let mut guard = self
            .additional
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !guard.iter().map(|grant| &grant.path).eq(expected.iter()) {
            return false;
        }
        *guard = roots
            .into_iter()
            .map(|path| {
                guard
                    .iter()
                    .find(|grant| grant.path == path)
                    .cloned()
                    .unwrap_or_else(|| WorkspaceRootGrant::new(path))
            })
            .collect();
        true
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
        if let Some(primary) = &self.primary
            && canonical.starts_with(primary)
        {
            return true;
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

fn child_roots_error(message: impl std::fmt::Display) -> Error {
    Error::validation(format!("PI_CHILD_WORKSPACE_ROOTS: {message}"))
}

fn validate_child_root(root: &Path) -> Result<()> {
    let text = root
        .to_str()
        .ok_or_else(|| child_roots_error("root paths must be UTF-8"))?;
    if text.is_empty()
        || text.len() > CHILD_WORKSPACE_ROOTS_MAX_BYTES
        || text.contains('\0')
        || !root.is_absolute()
    {
        return Err(child_roots_error(
            "root paths must be bounded absolute paths",
        ));
    }
    let canonical = std::fs::canonicalize(root)
        .map(crate::extensions::strip_unc_prefix)
        .map_err(|_| child_roots_error("every root must be an existing directory"))?;
    if canonical.as_os_str() != root.as_os_str() || !canonical.is_dir() {
        return Err(child_roots_error(
            "every root must name its exact canonical directory",
        ));
    }
    Ok(())
}

fn parse_child_workspace_roots(bytes: &[u8]) -> Result<Vec<PathBuf>> {
    if bytes.len() > CHILD_WORKSPACE_ROOTS_MAX_BYTES {
        return Err(child_roots_error("manifest exceeds 256 KiB"));
    }
    let manifest: ChildWorkspaceRoots = serde_json::from_slice(bytes)
        .map_err(|_| child_roots_error("expected a strict schema and roots JSON object"))?;
    if manifest.schema != CHILD_WORKSPACE_ROOTS_SCHEMA
        || manifest.roots.len() > CHILD_WORKSPACE_ROOTS_MAX_ROOTS
    {
        return Err(child_roots_error(
            "unsupported schema or more than 128 roots",
        ));
    }
    let mut roots = Vec::with_capacity(manifest.roots.len());
    for text in manifest.roots {
        let root = PathBuf::from(text);
        validate_child_root(&root)?;
        if roots.contains(&root) {
            return Err(child_roots_error("duplicate root paths"));
        }
        roots.push(root);
    }
    Ok(roots)
}

/// Encode the same strict manifest accepted by native child startup.
///
/// # Errors
/// Refuses unsupported path encodings, noncanonical or missing directories,
/// duplicate roots, or a manifest exceeding its count/byte limits.
#[doc(hidden)]
pub fn encode_child_workspace_roots(roots: &[PathBuf]) -> Result<Vec<u8>> {
    if roots.len() > CHILD_WORKSPACE_ROOTS_MAX_ROOTS {
        return Err(child_roots_error("manifest exceeds 128 roots"));
    }
    let mut strings = Vec::with_capacity(roots.len());
    let mut text_bytes = 0_usize;
    for root in roots {
        validate_child_root(root)?;
        let text = root
            .to_str()
            .ok_or_else(|| child_roots_error("root paths must be UTF-8"))?;
        text_bytes = text_bytes.saturating_add(text.len());
        if text_bytes > CHILD_WORKSPACE_ROOTS_MAX_BYTES {
            return Err(child_roots_error("manifest exceeds 256 KiB"));
        }
        if strings.iter().any(|existing| existing == text) {
            return Err(child_roots_error("duplicate root paths"));
        }
        strings.push(text.to_string());
    }
    let bytes = serde_json::to_vec(&ChildWorkspaceRoots {
        schema: CHILD_WORKSPACE_ROOTS_SCHEMA.to_string(),
        roots: strings,
    })
    .map_err(|_| child_roots_error("could not encode manifest"))?;
    if bytes.len() > CHILD_WORKSPACE_ROOTS_MAX_BYTES {
        return Err(child_roots_error("manifest exceeds 256 KiB"));
    }
    Ok(bytes)
}

fn read_child_workspace_roots(path: &Path) -> Result<Vec<PathBuf>> {
    let io_error = |_| child_roots_error("manifest is unavailable or changed during reading");
    let metadata = std::fs::symlink_metadata(path).map_err(io_error)?;
    if !metadata.is_file() || metadata.len() > CHILD_WORKSPACE_ROOTS_MAX_BYTES as u64 {
        return Err(child_roots_error(
            "manifest must be a regular non-symlink file of at most 256 KiB",
        ));
    }
    let identity = FileIdentity::of_path_nofollow(path).map_err(io_error)?;
    #[cfg(unix)]
    let mut file = File::from(
        rustix::fs::open(
            path,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .map_err(std::io::Error::from)
        .map_err(io_error)?,
    );
    #[cfg(windows)]
    let mut file: File = {
        use std::os::windows::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(0x0020_0000) // FILE_FLAG_OPEN_REPARSE_POINT
            .open(path)
            .map_err(io_error)?
    };
    #[cfg(not(any(unix, windows)))]
    let mut file = File::open(path).map_err(io_error)?;
    let opened = file.metadata().map_err(io_error)?;
    if !opened.is_file()
        || opened.len() != metadata.len()
        || FileIdentity::of_open_file(&file).map_err(io_error)? != identity
    {
        return Err(child_roots_error("manifest changed while opening"));
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(CHILD_WORKSPACE_ROOTS_MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    let after = file.metadata().map_err(io_error)?;
    if after.len() != opened.len()
        || after.modified().map_err(io_error)? != opened.modified().map_err(io_error)?
        || bytes.len() as u64 != opened.len()
        || FileIdentity::of_path_nofollow(path).map_err(io_error)? != identity
    {
        return Err(child_roots_error("manifest changed while reading"));
    }
    parse_child_workspace_roots(&bytes)
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
/// [`WorkspaceHandle::add_root`] (bd-cv653.3.12).
///
/// Adding a root grants read/write access, so the target must exist and be
/// a directory; the returned path is the canonical form to persist in
/// session headers.
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
/// under one of the allowed roots.
///
/// Named refusal otherwise. Callers must symlink-resolve
/// (`safe_canonicalize`) both the path and each root before calling so
/// escapes via symlinks cannot pass the prefix test.
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
/// path sits under any of the canonical roots.
///
/// Tool enforcement and the extension FS connector both call this so their
/// prefix semantics cannot drift.
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

        handle.add_root(&extra);
        assert_eq!(handle.additional_roots().len(), 1);
        // Re-adding the same root (nested lexical form) is a no-op.
        handle.add_root(&extra.join("."));
        assert_eq!(handle.additional_roots().len(), 1);
        // The primary never lands in additional.
        handle.add_root(&primary);
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
        handle.add_root(&extra);

        assert!(handle.remove_root(&extra));
        assert!(handle.additional_roots().is_empty());
        // Dot-segment lexical form still matches canonically.
        handle.add_root(&extra);
        assert!(handle.remove_root(&extra.join("sub").join("..")));
        assert!(!handle.remove_root(&primary), "primary removal refused");
        assert!(handle.is_within(&primary));
    }

    #[test]
    fn containment_covers_both_roots_and_outside_denies() {
        let primary = dir("contain-primary");
        let extra = dir("contain-extra");
        let mut handle = WorkspaceHandle::single(&primary);
        handle.add_root(&extra);

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
        assert_eq!(snapshot.all(), vec![cwd]);
    }

    #[test]
    fn clones_share_root_set_for_immediate_revocation() {
        let primary = dir("shared-primary");
        let extra = dir("shared-extra");
        let mut handle = WorkspaceHandle::single(&primary);
        handle.add_root(&extra);

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

    #[test]
    fn child_manifest_round_trip_installs_shared_roots_without_reissuing_grants() {
        let temporary = tempfile::tempdir().expect("workspace fixture");
        let primary = temporary.path().join("primary");
        let extra = temporary.path().join("extra root 雪");
        std::fs::create_dir(&primary).expect("primary");
        std::fs::create_dir(&extra).expect("additional root");
        let primary = safe_canonicalize(&primary);
        let extra = safe_canonicalize(&extra);
        let manifest = temporary.path().join("roots.json");
        let encoded = encode_child_workspace_roots(&[primary.clone(), extra.clone()])
            .expect("canonical manifest");
        std::fs::write(&manifest, &encoded).expect("manifest");
        let mut workspace = WorkspaceHandle::single(&primary);
        let cloned = workspace.clone();
        workspace
            .load_child_workspace_roots(&manifest)
            .expect("install parent roots");
        let grants = workspace.additional_root_grants();
        assert_eq!(cloned.additional_roots(), vec![extra.clone()]);
        assert_eq!(grants[0].path(), extra);
        workspace
            .load_child_workspace_roots(&manifest)
            .expect("duplicate installation is idempotent");
        assert!(cloned.contains_additional_root_grants(&grants));
        assert_eq!(workspace.roots(), vec![primary, extra]);
        assert_eq!(
            std::fs::read(&manifest).expect("unmodified source"),
            encoded
        );
    }

    #[test]
    fn child_manifest_invalid_later_entry_never_installs_a_valid_prefix() {
        let temporary = tempfile::tempdir().expect("workspace fixture");
        let primary = safe_canonicalize(temporary.path());
        let existing = primary.join("existing");
        let proposed = primary.join("proposed");
        std::fs::create_dir(&existing).expect("existing root");
        std::fs::create_dir(&proposed).expect("proposed root");
        let ordinary_file = primary.join("ordinary.txt");
        std::fs::write(&ordinary_file, "not a directory").expect("ordinary file");
        let manifest = primary.join("roots.json");
        let mut workspace = WorkspaceHandle::single(&primary);
        workspace.add_root(&existing);
        let grants = workspace.additional_root_grants();
        let cases = [
            serde_json::json!({"schema":CHILD_WORKSPACE_ROOTS_SCHEMA,"roots":[proposed,"relative"]}),
            serde_json::json!({"schema":CHILD_WORKSPACE_ROOTS_SCHEMA,"roots":[proposed,""]}),
            serde_json::json!({"schema":CHILD_WORKSPACE_ROOTS_SCHEMA,"roots":[proposed,"bad\u{0}root"]}),
            serde_json::json!({"schema":CHILD_WORKSPACE_ROOTS_SCHEMA,"roots":[proposed,primary.join("missing")]}),
            serde_json::json!({"schema":CHILD_WORKSPACE_ROOTS_SCHEMA,"roots":[proposed,ordinary_file]}),
            serde_json::json!({"schema":CHILD_WORKSPACE_ROOTS_SCHEMA,"roots":[proposed,proposed]}),
            serde_json::json!({"schema":CHILD_WORKSPACE_ROOTS_SCHEMA,"roots":[proposed,12]}),
            serde_json::json!({"schema":CHILD_WORKSPACE_ROOTS_SCHEMA,"roots":[proposed],"trust":true}),
            serde_json::json!({"schema":"pi.child_workspace_roots.v2","roots":[proposed]}),
            serde_json::json!({"schema":CHILD_WORKSPACE_ROOTS_SCHEMA,"roots":proposed}),
        ];
        for case in cases {
            std::fs::write(&manifest, serde_json::to_vec(&case).expect("fixture JSON"))
                .expect("manifest");
            let error = workspace
                .load_child_workspace_roots(&manifest)
                .expect_err("invalid manifest must fail as one operation");
            assert!(error.to_string().contains("PI_CHILD_WORKSPACE_ROOTS"));
            assert_eq!(workspace.additional_roots(), vec![existing.clone()]);
            assert!(workspace.contains_additional_root_grants(&grants));
        }
    }

    #[test]
    fn child_manifest_parser_rejects_ambiguous_json_and_noncanonical_paths() {
        for bytes in [
            b"".as_slice(),
            b"null".as_slice(),
            b"[]".as_slice(),
            b"{}".as_slice(),
            br#"{"schema":"pi.child_workspace_roots.v1","roots":[],"roots":[]}"#.as_slice(),
            br#"{"schema":"pi.child_workspace_roots.v1","schema":"pi.child_workspace_roots.v1","roots":[]}"#.as_slice(),
            br#"{"schema":"pi.child_workspace_roots.v1","roots":[]} {}"#.as_slice(),
            b"\xff".as_slice(),
        ] {
            assert!(parse_child_workspace_roots(bytes).is_err());
        }
        let temporary = tempfile::tempdir().expect("workspace fixture");
        let canonical = safe_canonicalize(temporary.path());
        let noncanonical = canonical.join(".");
        let bytes = serde_json::to_vec(&serde_json::json!({
            "schema":CHILD_WORKSPACE_ROOTS_SCHEMA,"roots":[noncanonical]
        }))
        .expect("fixture JSON");
        assert!(parse_child_workspace_roots(&bytes).is_err());
        assert!(encode_child_workspace_roots(&[noncanonical]).is_err());
        assert!(encode_child_workspace_roots(&[canonical.clone(), canonical]).is_err());
    }

    #[test]
    fn child_manifest_count_and_encoded_byte_boundaries_are_exact() {
        let temporary = tempfile::tempdir().expect("workspace fixture");
        let mut roots = Vec::new();
        for index in 0..=CHILD_WORKSPACE_ROOTS_MAX_ROOTS {
            let path = temporary.path().join(index.to_string());
            std::fs::create_dir(&path).expect("bounded root fixture");
            roots.push(safe_canonicalize(&path));
        }
        let accepted = &roots[..CHILD_WORKSPACE_ROOTS_MAX_ROOTS];
        let encoded = encode_child_workspace_roots(accepted).expect("128 roots accepted");
        assert_eq!(parse_child_workspace_roots(&encoded).unwrap(), accepted);
        assert!(encode_child_workspace_roots(&roots).is_err());
        let oversized_count = serde_json::to_vec(&serde_json::json!({
            "schema":CHILD_WORKSPACE_ROOTS_SCHEMA,"roots":roots
        }))
        .unwrap();
        assert!(parse_child_workspace_roots(&oversized_count).is_err());

        let mut exact = encode_child_workspace_roots(&[]).expect("empty manifest");
        exact.resize(CHILD_WORKSPACE_ROOTS_MAX_BYTES, b' ');
        let manifest = temporary.path().join("roots.json");
        std::fs::write(&manifest, &exact).expect("maximum-size JSON");
        assert!(read_child_workspace_roots(&manifest).unwrap().is_empty());
        exact.push(b' ');
        assert!(parse_child_workspace_roots(&exact).is_err());
        std::fs::write(&manifest, &exact).expect("oversized JSON");
        assert!(read_child_workspace_roots(&manifest).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn child_manifest_refuses_symlinks_special_files_and_non_utf8_roots() {
        use std::os::unix::ffi::OsStringExt as _;
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().expect("workspace fixture");
        let canonical = safe_canonicalize(temporary.path());
        let manifest = canonical.join("roots.json");
        std::fs::write(&manifest, encode_child_workspace_roots(&[]).unwrap())
            .expect("valid manifest");
        let file_link = canonical.join("manifest-link");
        symlink(&manifest, &file_link).expect("manifest symlink");
        assert!(read_child_workspace_roots(&file_link).is_err());
        assert!(read_child_workspace_roots(&canonical).is_err());
        let root_link = canonical.join("root-link");
        symlink(&canonical, &root_link).expect("root symlink");
        assert!(encode_child_workspace_roots(std::slice::from_ref(&root_link)).is_err());
        let linked_root = serde_json::to_vec(&serde_json::json!({
            "schema":CHILD_WORKSPACE_ROOTS_SCHEMA,"roots":[root_link]
        }))
        .unwrap();
        assert!(parse_child_workspace_roots(&linked_root).is_err());
        let fifo = canonical.join("manifest-fifo");
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .expect("FIFO fixture");
        assert!(read_child_workspace_roots(&fifo).is_err());
        let non_utf8 = canonical.join(std::ffi::OsString::from_vec(vec![b'x', 0xff]));
        std::fs::create_dir(&non_utf8).expect("non-UTF-8 directory");
        assert!(encode_child_workspace_roots(&[non_utf8]).is_err());
    }

    #[test]
    fn workspace_grants_survive_unrelated_changes_but_not_remove_and_readd() {
        let temporary = tempfile::tempdir().expect("workspace fixture");
        let mut workspace = WorkspaceHandle::single(temporary.path());
        let first = temporary.path().join("first");
        let other = temporary.path().join("other");
        std::fs::create_dir(&first).expect("first root");
        std::fs::create_dir(&other).expect("other root");
        workspace.add_root(&first);
        let grants = workspace.additional_root_grants();
        let mut cloned = workspace.clone();
        cloned.add_root(&other);
        assert!(workspace.contains_additional_root_grants(&grants));
        cloned.add_root(&first);
        assert!(workspace.contains_additional_root_grants(&grants));
        assert!(cloned.remove_root(&first));
        cloned.add_root(&first);
        assert!(!workspace.contains_additional_root_grants(&grants));
        let renewed = workspace.additional_root_grants();
        assert!(cloned.contains_additional_root_grants(&renewed));
        assert_eq!(renewed.len(), 2);
    }

    #[test]
    fn conditional_root_replacement_preserves_only_unrevoked_grants() {
        let temporary = tempfile::tempdir().expect("workspace fixture");
        let first = temporary.path().join("first");
        let second = temporary.path().join("second");
        let third = temporary.path().join("third");
        for path in [&first, &second, &third] {
            std::fs::create_dir(path).expect("root fixture");
        }
        let first = safe_canonicalize(&first);
        let second = safe_canonicalize(&second);
        let third = safe_canonicalize(&third);
        let mut workspace = WorkspaceHandle::single(temporary.path());
        workspace.add_root(&first);
        workspace.add_root(&second);
        let original = workspace.additional_root_grants();
        assert!(workspace.replace_additional_roots_if_unchanged(
            &[first.clone(), second.clone()],
            vec![second.clone(), first.clone(), third.clone()]
        ));
        assert!(workspace.contains_additional_root_grants(&original));
        assert!(
            !workspace.replace_additional_roots_if_unchanged(std::slice::from_ref(&first), vec![])
        );
        assert!(workspace.contains_additional_root_grants(&original));
        let expected = workspace.additional_roots();
        assert!(workspace.replace_additional_roots_if_unchanged(&expected, vec![second.clone()]));
        let expected = workspace.additional_roots();
        assert!(
            workspace.replace_additional_roots_if_unchanged(&expected, vec![first, second, third])
        );
        assert!(!workspace.contains_additional_root_grants(&original));
        assert!(workspace.contains_additional_root_grants(&original[1..]));
    }
}
