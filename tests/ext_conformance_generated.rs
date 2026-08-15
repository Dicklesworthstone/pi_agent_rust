#![cfg(feature = "ext-conformance")]
//! Auto-generated conformance tests for all extensions in the validated manifest (bd-15jg).
//!
//! Each extension in `tests/ext_conformance/VALIDATED_MANIFEST.json` gets its own `#[test]`
//! function via the `conformance_test!` macro. This provides:
//! - Parallelism (cargo test runs tests in parallel)
//! - Isolation (one failure does not block others)
//! - Clear reporting (each extension shows as pass/fail in test output)
//!
//! Tiers 1–2 run by default; tiers 3–5 are owner-bead-stamped `#[ignore]`
//! cases (multi-file, npm deps, UI, platform).
//!
//! Run tracked stretch tiers:
//!   cargo test --test `ext_conformance_generated` --features ext-conformance -- --include-ignored
//!
//! Run only tier 1–2 (default):
//!   cargo test --test `ext_conformance_generated`

mod common;

use pi::extensions::{ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::tools::ToolRegistry;
use serde_json::Value;
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

// ─── Manifest types ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Deserialize)]
struct ManifestEntry {
    id: String,
    entry_path: String,
    conformance_tier: u32,
    capabilities: Capabilities,
    registrations: Registrations,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[allow(dead_code, clippy::struct_excessive_bools)]
struct Capabilities {
    registers_tools: bool,
    registers_commands: bool,
    registers_flags: bool,
    registers_providers: bool,
    subscribes_events: Vec<String>,
    is_multi_file: bool,
    has_npm_deps: bool,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[allow(dead_code)]
struct Registrations {
    tools: Vec<String>,
    commands: Vec<String>,
    flags: Vec<String>,
    event_handlers: Vec<String>,
}

#[derive(Debug, serde::Deserialize)]
struct Manifest {
    schema: String,
    extensions: Vec<ManifestEntry>,
}

impl Manifest {
    fn find(&self, ext_id: &str) -> Option<&ManifestEntry> {
        self.extensions.iter().find(|e| e.id == ext_id)
    }
}

// ─── Manifest loading ───────────────────────────────────────────────────────

fn artifacts_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("ext_conformance")
        .join("artifacts")
}

fn manifest_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(MUST_PASS_MANIFEST_PATH)
}

const MUST_PASS_INCLUSION_PATH: &str = "docs/extension-inclusion-list.json";
const MUST_PASS_MANIFEST_PATH: &str = "tests/ext_conformance/VALIDATED_MANIFEST.json";
const EXPECTED_CANONICAL_MUST_PASS_EXTENSIONS_V1: usize = 208;
const MUST_PASS_ARTIFACTS_PATH: &str = "tests/ext_conformance/artifacts";

const MUST_PASS_SOURCE_PATHS: &[&str] = &[
    ".cargo/config.toml",
    ".gitattributes",
    "CHANGELOG.md",
    "Cargo.lock",
    "Cargo.toml",
    "build.rs",
    "docs/evidence/tool-output-context-cache.jsonl",
    "docs/extension-artifact-provenance.json",
    "docs/provider-upstream-model-ids-snapshot.json",
    "docs/schema/extension_protocol.json",
    "legacy_pi_mono_code/pi-mono/packages/ai/src/models.generated.ts",
    "rust-toolchain.toml",
    "src",
    "tests/common",
    MUST_PASS_INCLUSION_PATH,
    MUST_PASS_MANIFEST_PATH,
    MUST_PASS_ARTIFACTS_PATH,
    "tests/ext_conformance_generated.rs",
    "tests/release_readiness.rs",
];

fn current_git_commit(root: &Path) -> Result<String, String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .map_err(|err| format!("failed to execute git rev-parse HEAD: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "git rev-parse HEAD failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let commit = String::from_utf8(output.stdout)
        .map_err(|err| format!("git rev-parse HEAD returned non-UTF-8 output: {err}"))?;
    let commit = commit.trim();
    if !matches!(commit.len(), 40 | 64) || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!(
            "git rev-parse HEAD returned invalid commit: {commit}"
        ));
    }
    Ok(commit.to_string())
}

fn ensure_must_pass_worktree_matches_commit(
    root: &Path,
    commit: &str,
    source_paths: &[&str],
) -> Result<(), String> {
    let observed_head = current_git_commit(root)?;
    if observed_head != commit {
        return Err(format!(
            "must-pass source HEAD changed while capturing provenance: expected {commit}, found {observed_head}"
        ));
    }

    let mut untracked_command = std::process::Command::new("git");
    untracked_command
        .arg("-C")
        .arg(root)
        .args(["ls-files", "--others", "-z", "--"])
        .args(source_paths);
    let untracked_output = untracked_command
        .output()
        .map_err(|err| format!("failed to list untracked must-pass source inputs: {err}"))?;
    if !untracked_output.status.success() {
        return Err(format!(
            "git ls-files failed while checking untracked must-pass inputs: {}",
            String::from_utf8_lossy(&untracked_output.stderr).trim()
        ));
    }
    let untracked = String::from_utf8(untracked_output.stdout)
        .map_err(|err| format!("git ls-files returned non-UTF-8 untracked paths: {err}"))?;
    let untracked = untracked
        .split('\0')
        .filter(|path| !path.is_empty())
        .take(5)
        .collect::<Vec<_>>();
    if !untracked.is_empty() {
        return Err(format!(
            "must-pass source inputs contain untracked files: {}",
            untracked.join(", ")
        ));
    }

    let flag_output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-v", "-z", "--"])
        .args(source_paths)
        .output()
        .map_err(|err| format!("failed to inspect must-pass index flags: {err}"))?;
    if !flag_output.status.success() {
        return Err(format!(
            "git ls-files failed while checking must-pass index flags: {}",
            String::from_utf8_lossy(&flag_output.stderr).trim()
        ));
    }
    let flagged = String::from_utf8(flag_output.stdout)
        .map_err(|err| format!("git ls-files returned non-UTF-8 flag records: {err}"))?
        .split('\0')
        .filter(|record| !record.is_empty())
        .filter(|record| !record.starts_with("H "))
        .take(5)
        .map(str::to_string)
        .collect::<Vec<_>>();
    if !flagged.is_empty() {
        return Err(format!(
            "must-pass source inputs use non-canonical index flags (including assume-unchanged or skip-worktree): {}",
            flagged.join(", ")
        ));
    }

    for (label, diff_args) in [
        ("worktree", vec!["diff", "--quiet", "--no-ext-diff", "--"]),
        (
            "index",
            vec!["diff", "--cached", "--quiet", "--no-ext-diff", commit, "--"],
        ),
    ] {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(diff_args)
            .args(source_paths)
            .status()
            .map_err(|err| format!("failed to inspect must-pass source dirt: {err}"))?;
        match status.code() {
            Some(0) => {}
            Some(1) => {
                return Err(format!(
                    "must-pass source inputs differ in the {label}; commit them before generating release evidence"
                ));
            }
            code => {
                return Err(format!(
                    "git diff failed while inspecting must-pass source inputs (status {code:?})"
                ));
            }
        }
    }

    Ok(())
}

fn must_pass_tree_records(
    root: &Path,
    commit: &str,
    source_paths: &[&str],
) -> Result<Vec<(String, String, String)>, String> {
    let mut command = std::process::Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(["ls-tree", "-r", "-z", "--full-tree", commit, "--"])
        .args(source_paths);
    let output = command
        .output()
        .map_err(|err| format!("failed to list tracked must-pass source inputs: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "git ls-tree failed for must-pass source inputs: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let records = String::from_utf8(output.stdout)
        .map_err(|err| format!("git ls-tree returned non-UTF-8 records: {err}"))?;
    let records = records
        .split('\0')
        .filter(|record| !record.is_empty())
        .map(|record| {
            let (metadata, path) = record
                .split_once('\t')
                .ok_or_else(|| format!("malformed git ls-tree record: {record}"))?;
            let mut fields = metadata.split_ascii_whitespace();
            let mode = fields
                .next()
                .ok_or_else(|| format!("missing mode in git ls-tree record: {record}"))?;
            let object_type = fields
                .next()
                .ok_or_else(|| format!("missing type in git ls-tree record: {record}"))?;
            let blob = fields
                .next()
                .ok_or_else(|| format!("missing blob in git ls-tree record: {record}"))?;
            if fields.next().is_some()
                || object_type != "blob"
                || !matches!(mode, "100644" | "100755")
                || !matches!(blob.len(), 40 | 64)
                || !blob.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(format!("invalid git ls-tree record: {record}"));
            }
            Ok((path.to_string(), mode.to_string(), blob.to_string()))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut parsed = records;
    parsed.sort_by(|left, right| left.0.cmp(&right.0));
    if parsed.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err("git ls-tree returned duplicate must-pass source paths".to_string());
    }
    for required in source_paths {
        let prefix = format!("{required}/");
        if !parsed
            .iter()
            .any(|(path, _, _)| *path == *required || path.starts_with(&prefix))
        {
            return Err(format!(
                "required must-pass source input is not tracked: {required}"
            ));
        }
    }

    Ok(parsed)
}

fn source_tree_sha256(records: &[(String, String, String)]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"pi.ext.must_pass_source_tree.v2\0");
    for (path, mode, blob) in records {
        hasher.update(path.as_bytes());
        hasher.update([0]);
        hasher.update(mode.as_bytes());
        hasher.update([0]);
        hasher.update(blob.as_bytes());
        hasher.update([0]);
    }
    pi::package_manager::hex_encode(&hasher.finalize())
}

fn git_blob_oid(contents: &[u8], oid_hex_len: usize) -> Result<String, String> {
    let header = format!("blob {}\0", contents.len());
    match oid_hex_len {
        40 => {
            let mut hasher = Sha1::new();
            hasher.update(header.as_bytes());
            hasher.update(contents);
            Ok(pi::package_manager::hex_encode(&hasher.finalize()))
        }
        64 => {
            let mut hasher = Sha256::new();
            hasher.update(header.as_bytes());
            hasher.update(contents);
            Ok(pi::package_manager::hex_encode(&hasher.finalize()))
        }
        length => Err(format!("unsupported Git object ID length: {length}")),
    }
}

fn ensure_worktree_bytes_match_tree(
    root: &Path,
    records: &[(String, String, String)],
) -> Result<(), String> {
    for (path, _, expected_blob) in records {
        let contents = std::fs::read(root.join(path)).map_err(|err| {
            format!("failed to read must-pass worktree input {path} for byte comparison: {err}")
        })?;
        let actual_blob = git_blob_oid(&contents, expected_blob.len())?;
        if actual_blob != *expected_blob {
            return Err(format!(
                "must-pass worktree bytes differ from canonical commit for {path}"
            ));
        }
    }
    Ok(())
}

fn git_commit_file_contents(root: &Path, commit: &str, relative: &str) -> Result<Vec<u8>, String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["show", &format!("{commit}:{relative}")])
        .output()
        .map_err(|err| format!("failed to read canonical Git blob for {relative}: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "git show failed for canonical {relative} blob: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

#[derive(Debug, serde::Deserialize)]
struct AuthoritativeInclusionList {
    schema: String,
    summary: AuthoritativeInclusionSummary,
    tier1: Vec<AuthoritativeInclusionEntry>,
    tier1_review: Vec<AuthoritativeInclusionEntry>,
}

#[derive(Debug, serde::Deserialize)]
struct AuthoritativeInclusionSummary {
    tier1_count: usize,
    tier1_review_count: usize,
    total_must_pass: usize,
}

#[derive(Debug, serde::Deserialize)]
struct AuthoritativeInclusionEntry {
    id: String,
}

fn parse_authoritative_must_pass_ids(
    contents: &[u8],
    expected_must_pass: usize,
) -> Result<Vec<String>, String> {
    let inclusion: AuthoritativeInclusionList = serde_json::from_slice(contents)
        .map_err(|err| format!("invalid {MUST_PASS_INCLUSION_PATH}: {err}"))?;
    if inclusion.schema != "pi.ext.inclusion_list.v1" {
        return Err(format!(
            "unexpected schema in {MUST_PASS_INCLUSION_PATH}: {}",
            inclusion.schema
        ));
    }

    let observed_tier1 = inclusion.tier1.len();
    let observed_review = inclusion.tier1_review.len();
    let observed_total = observed_tier1
        .checked_add(observed_review)
        .ok_or_else(|| format!("must-pass count overflow in {MUST_PASS_INCLUSION_PATH}"))?;
    if inclusion.summary.tier1_count != observed_tier1
        || inclusion.summary.tier1_review_count != observed_review
        || inclusion.summary.total_must_pass != observed_total
        || observed_total != expected_must_pass
    {
        return Err(format!(
            "{MUST_PASS_INCLUSION_PATH} summary mismatch or unexpected versioned must-pass denominator: summary={}+{}={}, observed={observed_tier1}+{observed_review}={observed_total}, expected={expected_must_pass}",
            inclusion.summary.tier1_count,
            inclusion.summary.tier1_review_count,
            inclusion.summary.total_must_pass,
        ));
    }

    let mut ids = Vec::with_capacity(observed_total);
    let mut seen = std::collections::BTreeSet::new();
    for (section, entries) in [
        ("tier1", inclusion.tier1),
        ("tier1_review", inclusion.tier1_review),
    ] {
        for (index, entry) in entries.into_iter().enumerate() {
            let id = entry.id;
            if id.is_empty() || id.trim() != id || id.chars().any(char::is_control) {
                return Err(format!(
                    "{MUST_PASS_INCLUSION_PATH} {section}[{index}] has a malformed id"
                ));
            }
            if !seen.insert(id.clone()) {
                return Err(format!(
                    "{MUST_PASS_INCLUSION_PATH} contains duplicate must-pass id {id}"
                ));
            }
            ids.push(id);
        }
    }
    Ok(ids)
}

fn is_canonical_manifest_name(value: &str) -> bool {
    !value.is_empty() && value.trim() == value && !value.chars().any(char::is_control)
}

fn is_canonical_artifact_entry_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    let has_windows_drive_prefix =
        bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    is_canonical_manifest_name(value)
        && !value.contains('\\')
        && !value.starts_with('/')
        && !has_windows_drive_prefix
        && value
            .split('/')
            .all(|component| !component.is_empty() && component != "." && component != "..")
}

fn declared_name_set(
    entry_id: &str,
    field: &str,
    values: &[String],
) -> Result<std::collections::BTreeSet<String>, String> {
    let mut names = std::collections::BTreeSet::new();
    for (index, value) in values.iter().enumerate() {
        if !is_canonical_manifest_name(value) {
            return Err(format!(
                "manifest entry {entry_id} has malformed {field}[{index}]"
            ));
        }
        if !names.insert(value.clone()) {
            return Err(format!(
                "manifest entry {entry_id} has duplicate {field} identity {value}"
            ));
        }
    }
    Ok(names)
}

fn validate_manifest_entry_contract(entry: &ManifestEntry) -> Result<(), String> {
    if !is_canonical_manifest_name(&entry.id) {
        return Err(format!("manifest entry has malformed id: {:?}", entry.id));
    }
    if !is_canonical_artifact_entry_path(&entry.entry_path) {
        return Err(format!(
            "manifest entry {} must use a normalized relative artifact entry_path, found {:?}",
            entry.id, entry.entry_path
        ));
    }
    if !(1..=5).contains(&entry.conformance_tier) {
        return Err(format!(
            "manifest entry {} has invalid conformance_tier {}; expected 1..=5",
            entry.id, entry.conformance_tier
        ));
    }

    let tools = declared_name_set(&entry.id, "registrations.tools", &entry.registrations.tools)?;
    let commands = declared_name_set(
        &entry.id,
        "registrations.commands",
        &entry.registrations.commands,
    )?;
    let flags = declared_name_set(&entry.id, "registrations.flags", &entry.registrations.flags)?;
    let event_handlers = declared_name_set(
        &entry.id,
        "registrations.event_handlers",
        &entry.registrations.event_handlers,
    )?;
    let subscribed_events = declared_name_set(
        &entry.id,
        "capabilities.subscribes_events",
        &entry.capabilities.subscribes_events,
    )?;

    for (capability, declared, label) in [
        (entry.capabilities.registers_tools, &tools, "tools"),
        (entry.capabilities.registers_commands, &commands, "commands"),
        (entry.capabilities.registers_flags, &flags, "flags"),
    ] {
        if !capability && !declared.is_empty() {
            return Err(format!(
                "manifest entry {} declares {label} identities while the matching capability is false",
                entry.id
            ));
        }
    }
    if subscribed_events != event_handlers {
        return Err(format!(
            "manifest entry {} has divergent subscribes_events and event_handlers identities",
            entry.id
        ));
    }
    Ok(())
}

struct AuthoritativeConformanceSets<'a> {
    must_pass: Vec<&'a ManifestEntry>,
    stretch: Vec<&'a ManifestEntry>,
}

fn select_authoritative_conformance_sets<'a>(
    manifest: &'a Manifest,
    must_pass_ids: &[String],
) -> Result<AuthoritativeConformanceSets<'a>, String> {
    if manifest.schema != "pi.ext.validated-manifest.v1" {
        return Err(format!(
            "unexpected schema in {MUST_PASS_MANIFEST_PATH}: {}",
            manifest.schema
        ));
    }

    let mut manifest_by_id = std::collections::BTreeMap::new();
    let mut manifest_entry_paths = std::collections::BTreeSet::new();
    for (index, entry) in manifest.extensions.iter().enumerate() {
        validate_manifest_entry_contract(entry).map_err(|err| {
            format!("{MUST_PASS_MANIFEST_PATH} extensions[{index}] is invalid: {err}")
        })?;
        if manifest_by_id.insert(entry.id.as_str(), entry).is_some() {
            return Err(format!(
                "{MUST_PASS_MANIFEST_PATH} contains duplicate extension id {}",
                entry.id
            ));
        }
        if !manifest_entry_paths.insert(entry.entry_path.as_str()) {
            return Err(format!(
                "{MUST_PASS_MANIFEST_PATH} contains duplicate artifact entry_path {}",
                entry.entry_path
            ));
        }
    }

    let mut selected_ids = std::collections::BTreeSet::new();
    let mut must_pass = Vec::with_capacity(must_pass_ids.len());
    for id in must_pass_ids {
        if !selected_ids.insert(id.as_str()) {
            return Err(format!(
                "{MUST_PASS_INCLUSION_PATH} contains duplicate must-pass id {id}"
            ));
        }
        let entry = manifest_by_id.get(id.as_str()).copied().ok_or_else(|| {
            format!(
                "canonical must-pass id {id} from {MUST_PASS_INCLUSION_PATH} is absent from {MUST_PASS_MANIFEST_PATH}"
            )
        })?;
        must_pass.push(entry);
    }

    let stretch = manifest
        .extensions
        .iter()
        .filter(|entry| !selected_ids.contains(entry.id.as_str()))
        .collect();
    Ok(AuthoritativeConformanceSets { must_pass, stretch })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MustPassSourceProvenance {
    git_commit: String,
    source_tree_sha256: String,
    inclusion_sha256: String,
    manifest_sha256: String,
}

#[derive(Debug)]
struct MustPassSourceSnapshot {
    provenance: MustPassSourceProvenance,
    inclusion_contents: Vec<u8>,
    manifest_contents: Vec<u8>,
    tracked_paths: std::collections::BTreeSet<String>,
}

fn capture_must_pass_source_snapshot(root: &Path) -> Result<MustPassSourceSnapshot, String> {
    let git_commit = current_git_commit(root)?;
    ensure_must_pass_worktree_matches_commit(root, &git_commit, MUST_PASS_SOURCE_PATHS)?;
    let records = must_pass_tree_records(root, &git_commit, MUST_PASS_SOURCE_PATHS)?;
    ensure_worktree_bytes_match_tree(root, &records)?;
    let inclusion_contents = git_commit_file_contents(root, &git_commit, MUST_PASS_INCLUSION_PATH)?;
    let manifest_contents = git_commit_file_contents(root, &git_commit, MUST_PASS_MANIFEST_PATH)?;
    let observed_head = current_git_commit(root)?;
    if observed_head != git_commit {
        return Err(format!(
            "must-pass source HEAD changed during snapshot capture: {git_commit} -> {observed_head}"
        ));
    }
    ensure_must_pass_worktree_matches_commit(root, &git_commit, MUST_PASS_SOURCE_PATHS)?;
    ensure_worktree_bytes_match_tree(root, &records)?;

    let tracked_paths = records.iter().map(|record| record.0.clone()).collect();
    Ok(MustPassSourceSnapshot {
        provenance: MustPassSourceProvenance {
            git_commit,
            source_tree_sha256: source_tree_sha256(&records),
            inclusion_sha256: pi::package_manager::hex_encode(&Sha256::digest(&inclusion_contents)),
            manifest_sha256: pi::package_manager::hex_encode(&Sha256::digest(&manifest_contents)),
        },
        inclusion_contents,
        manifest_contents,
        tracked_paths,
    })
}

fn ensure_must_pass_source_unchanged(
    before: &MustPassSourceProvenance,
    after: &MustPassSourceProvenance,
) -> Result<(), String> {
    if before == after {
        return Ok(());
    }

    let mut changed = Vec::new();
    if before.git_commit != after.git_commit {
        changed.push("git_commit");
    }
    if before.source_tree_sha256 != after.source_tree_sha256 {
        changed.push("source_tree_sha256");
    }
    if before.inclusion_sha256 != after.inclusion_sha256 {
        changed.push("inclusion_sha256");
    }
    if before.manifest_sha256 != after.manifest_sha256 {
        changed.push("manifest_sha256");
    }
    Err(format!(
        "must-pass source provenance changed during extension execution: {}",
        changed.join(", ")
    ))
}

fn validate_snapshot_artifact_paths(
    snapshot: &MustPassSourceSnapshot,
    manifest: &Manifest,
) -> Result<(), String> {
    for entry in &manifest.extensions {
        let artifact_path = format!("{MUST_PASS_ARTIFACTS_PATH}/{}", entry.entry_path);
        if !snapshot.tracked_paths.contains(&artifact_path) {
            return Err(format!(
                "manifest entry {} points to artifact input not tracked by canonical commit {}: {artifact_path}",
                entry.id, snapshot.provenance.git_commit
            ));
        }
    }
    Ok(())
}

fn parse_manifest(contents: &[u8]) -> Result<Manifest, String> {
    serde_json::from_slice(contents)
        .map_err(|err| format!("failed to parse {MUST_PASS_MANIFEST_PATH}: {err}"))
}

fn load_manifest() -> &'static Manifest {
    static MANIFEST: OnceLock<Manifest> = OnceLock::new();
    MANIFEST.get_or_init(|| {
        let data = std::fs::read(manifest_path()).expect("Failed to read VALIDATED_MANIFEST.json");
        parse_manifest(&data).expect("Failed to parse VALIDATED_MANIFEST.json")
    })
}

fn hermetic_conformance_env(cwd: &Path) -> std::collections::HashMap<String, String> {
    let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let home = cwd.join("home");
    let tmp = cwd.join("tmp");
    std::fs::create_dir_all(&home).expect("create conformance home");
    std::fs::create_dir_all(&tmp).expect("create conformance tmp");

    let cwd = cwd.display().to_string();
    let home = home.display().to_string();
    let tmp = tmp.display().to_string();
    std::collections::HashMap::from([
        ("PI_DETERMINISTIC_CWD".to_string(), cwd),
        ("HOME".to_string(), home.clone()),
        ("USERPROFILE".to_string(), home.clone()),
        ("PI_DETERMINISTIC_HOME".to_string(), home),
        ("TMPDIR".to_string(), tmp.clone()),
        ("TEMP".to_string(), tmp.clone()),
        ("TMP".to_string(), tmp),
    ])
}

fn conformance_work_dir(name: impl AsRef<str>) -> PathBuf {
    let dir = std::env::temp_dir().join(name.as_ref());
    std::fs::create_dir_all(&dir).expect("create conformance work dir");
    std::fs::canonicalize(&dir).unwrap_or(dir)
}

const PROMPT_TEMPLATE_MODEL_EXT_ID: &str = "npm/pi-prompt-template-model";
const PROMPT_TEMPLATE_MODEL_FIXTURE_COMMAND: &str = "model-mode";
const PROMPT_TEMPLATE_MODEL_FIXTURE_FILE: &str = "model-mode.md";
const PROMPT_TEMPLATE_MODEL_FIXTURE_BODY: &str = "\
---
description: Conformance prompt template with model routing
model: anthropic/claude-sonnet-4-20250514
thinking: low
restore: false
---
Use this conformance prompt: $@
";

fn prepare_extension_fixture(ext_id: &str, cwd: &Path) {
    if ext_id != PROMPT_TEMPLATE_MODEL_EXT_ID {
        return;
    }

    for prompt_dir in [
        cwd.join(".pi").join("prompts"),
        cwd.join("home").join(".pi").join("agent").join("prompts"),
    ] {
        std::fs::create_dir_all(&prompt_dir).expect("create prompt-template-model fixture dir");
        std::fs::write(
            prompt_dir.join(PROMPT_TEMPLATE_MODEL_FIXTURE_FILE),
            PROMPT_TEMPLATE_MODEL_FIXTURE_BODY,
        )
        .expect("write prompt-template-model fixture prompt");
    }
}

// ─── Core conformance test runner ───────────────────────────────────────────

/// Load an extension from the artifacts directory, validate registrations match
/// the manifest, and capture a registration snapshot as a JSON artifact.
#[allow(clippy::too_many_lines)]
fn run_conformance_test(ext_id: &str) {
    let manifest = load_manifest();
    let Some(entry) = manifest.find(ext_id) else {
        unreachable!("Extension '{ext_id}' not found in VALIDATED_MANIFEST.json");
    };

    let harness = common::TestHarness::new(format!("conformance_{}", ext_id.replace('/', "_")));
    let cwd = harness.temp_dir().to_path_buf();
    prepare_extension_fixture(ext_id, &cwd);

    // Resolve the extension entry file.
    // Some artifacts live under dist/ which rch excludes from sync.
    // Gracefully skip when the artifact is absent (matches try_conformance behaviour).
    let entry_file = artifacts_dir().join(&entry.entry_path);
    if !entry_file.exists() {
        eprintln!(
            "SKIP: artifact not found (likely rch dist/ exclusion): {}",
            entry_file.display()
        );
        return;
    }

    harness
        .log()
        .info_ctx("conformance", "Loading extension", |ctx| {
            ctx.push(("ext_id".into(), ext_id.to_string()));
            ctx.push(("entry_path".into(), entry.entry_path.clone()));
            ctx.push(("tier".into(), entry.conformance_tier.to_string()));
        });

    // Create the load spec.
    let spec = JsExtensionLoadSpec::from_entry_path(&entry_file).unwrap_or_else(|e| {
        unreachable!(
            "Failed to create JsExtensionLoadSpec for '{ext_id}' at {}: {e}",
            entry_file.display()
        )
    });

    // Start JS runtime and load the extension.
    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let js_config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        env: hermetic_conformance_env(&cwd),
        deny_env: false,
        ..Default::default()
    };

    let runtime = common::run_async({
        let manager = manager.clone();
        let tools = Arc::clone(&tools);
        async move {
            JsExtensionRuntimeHandle::start(js_config, tools, manager)
                .await
                .expect("start JS runtime")
        }
    });
    manager.set_js_runtime(runtime);

    let ext_id_owned = ext_id.to_string();
    common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .load_js_extensions(vec![spec])
                .await
                .unwrap_or_else(|e| unreachable!("Failed to load extension '{ext_id_owned}': {e}"));
        }
    });

    harness
        .log()
        .info("conformance", "Extension loaded successfully");

    // Capture registration snapshot.
    let snapshot = snapshot_registrations(&manager);

    // Write snapshot artifact.
    let snapshot_path = harness.temp_path("registration_snapshot.json");
    let snapshot_json =
        serde_json::to_string_pretty(&snapshot).expect("serialize registration snapshot");
    std::fs::write(&snapshot_path, &snapshot_json).expect("write snapshot");
    harness.record_artifact("registration_snapshot", &snapshot_path);

    // ── Validate registrations against manifest ──

    // Use the same fail-closed identity contract as the aggregate must-pass
    // gate so individual extension tests cannot accept weaker evidence.
    let observation = observe_registrations(&manager).unwrap_or_else(|err| {
        panic!("Extension '{ext_id}': malformed runtime registration evidence: {err}")
    });
    validate_registration_observation(entry, &observation).unwrap_or_else(|err| panic!("{err}"));

    harness
        .log()
        .info_ctx("conformance", "Validation passed", |ctx| {
            ctx.push(("commands".into(), observation.commands.len().to_string()));
            ctx.push(("flags".into(), observation.flags.len().to_string()));
            ctx.push(("tool_defs".into(), observation.tools.len().to_string()));
            ctx.push(("providers".into(), observation.providers.len().to_string()));
        });

    // Write JSONL logs.
    let logs_path = harness.temp_path("test_logs.jsonl");
    if let Err(e) = harness.write_jsonl_logs_normalized(&logs_path) {
        harness
            .log()
            .warn("jsonl", format!("Failed to write JSONL logs: {e}"));
    } else {
        harness.record_artifact("jsonl_logs", &logs_path);
    }
}

/// Capture the current registration state as a JSON snapshot.
fn snapshot_registrations(manager: &ExtensionManager) -> Value {
    let commands = manager.list_commands();
    let shortcuts = manager.list_shortcuts();
    let flags = manager.list_flags();
    let providers = manager.extension_providers();
    let tool_defs = manager.extension_tool_defs();
    let event_hooks = manager.list_event_hooks();
    let models: Vec<Value> = manager
        .extension_model_entries()
        .into_iter()
        .map(|entry| serde_json::to_value(entry.model).expect("model to json"))
        .collect();

    serde_json::json!({
        "commands": commands,
        "shortcuts": shortcuts,
        "flags": flags,
        "providers": providers,
        "tool_defs": tool_defs,
        "event_handlers": event_hooks,
        "event_hooks": event_hooks,
        "models": models,
    })
}

const PROVIDER_MIRROR_OWNER_MODULE: &str = "@mariozechner/pi-ai";
const PROVIDER_MIRROR_FIX_SURFACE: &str = "shim_code";
const PROVIDER_MIRROR_PROBE_ID: &str = "mirror-helper-probe";
const PROVIDER_MIRROR_REQUIRED_HELPERS: &[&str] = &[
    "getModels",
    "getApiProvider",
    "streamSimpleOpenAIResponses",
    "loginOpenAICodex",
    "refreshOpenAICodexToken",
];

const PROVIDER_MIRROR_PROBE_SOURCE: &str = r#"
import {
  getApiProvider,
  getModels,
  loginOpenAICodex,
  refreshOpenAICodexToken,
  streamSimpleOpenAIResponses,
} from "@mariozechner/pi-ai";

const OWNER_MODULE = "@mariozechner/pi-ai";
const FIX_SURFACE = "shim_code";
const requiredHelpers = [
  ["getModels", getModels],
  ["getApiProvider", getApiProvider],
  ["streamSimpleOpenAIResponses", streamSimpleOpenAIResponses],
  ["loginOpenAICodex", loginOpenAICodex],
  ["refreshOpenAICodexToken", refreshOpenAICodexToken],
];

function drift(name, detail) {
  throw new Error(`${OWNER_MODULE} mirror helper drift: ${name}: ${detail}; owning shim module=${OWNER_MODULE}; fix_surface=${FIX_SURFACE}`);
}

function assertCallable(name, value) {
  if (typeof value !== "function") {
    drift(name, `expected function, got ${typeof value}`);
  }
}

function assertMirror(condition, name, detail) {
  if (!condition) {
    drift(name, detail);
  }
}

export default function(pi) {
  for (const [name, value] of requiredHelpers) {
    assertCallable(name, value);
  }

  const codexModels = getModels("openai-codex");
  assertMirror(Array.isArray(codexModels), "getModels", "openai-codex mirror returned a non-array");
  assertMirror(codexModels.length > 0, "getModels", "openai-codex mirror returned no models");
  const firstModel = codexModels[0];
  assertMirror(firstModel && firstModel.api === "openai-codex-responses", "getModels", "openai-codex mirror model does not target openai-codex-responses");
  assertMirror(Array.isArray(firstModel.input) && firstModel.input.includes("text"), "getModels", "openai-codex mirror model is missing text input support");

  const baseProvider = getApiProvider("openai-codex-responses");
  assertMirror(baseProvider && typeof baseProvider.streamSimple === "function", "getApiProvider", "openai-codex-responses mirror provider is missing streamSimple");

  pi.registerProvider("mirror-helper-probe", {
    api: "openai-codex-responses",
    baseUrl: firstModel.baseUrl,
    models: [firstModel],
    streamSimple: baseProvider.streamSimple,
  });
}
"#;

fn provider_mirror_probe_evidence(status: &str, error: Option<&str>) -> Value {
    serde_json::json!({
        "schema": "pi.ext.provider_mirror_helper_probe.v1",
        "status": status,
        "owner_module": PROVIDER_MIRROR_OWNER_MODULE,
        "fix_surface": PROVIDER_MIRROR_FIX_SURFACE,
        "fixture": {
            "source": "runtime-generated",
            "provider_bearing": true,
            "provider_id": PROVIDER_MIRROR_PROBE_ID,
        },
        "helpers": PROVIDER_MIRROR_REQUIRED_HELPERS,
        "error": error,
    })
}

fn write_provider_mirror_probe_fixture(cwd: &Path) -> PathBuf {
    let entry_dir = cwd.join("extension");
    std::fs::create_dir_all(&entry_dir).expect("create mirror helper probe dir");
    let entry_file = entry_dir.join("index.ts");
    std::fs::write(&entry_file, PROVIDER_MIRROR_PROBE_SOURCE)
        .expect("write mirror helper probe extension");
    entry_file
}

fn load_provider_mirror_probe(entry_file: &Path, cwd: &Path) -> Result<ExtensionManager, String> {
    let spec = JsExtensionLoadSpec::from_entry_path(entry_file).map_err(|e| {
        format!(
            "Failed to create mirror helper probe spec for {}: {e}",
            entry_file.display()
        )
    })?;
    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let js_config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        env: hermetic_conformance_env(&cwd),
        deny_env: false,
        ..Default::default()
    };

    let runtime = common::run_async({
        let manager = manager.clone();
        let tools = Arc::clone(&tools);
        async move {
            JsExtensionRuntimeHandle::start(js_config, tools, manager)
                .await
                .expect("start JS runtime")
        }
    });
    manager.set_js_runtime(runtime);

    let load_result = common::run_async({
        let manager = manager.clone();
        async move { manager.load_js_extensions(vec![spec]).await }
    });
    load_result.map(|_| manager).map_err(|err| err.to_string())
}

fn assert_provider_mirror_probe_registration(manager: &ExtensionManager) -> Result<(), String> {
    let providers = manager.extension_providers();
    let Some(provider) = providers.iter().find(|provider| {
        provider.get("id").and_then(Value::as_str) == Some(PROVIDER_MIRROR_PROBE_ID)
    }) else {
        return Err(format!(
            "expected provider registration {PROVIDER_MIRROR_PROBE_ID}"
        ));
    };

    if provider.get("api").and_then(Value::as_str) != Some("openai-codex-responses") {
        return Err(format!(
            "provider mirror probe registered unexpected provider: {provider:?}"
        ));
    }

    if provider.get("hasStreamSimple").and_then(Value::as_bool) != Some(true) {
        return Err(format!(
            "provider mirror probe did not expose streamSimple: {provider:?}"
        ));
    }

    let Some(models) = provider.get("models").and_then(Value::as_array) else {
        return Err(format!(
            "provider mirror probe models missing from {provider:?}"
        ));
    };

    if !models
        .iter()
        .any(|model| model.get("id").and_then(Value::as_str) == Some("gpt-5.5"))
    {
        return Err(format!(
            "provider mirror probe should mirror Codex models: {models:?}"
        ));
    }

    let evidence = provider_mirror_probe_evidence("covered", None);
    if evidence.get("owner_module").and_then(Value::as_str) != Some(PROVIDER_MIRROR_OWNER_MODULE) {
        return Err(format!(
            "mirror probe evidence lost owner module: {evidence:?}"
        ));
    }
    if evidence.get("fix_surface").and_then(Value::as_str) != Some(PROVIDER_MIRROR_FIX_SURFACE) {
        return Err(format!(
            "mirror probe evidence lost fix surface: {evidence:?}"
        ));
    }

    Ok(())
}

#[test]
fn pi_ai_provider_mirror_helpers_are_callable_for_provider_extensions() -> Result<(), String> {
    let cwd = conformance_work_dir("pi-provider-mirror-helper-probe");
    let entry_file = write_provider_mirror_probe_fixture(&cwd);
    let manager = load_provider_mirror_probe(&entry_file, &cwd).map_err(|message| {
        let evidence = provider_mirror_probe_evidence("load_failed", Some(&message));
        let evidence_json = serde_json::to_string_pretty(&evidence)
            .unwrap_or_else(|err| format!("failed to serialize mirror probe evidence: {err}"));
        format!("provider mirror helper regression failed: {message}\n{evidence_json}")
    })?;

    assert_provider_mirror_probe_registration(&manager)
}

fn load_extension_runtime_for_journey(
    ext_id: &str,
) -> (ManifestEntry, ExtensionManager, JsExtensionRuntimeHandle) {
    let manifest = load_manifest();
    let entry = manifest
        .find(ext_id)
        .unwrap_or_else(|| panic!("Extension '{ext_id}' not found in VALIDATED_MANIFEST.json"))
        .clone();

    let cwd = conformance_work_dir(format!(
        "pi-journey-regression-{}",
        ext_id.replace('/', "_")
    ));
    prepare_extension_fixture(ext_id, &cwd);
    let entry_file = artifacts_dir().join(&entry.entry_path);
    assert!(
        entry_file.exists(),
        "Extension '{ext_id}' artifact must exist at {}",
        entry_file.display()
    );

    let spec = JsExtensionLoadSpec::from_entry_path(&entry_file).unwrap_or_else(|e| {
        panic!(
            "Failed to create JsExtensionLoadSpec for '{ext_id}' at {}: {e}",
            entry_file.display()
        )
    });

    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let js_config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        env: hermetic_conformance_env(&cwd),
        deny_env: false,
        ..Default::default()
    };

    let runtime = common::run_async({
        let manager = manager.clone();
        let tools = Arc::clone(&tools);
        async move {
            JsExtensionRuntimeHandle::start(js_config, tools, manager)
                .await
                .expect("start JS runtime")
        }
    });
    manager.set_js_runtime(runtime.clone());

    let ext_id_owned = ext_id.to_string();
    common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .load_js_extensions(vec![spec])
                .await
                .unwrap_or_else(|e| panic!("Failed to load extension '{ext_id_owned}': {e}"));
        }
    });

    (entry, manager, runtime)
}

fn string_array_field(value: &Value, field: &str) -> Vec<String> {
    value
        .get(field)
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("registration snapshot missing array field '{field}'"))
        .iter()
        .map(|item| {
            item.as_str()
                .unwrap_or_else(|| {
                    panic!("registration snapshot field '{field}' contains non-string item: {item}")
                })
                .to_string()
        })
        .collect()
}

fn assert_event_registration_evidence(ext_id: &str, expected_hooks: &[&str], regs: &Value) {
    assert!(
        !expected_hooks.is_empty(),
        "test fixture '{ext_id}' must name expected runtime event hooks"
    );

    let mut actual_handlers = string_array_field(regs, "event_handlers");
    let mut actual_hooks = string_array_field(regs, "event_hooks");
    actual_handlers.sort();
    actual_hooks.sort();

    assert_eq!(
        actual_hooks, actual_handlers,
        "Extension '{ext_id}' event_hooks evidence must mirror event_handlers"
    );
    assert!(
        !actual_handlers.is_empty(),
        "Extension '{ext_id}' must expose concrete runtime event registrations"
    );
    for expected_hook in expected_hooks {
        assert!(
            actual_handlers.iter().any(|hook| hook == expected_hook),
            "Extension '{ext_id}' runtime event evidence missing expected hook '{expected_hook}': {actual_handlers:?}"
        );
    }
}

fn assert_command_registration_evidence(ext_id: &str, expected_command: &str, regs: &Value) {
    let commands = regs
        .get("commands")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("registration snapshot missing commands array for '{ext_id}'"));
    let actual_names: Vec<&str> = commands
        .iter()
        .filter_map(|cmd| cmd.get("name").and_then(Value::as_str))
        .collect();

    let command = commands
        .iter()
        .find(|cmd| cmd.get("name").and_then(Value::as_str) == Some(expected_command))
        .unwrap_or_else(|| {
            panic!(
                "Extension '{ext_id}' runtime command evidence missing expected command '{expected_command}': {actual_names:?}"
            )
        });
    let description = command
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        description.contains("Conformance prompt template")
            && description.contains("claude-sonnet-4-20250514")
            && description.contains("low"),
        "Extension '{ext_id}' command '{expected_command}' description should expose prompt metadata, got: {description}"
    );
}

#[test]
fn event_subscriber_journeys_record_concrete_registration_evidence() {
    let representative_ids: [(&str, &[&str]); 4] = [
        ("auto-commit-on-exit", &["session_shutdown"]),
        (
            "community/prateekmedia-token-rate",
            &[
                "session_start",
                "session_switch",
                "tool_call",
                "turn_end",
                "turn_start",
            ],
        ),
        ("npm/pi-notify", &["agent_end"]),
        (
            "third-party/rytswd-direnv",
            &["session_start", "tool_result"],
        ),
    ];

    for (ext_id, expected_hooks) in representative_ids {
        let (entry, manager, _runtime) = load_extension_runtime_for_journey(ext_id);
        assert_eq!(
            JourneyCategory::classify(&entry),
            JourneyCategory::EventSubscriber,
            "representative '{ext_id}' must exercise the event-subscriber journey lane"
        );
        let regs = snapshot_registrations(&manager);
        assert_event_registration_evidence(ext_id, expected_hooks, &regs);

        let (failure, completed, journey_regs) =
            run_category_journey(ext_id, &entry, JourneyCategory::EventSubscriber);
        assert!(
            failure.is_none(),
            "event subscriber journey for '{ext_id}' failed: {failure:?}"
        );
        assert_eq!(
            completed, 2,
            "journey for '{ext_id}' should complete both event steps"
        );
        assert_event_registration_evidence(ext_id, expected_hooks, &journey_regs);
    }
}

#[test]
fn prompt_template_model_multi_capability_journey_registers_fixture_command() {
    let (entry, manager, _runtime) =
        load_extension_runtime_for_journey(PROMPT_TEMPLATE_MODEL_EXT_ID);
    assert_eq!(
        JourneyCategory::classify(&entry),
        JourneyCategory::MultiCapability,
        "prompt-template-model must exercise the multi-capability journey lane"
    );

    let regs = snapshot_registrations(&manager);
    assert_command_registration_evidence(
        PROMPT_TEMPLATE_MODEL_EXT_ID,
        PROMPT_TEMPLATE_MODEL_FIXTURE_COMMAND,
        &regs,
    );
    assert_event_registration_evidence(
        PROMPT_TEMPLATE_MODEL_EXT_ID,
        &["agent_end", "before_agent_start", "session_start"],
        &regs,
    );

    let (failure, completed, journey_regs) = run_category_journey(
        PROMPT_TEMPLATE_MODEL_EXT_ID,
        &entry,
        JourneyCategory::MultiCapability,
    );
    assert!(
        failure.is_none(),
        "prompt-template-model multi-capability journey failed: {failure:?}"
    );
    assert_eq!(
        completed, 3,
        "prompt-template-model journey should complete every multi-capability step"
    );
    assert_command_registration_evidence(
        PROMPT_TEMPLATE_MODEL_EXT_ID,
        PROMPT_TEMPLATE_MODEL_FIXTURE_COMMAND,
        &journey_regs,
    );
    assert_event_registration_evidence(
        PROMPT_TEMPLATE_MODEL_EXT_ID,
        &["agent_end", "before_agent_start", "session_start"],
        &journey_regs,
    );
}

#[test]
fn denied_capability_event_hook_keeps_registration_but_blocks_exec() {
    let (_entry, manager, runtime) = load_extension_runtime_for_journey("negative-denied-caps");
    let regs = snapshot_registrations(&manager);
    assert_event_registration_evidence("negative-denied-caps", &["session:start"], &regs);

    let ctx = Arc::new(serde_json::json!({
        "hasUI": false,
        "cwd": "/tmp/negative-event-regression",
        "sessionEntries": [],
        "sessionBranch": [],
        "sessionLeafEntry": null,
        "modelRegistry": {},
    }));
    let result = common::run_async({
        async move {
            runtime
                .dispatch_event(
                    "session:start".to_string(),
                    serde_json::json!({}),
                    Arc::clone(&ctx),
                    10_000,
                )
                .await
        }
    });

    let value = result.expect("negative-denied-caps event handler should return denial evidence");
    assert_eq!(
        value.get("blocked").and_then(Value::as_bool),
        Some(true),
        "exec in negative-denied-caps event hook should be blocked: {value}"
    );
    let error = value
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        error.to_lowercase().contains("denied") || error.to_lowercase().contains("exec"),
        "denial evidence should mention exec policy denial, got: {error}"
    );
}

// ─── Report generator (bd-31j) ──────────────────────────────────────────────

/// Result of a single extension conformance check.
#[derive(Debug, serde::Serialize)]
struct ExtensionConformanceResult {
    id: String,
    tier: u32,
    status: String, // "pass", "fail", "skip"
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    artifact_path: Option<String>,
    commands_registered: usize,
    flags_registered: usize,
    tools_registered: usize,
    providers_registered: usize,
    duration_ms: u64,
}

#[derive(Debug, Default)]
struct RegistrationObservation {
    commands: std::collections::BTreeSet<String>,
    flags: std::collections::BTreeSet<String>,
    tools: std::collections::BTreeSet<String>,
    providers: std::collections::BTreeSet<String>,
    event_handlers: std::collections::BTreeSet<String>,
}

fn value_identity_set(
    values: &[Value],
    identity_field: &str,
    kind: &str,
) -> Result<std::collections::BTreeSet<String>, String> {
    let mut identities = std::collections::BTreeSet::new();
    for (index, value) in values.iter().enumerate() {
        let identity = value
            .get(identity_field)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("registered {kind}[{index}] is missing {identity_field}"))?;
        if !is_canonical_manifest_name(identity) {
            return Err(format!(
                "registered {kind}[{index}] has malformed {identity_field}"
            ));
        }
        if !identities.insert(identity.to_string()) {
            return Err(format!(
                "registered {kind} identity is duplicated: {identity}"
            ));
        }
    }
    Ok(identities)
}

fn string_identity_set(
    values: &[String],
    kind: &str,
) -> Result<std::collections::BTreeSet<String>, String> {
    let mut identities = std::collections::BTreeSet::new();
    for (index, identity) in values.iter().enumerate() {
        if !is_canonical_manifest_name(identity) {
            return Err(format!(
                "registered {kind}[{index}] has a malformed identity"
            ));
        }
        if !identities.insert(identity.clone()) {
            return Err(format!(
                "registered {kind} identity is duplicated: {identity}"
            ));
        }
    }
    Ok(identities)
}

fn observe_registrations(manager: &ExtensionManager) -> Result<RegistrationObservation, String> {
    Ok(RegistrationObservation {
        commands: value_identity_set(&manager.list_commands(), "name", "commands")?,
        flags: value_identity_set(&manager.list_flags(), "name", "flags")?,
        tools: value_identity_set(&manager.extension_tool_defs(), "name", "tools")?,
        providers: value_identity_set(&manager.extension_providers(), "id", "providers")?,
        event_handlers: string_identity_set(&manager.list_event_hooks(), "event handlers")?,
    })
}

fn validate_named_registration_contract(
    entry_id: &str,
    kind: &str,
    capability: bool,
    declared: &[String],
    actual: &std::collections::BTreeSet<String>,
) -> Result<(), String> {
    if capability != !actual.is_empty() {
        return Err(format!(
            "Extension '{entry_id}': manifest {kind} capability={capability}, but runtime registered {} {kind}: {actual:?}",
            actual.len()
        ));
    }
    let declared = declared_name_set(entry_id, kind, declared)?;
    let missing = declared.difference(actual).cloned().collect::<Vec<_>>();
    let unexpected = actual.difference(&declared).cloned().collect::<Vec<_>>();
    if !missing.is_empty() || !unexpected.is_empty() {
        return Err(format!(
            "Extension '{entry_id}': runtime {kind} identities differ from the manifest; missing declared identities={missing:?}, unexpected runtime identities={unexpected:?}, expected={declared:?}, actual={actual:?}"
        ));
    }
    Ok(())
}

fn validate_registration_observation(
    entry: &ManifestEntry,
    actual: &RegistrationObservation,
) -> Result<(), String> {
    validate_manifest_entry_contract(entry)?;
    validate_named_registration_contract(
        &entry.id,
        "commands",
        entry.capabilities.registers_commands,
        &entry.registrations.commands,
        &actual.commands,
    )?;
    validate_named_registration_contract(
        &entry.id,
        "flags",
        entry.capabilities.registers_flags,
        &entry.registrations.flags,
        &actual.flags,
    )?;
    validate_named_registration_contract(
        &entry.id,
        "tools",
        entry.capabilities.registers_tools,
        &entry.registrations.tools,
        &actual.tools,
    )?;
    if entry.capabilities.registers_providers != !actual.providers.is_empty() {
        return Err(format!(
            "Extension '{}': manifest provider capability={}, but runtime registered {} providers",
            entry.id,
            entry.capabilities.registers_providers,
            actual.providers.len()
        ));
    }

    let expected_events = declared_name_set(
        &entry.id,
        "event handlers",
        &entry.registrations.event_handlers,
    )?;
    if actual.event_handlers != expected_events {
        return Err(format!(
            "Extension '{}': event-handler identities differ from the manifest; expected={expected_events:?}, actual={:?}",
            entry.id, actual.event_handlers
        ));
    }
    Ok(())
}

/// Run conformance check for a single extension, returning a result without
/// panicking. This is used by the report generator to collect all results
/// even when some extensions fail.
#[allow(clippy::too_many_lines, clippy::cast_possible_truncation)]
fn try_conformance(ext_id: &str) -> ExtensionConformanceResult {
    try_conformance_with_manifest(load_manifest(), ext_id)
}

#[allow(clippy::too_many_lines, clippy::cast_possible_truncation)]
fn try_conformance_with_manifest(manifest: &Manifest, ext_id: &str) -> ExtensionConformanceResult {
    let Some(entry) = manifest.find(ext_id) else {
        return ExtensionConformanceResult {
            id: ext_id.to_string(),
            tier: 0,
            status: "skip".to_string(),
            failure_reason: Some("Not found in VALIDATED_MANIFEST.json".to_string()),
            artifact_path: None,
            commands_registered: 0,
            flags_registered: 0,
            tools_registered: 0,
            providers_registered: 0,
            duration_ms: 0,
        };
    };

    let start = std::time::Instant::now();
    let cwd = conformance_work_dir(format!(
        "pi-conformance-report-{}",
        ext_id.replace('/', "_")
    ));
    prepare_extension_fixture(ext_id, &cwd);

    let entry_file = artifacts_dir().join(&entry.entry_path);
    if !entry_file.exists() {
        return ExtensionConformanceResult {
            id: ext_id.to_string(),
            tier: entry.conformance_tier,
            status: "skip".to_string(),
            failure_reason: Some(format!("Artifact not found: {}", entry_file.display())),
            artifact_path: None,
            commands_registered: 0,
            flags_registered: 0,
            tools_registered: 0,
            providers_registered: 0,
            duration_ms: start.elapsed().as_millis() as u64,
        };
    }

    let spec = match JsExtensionLoadSpec::from_entry_path(&entry_file) {
        Ok(s) => s,
        Err(e) => {
            return ExtensionConformanceResult {
                id: ext_id.to_string(),
                tier: entry.conformance_tier,
                status: "fail".to_string(),
                failure_reason: Some(format!("Load spec error: {e}")),
                artifact_path: None,
                commands_registered: 0,
                flags_registered: 0,
                tools_registered: 0,
                providers_registered: 0,
                duration_ms: start.elapsed().as_millis() as u64,
            };
        }
    };

    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    // Some npm extensions gate registration behind API-key presence checks.
    // Conformance validates registration shape, so inject deterministic dummy
    // keys for those specific extensions.
    let mut env = hermetic_conformance_env(&cwd);
    match ext_id {
        "npm/aliou-pi-linkup" => {
            env.insert(
                "LINKUP_API_KEY".to_string(),
                "conformance-dummy-key".to_string(),
            );
        }
        "npm/aliou-pi-synthetic" => {
            env.insert(
                "SYNTHETIC_API_KEY".to_string(),
                "conformance-dummy-key".to_string(),
            );
        }
        _ => {}
    }

    let js_config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        env,
        deny_env: false,
        ..Default::default()
    };

    let runtime_result = common::run_async({
        let manager = manager.clone();
        let tools = Arc::clone(&tools);
        async move { JsExtensionRuntimeHandle::start(js_config, tools, manager).await }
    });
    let runtime = match runtime_result {
        Ok(rt) => rt,
        Err(e) => {
            return ExtensionConformanceResult {
                id: ext_id.to_string(),
                tier: entry.conformance_tier,
                status: "fail".to_string(),
                failure_reason: Some(format!("Runtime start error: {e}")),
                artifact_path: None,
                commands_registered: 0,
                flags_registered: 0,
                tools_registered: 0,
                providers_registered: 0,
                duration_ms: start.elapsed().as_millis() as u64,
            };
        }
    };
    manager.set_js_runtime(runtime);

    let load_err = common::run_async({
        let manager = manager.clone();
        async move { manager.load_js_extensions(vec![spec]).await }
    });
    if let Err(e) = load_err {
        return ExtensionConformanceResult {
            id: ext_id.to_string(),
            tier: entry.conformance_tier,
            status: "fail".to_string(),
            failure_reason: Some(format!("Load error: {e}")),
            artifact_path: None,
            commands_registered: 0,
            flags_registered: 0,
            tools_registered: 0,
            providers_registered: 0,
            duration_ms: start.elapsed().as_millis() as u64,
        };
    }

    // Validate both capability presence and exact identity equality with the
    // canonical manifest. Undeclared runtime registrations are evidence drift,
    // not successful conformance.
    let observation = match observe_registrations(&manager) {
        Ok(observation) => observation,
        Err(err) => {
            return ExtensionConformanceResult {
                id: ext_id.to_string(),
                tier: entry.conformance_tier,
                status: "fail".to_string(),
                failure_reason: Some(format!("Malformed runtime registration evidence: {err}")),
                artifact_path: None,
                commands_registered: manager.list_commands().len(),
                flags_registered: manager.list_flags().len(),
                tools_registered: manager.extension_tool_defs().len(),
                providers_registered: manager.extension_providers().len(),
                duration_ms: start.elapsed().as_millis() as u64,
            };
        }
    };
    if let Err(err) = validate_registration_observation(entry, &observation) {
        return ExtensionConformanceResult {
            id: ext_id.to_string(),
            tier: entry.conformance_tier,
            status: "fail".to_string(),
            failure_reason: Some(err),
            artifact_path: None,
            commands_registered: observation.commands.len(),
            flags_registered: observation.flags.len(),
            tools_registered: observation.tools.len(),
            providers_registered: observation.providers.len(),
            duration_ms: start.elapsed().as_millis() as u64,
        };
    }

    let snapshot = snapshot_registrations(&manager);
    let snapshot_path = cwd.join("registration_snapshot.json");
    let _ = std::fs::write(
        &snapshot_path,
        serde_json::to_string_pretty(&snapshot).unwrap_or_default(),
    );

    ExtensionConformanceResult {
        id: ext_id.to_string(),
        tier: entry.conformance_tier,
        status: "pass".to_string(),
        failure_reason: None,
        artifact_path: Some(snapshot_path.display().to_string()),
        commands_registered: observation.commands.len(),
        flags_registered: observation.flags.len(),
        tools_registered: observation.tools.len(),
        providers_registered: observation.providers.len(),
        duration_ms: start.elapsed().as_millis() as u64,
    }
}

/// Generate a comprehensive conformance report for all extensions in the manifest.
///
/// Run: `cargo test --test ext_conformance_generated --features ext-conformance -- conformance_full_report --nocapture`
#[test]
#[allow(clippy::too_many_lines)]
fn conformance_full_report() {
    use chrono::{SecondsFormat, Utc};
    use std::fmt::Write as _;

    let manifest = load_manifest();
    let report_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("ext_conformance")
        .join("reports")
        .join("conformance");
    let _ = std::fs::create_dir_all(&report_dir);

    let mut results: Vec<ExtensionConformanceResult> = Vec::new();
    let total = manifest.extensions.len();

    eprintln!("\n=== Conformance Report Generator (bd-31j) ===");
    eprintln!("  Extensions in manifest: {total}");
    eprintln!("  Checking extensions with available artifacts...\n");

    for (idx, entry) in manifest.extensions.iter().enumerate() {
        let entry_file = artifacts_dir().join(&entry.entry_path);
        if !entry_file.exists() {
            results.push(ExtensionConformanceResult {
                id: entry.id.clone(),
                tier: entry.conformance_tier,
                status: "skip".to_string(),
                failure_reason: Some("Artifact not available".to_string()),
                artifact_path: None,
                commands_registered: 0,
                flags_registered: 0,
                tools_registered: 0,
                providers_registered: 0,
                duration_ms: 0,
            });
            continue;
        }

        eprint!("  [{:>3}/{total}] {:<50} ", idx + 1, entry.id);
        let result = try_conformance(&entry.id);
        eprintln!(
            "{:<6} ({}ms)",
            result.status.to_uppercase(),
            result.duration_ms
        );
        results.push(result);
    }

    // ── Compute statistics ──
    let pass_count = results.iter().filter(|r| r.status == "pass").count();
    let fail_count = results.iter().filter(|r| r.status == "fail").count();
    let skip_count = results.iter().filter(|r| r.status == "skip").count();
    let tested = pass_count + fail_count;
    let pass_rate = if tested > 0 {
        #[allow(clippy::cast_precision_loss)]
        {
            (pass_count as f64) / (tested as f64) * 100.0
        }
    } else {
        0.0
    };

    let by_tier: std::collections::BTreeMap<u32, (usize, usize, usize)> = {
        let mut m = std::collections::BTreeMap::new();
        for r in &results {
            let entry = m.entry(r.tier).or_insert((0, 0, 0));
            match r.status.as_str() {
                "pass" => entry.0 += 1,
                "fail" => entry.1 += 1,
                _ => entry.2 += 1,
            }
        }
        m
    };

    // ── Write JSONL events ──
    let events_path = report_dir.join("conformance_events.jsonl");
    let mut lines: Vec<String> = Vec::new();
    for r in &results {
        let entry = serde_json::json!({
            "schema": "pi.ext.conformance_result.v1",
            "id": r.id,
            "tier": r.tier,
            "status": r.status,
            "failure_reason": r.failure_reason,
            "commands_registered": r.commands_registered,
            "flags_registered": r.flags_registered,
            "tools_registered": r.tools_registered,
            "providers_registered": r.providers_registered,
            "duration_ms": r.duration_ms,
            "ts": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        });
        lines.push(serde_json::to_string(&entry).unwrap_or_default());
    }
    let _ = std::fs::write(&events_path, lines.join("\n") + "\n");

    // ── Write JSON summary ──
    let summary = serde_json::json!({
        "schema": "pi.ext.conformance_report.v1",
        "generated_at": Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        "manifest_count": total,
        "tested": tested,
        "passed": pass_count,
        "failed": fail_count,
        "skipped": skip_count,
        "pass_rate_pct": pass_rate,
        "by_tier": by_tier.iter().map(|(tier, (p, f, s))| {
            serde_json::json!({
                "tier": tier,
                "pass": p,
                "fail": f,
                "skip": s,
            })
        }).collect::<Vec<_>>(),
        "failures": results.iter()
            .filter(|r| r.status == "fail")
            .map(|r| serde_json::json!({
                "id": r.id,
                "tier": r.tier,
                "reason": r.failure_reason,
            }))
            .collect::<Vec<_>>(),
    });
    let summary_path = report_dir.join("conformance_report.json");
    let _ = std::fs::write(
        &summary_path,
        serde_json::to_string_pretty(&summary).unwrap_or_default(),
    );

    // ── Write Markdown report ──
    let mut md = String::new();
    md.push_str("# Extension Conformance Report\n\n");
    let _ = writeln!(
        md,
        "> Generated: {}",
        Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
    );
    md.push_str("\n## Summary\n\n");
    md.push_str("| Metric | Value |\n|--------|-------|\n");
    let _ = writeln!(md, "| Total in manifest | {total} |");
    let _ = writeln!(md, "| Tested | {tested} |");
    let _ = writeln!(md, "| Passed | {pass_count} |");
    let _ = writeln!(md, "| Failed | {fail_count} |");
    let _ = writeln!(md, "| Skipped | {skip_count} |");
    let _ = writeln!(md, "| Pass rate | {pass_rate:.1}% |");
    md.push('\n');

    md.push_str("## By Tier\n\n");
    md.push_str("| Tier | Pass | Fail | Skip |\n|------|------|------|------|\n");
    for (tier, (p, f, s)) in &by_tier {
        let _ = writeln!(md, "| {tier} | {p} | {f} | {s} |");
    }
    md.push('\n');

    if fail_count > 0 {
        md.push_str("## Failures\n\n");
        md.push_str("| Extension | Tier | Reason |\n|-----------|------|--------|\n");
        for r in results.iter().filter(|r| r.status == "fail") {
            let _ = writeln!(
                md,
                "| {} | {} | {} |",
                r.id,
                r.tier,
                r.failure_reason.as_deref().unwrap_or("unknown")
            );
        }
        md.push('\n');
    }

    md.push_str("## All Results\n\n");
    md.push_str("| Extension | Tier | Status | Cmds | Flags | Tools | Providers | Time (ms) |\n");
    md.push_str("|-----------|------|--------|------|-------|-------|-----------|-----------|\n");
    for r in &results {
        let _ = writeln!(
            md,
            "| {} | {} | {} | {} | {} | {} | {} | {} |",
            r.id,
            r.tier,
            r.status,
            r.commands_registered,
            r.flags_registered,
            r.tools_registered,
            r.providers_registered,
            r.duration_ms
        );
    }

    let md_path = report_dir.join("conformance_report.md");
    let _ = std::fs::write(&md_path, &md);

    // ── Print summary ──
    eprintln!("\n=== Conformance Report Summary ===");
    eprintln!("  Manifest: {total} extensions");
    eprintln!("  Tested:   {tested}");
    eprintln!("  Passed:   {pass_count}");
    eprintln!("  Failed:   {fail_count}");
    eprintln!("  Skipped:  {skip_count}");
    eprintln!("  Rate:     {pass_rate:.1}%");
    eprintln!("\n  Reports:");
    eprintln!("    JSON:   {}", summary_path.display());
    eprintln!("    JSONL:  {}", events_path.display());
    eprintln!("    MD:     {}\n", md_path.display());

    // Report is informational — per-extension tests enforce pass/fail individually.
    // Log a warning if failures exist so CI logs are visible but don't block the report.
    if fail_count > 0 {
        eprintln!(
            "  WARNING: {fail_count} extension(s) failed conformance. See {}",
            summary_path.display()
        );
    }
}

// ─── Sharded Extension Matrix Executor (bd-1f42.4.3) ────────────────────────
//
// Runs the full extension corpus with parallel sharding and deterministic
// ordering.  Each shard gets a stable, reproducible subset of extensions
// so CI matrix jobs can fan out.
//
// Environment variables:
//   PI_SHARD_INDEX  — 0-based index of this shard (default: 0)
//   PI_SHARD_TOTAL  — total number of shards (default: 1 = no sharding)
//   PI_SHARD_PARALLELISM — max threads within a shard (default: num_cpus or 4)
//
// Run:
//   cargo test --test ext_conformance_generated --features ext-conformance \
//     -- conformance_sharded_matrix --nocapture
//
// CI matrix example (4 shards):
//   PI_SHARD_INDEX=0 PI_SHARD_TOTAL=4 cargo test ...
//   PI_SHARD_INDEX=1 PI_SHARD_TOTAL=4 cargo test ...
//   PI_SHARD_INDEX=2 PI_SHARD_TOTAL=4 cargo test ...
//   PI_SHARD_INDEX=3 PI_SHARD_TOTAL=4 cargo test ...

/// Failure category for triage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum FailureCategory {
    /// Extension artifact file not found on disk.
    ArtifactMissing,
    /// `JsExtensionLoadSpec::from_entry_path` failed.
    LoadSpecError,
    /// JS runtime failed to start.
    RuntimeStartError,
    /// Extension load (`load_js_extensions`) failed.
    ExtensionLoadError,
    /// Registration mismatch (commands/flags/tools/providers).
    RegistrationMismatch,
    /// Extension not in the validated manifest.
    ManifestMissing,
    /// Unknown / uncategorized failure.
    Unknown,
}

impl FailureCategory {
    /// Classify a failure reason string into a category.
    fn classify(reason: &str) -> Self {
        if reason.contains("Artifact not") || reason.contains("not found") {
            Self::ArtifactMissing
        } else if reason.contains("Load spec error") {
            Self::LoadSpecError
        } else if reason.contains("Runtime start error") {
            Self::RuntimeStartError
        } else if reason.contains("Load error") {
            Self::ExtensionLoadError
        } else if reason.contains("Missing command")
            || reason.contains("Missing flag")
            || reason.contains("expects tools")
            || reason.contains("expects providers")
        {
            Self::RegistrationMismatch
        } else if reason.contains("Not found in VALIDATED_MANIFEST") {
            Self::ManifestMissing
        } else {
            Self::Unknown
        }
    }
}

/// Enhanced result with failure categorization.
#[derive(Debug, serde::Serialize)]
struct ShardedConformanceResult {
    #[serde(flatten)]
    inner: ExtensionConformanceResult,
    /// Classified failure category (present only when status != "pass").
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_category: Option<FailureCategory>,
    /// Shard index that ran this extension.
    shard_index: usize,
}

/// Configuration for sharded execution.
struct ShardConfig {
    /// 0-based shard index.
    shard_index: usize,
    /// Total number of shards.
    shard_total: usize,
    /// Max parallel threads within this shard.
    parallelism: usize,
}

impl ShardConfig {
    /// Read configuration from environment variables with sensible defaults.
    fn from_env() -> Self {
        let shard_index: usize = std::env::var("PI_SHARD_INDEX")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let shard_total: usize = std::env::var("PI_SHARD_TOTAL")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&v| v > 0)
            .unwrap_or(1);
        let parallelism: usize = std::env::var("PI_SHARD_PARALLELISM")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&v| v > 0)
            .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get().min(8)));
        Self {
            shard_index: shard_index.min(shard_total.saturating_sub(1)),
            shard_total,
            parallelism,
        }
    }
}

/// Deterministically assign extensions to shards using stable sorted order.
///
/// Extensions are sorted by ID (lexicographic) for reproducibility, then
/// assigned to shards via round-robin.  This ensures each shard gets a
/// balanced mix of tiers and categories.
fn shard_extensions<'a>(
    extensions: &'a [ManifestEntry],
    config: &ShardConfig,
) -> Vec<&'a ManifestEntry> {
    let mut sorted: Vec<&ManifestEntry> = extensions.iter().collect();
    sorted.sort_by(|a, b| a.id.cmp(&b.id));

    sorted
        .into_iter()
        .enumerate()
        .filter(|(idx, _)| idx % config.shard_total == config.shard_index)
        .map(|(_, entry)| entry)
        .collect()
}

/// Run conformance checks for a slice of extensions in parallel using threads.
///
/// Returns results in deterministic order (sorted by extension ID).
fn run_shard_parallel(
    extensions: &[&ManifestEntry],
    shard_index: usize,
    parallelism: usize,
) -> Vec<ShardedConformanceResult> {
    use std::sync::mpsc;

    let (tx, rx) = mpsc::channel();

    // Process extensions in chunks to control parallelism.
    let chunks: Vec<Vec<String>> = extensions
        .chunks(parallelism)
        .map(|chunk| chunk.iter().map(|e| e.id.clone()).collect())
        .collect();

    for chunk in chunks {
        let handles: Vec<_> = chunk
            .into_iter()
            .map(|ext_id| {
                let tx = tx.clone();
                std::thread::spawn(move || {
                    let result = try_conformance(&ext_id);
                    let failure_category = if result.status == "pass" {
                        None
                    } else {
                        Some(FailureCategory::classify(
                            result.failure_reason.as_deref().unwrap_or(""),
                        ))
                    };
                    let _ = tx.send(ShardedConformanceResult {
                        inner: result,
                        failure_category,
                        shard_index,
                    });
                })
            })
            .collect();

        // Wait for this chunk to complete before starting the next.
        for handle in handles {
            let _ = handle.join();
        }
    }

    drop(tx);

    // Collect and sort by extension ID for deterministic output.
    let mut results: Vec<ShardedConformanceResult> = rx.into_iter().collect();
    results.sort_by(|a, b| a.inner.id.cmp(&b.inner.id));
    results
}

/// Shard report output for CI artifact merging.
#[derive(Debug, serde::Serialize)]
struct ShardReport {
    schema: String,
    generated_at: String,
    shard_index: usize,
    shard_total: usize,
    parallelism: usize,
    manifest_count: usize,
    shard_count: usize,
    tested: usize,
    passed: usize,
    failed: usize,
    skipped: usize,
    pass_rate_pct: f64,
    total_duration_ms: u64,
    by_tier: Vec<serde_json::Value>,
    by_failure_category: Vec<serde_json::Value>,
    failures: Vec<serde_json::Value>,
    results: Vec<ShardedConformanceResult>,
}

/// Run the sharded extension matrix executor.
///
/// Run with:
/// `cargo test --test ext_conformance_generated --features ext-conformance -- conformance_sharded_matrix --nocapture`
#[test]
#[allow(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss
)]
fn conformance_sharded_matrix() {
    use chrono::{SecondsFormat, Utc};
    use std::fmt::Write as _;

    let config = ShardConfig::from_env();
    let manifest = load_manifest();

    let report_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("ext_conformance")
        .join("reports")
        .join("sharded");
    let _ = std::fs::create_dir_all(&report_dir);

    let shard_entries = shard_extensions(&manifest.extensions, &config);
    let shard_count = shard_entries.len();
    let total = manifest.extensions.len();

    eprintln!("\n=== Sharded Extension Matrix Executor (bd-1f42.4.3) ===");
    eprintln!(
        "  Shard:       {}/{}",
        config.shard_index + 1,
        config.shard_total
    );
    eprintln!("  Extensions:  {shard_count}/{total} in this shard");
    eprintln!("  Parallelism: {} threads", config.parallelism);
    eprintln!();

    let start = std::time::Instant::now();
    let results = run_shard_parallel(&shard_entries, config.shard_index, config.parallelism);
    let elapsed = start.elapsed();

    // ── Compute statistics ──
    let pass_count = results.iter().filter(|r| r.inner.status == "pass").count();
    let fail_count = results.iter().filter(|r| r.inner.status == "fail").count();
    let skip_count = results.iter().filter(|r| r.inner.status == "skip").count();
    let tested = pass_count + fail_count;
    let pass_rate = if tested > 0 {
        (pass_count as f64) / (tested as f64) * 100.0
    } else {
        0.0
    };

    // By tier breakdown.
    let by_tier: std::collections::BTreeMap<u32, (usize, usize, usize)> = {
        let mut m = std::collections::BTreeMap::new();
        for r in &results {
            let entry = m.entry(r.inner.tier).or_insert((0, 0, 0));
            match r.inner.status.as_str() {
                "pass" => entry.0 += 1,
                "fail" => entry.1 += 1,
                _ => entry.2 += 1,
            }
        }
        m
    };

    // By failure category breakdown.
    let by_category: std::collections::BTreeMap<String, usize> = {
        let mut m = std::collections::BTreeMap::new();
        for r in &results {
            if let Some(cat) = &r.failure_category {
                let key = serde_json::to_value(cat)
                    .ok()
                    .and_then(|v| v.as_str().map(String::from))
                    .unwrap_or_else(|| format!("{cat:?}"));
                *m.entry(key).or_insert(0) += 1;
            }
        }
        m
    };

    // ── Write JSONL events ──
    let events_path = report_dir.join(format!("shard_{}_events.jsonl", config.shard_index));
    let mut lines: Vec<String> = Vec::new();
    for r in &results {
        let entry = serde_json::json!({
            "schema": "pi.ext.conformance_result.v2",
            "id": r.inner.id,
            "tier": r.inner.tier,
            "status": r.inner.status,
            "failure_reason": r.inner.failure_reason,
            "failure_category": r.failure_category,
            "shard_index": r.shard_index,
            "commands_registered": r.inner.commands_registered,
            "flags_registered": r.inner.flags_registered,
            "tools_registered": r.inner.tools_registered,
            "providers_registered": r.inner.providers_registered,
            "duration_ms": r.inner.duration_ms,
            "ts": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        });
        lines.push(serde_json::to_string(&entry).unwrap_or_default());
    }
    let _ = std::fs::write(&events_path, lines.join("\n") + "\n");

    // ── Write JSON shard report ──
    let report = ShardReport {
        schema: "pi.ext.conformance_shard_report.v1".to_string(),
        generated_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        shard_index: config.shard_index,
        shard_total: config.shard_total,
        parallelism: config.parallelism,
        manifest_count: total,
        shard_count,
        tested,
        passed: pass_count,
        failed: fail_count,
        skipped: skip_count,
        pass_rate_pct: pass_rate,
        total_duration_ms: elapsed.as_millis() as u64,
        by_tier: by_tier
            .iter()
            .map(|(tier, (p, f, s))| {
                serde_json::json!({
                    "tier": tier,
                    "pass": p,
                    "fail": f,
                    "skip": s,
                })
            })
            .collect(),
        by_failure_category: by_category
            .iter()
            .map(|(cat, count)| {
                serde_json::json!({
                    "category": cat,
                    "count": count,
                })
            })
            .collect(),
        failures: results
            .iter()
            .filter(|r| r.inner.status == "fail")
            .map(|r| {
                serde_json::json!({
                    "id": r.inner.id,
                    "tier": r.inner.tier,
                    "reason": r.inner.failure_reason,
                    "category": r.failure_category,
                    "duration_ms": r.inner.duration_ms,
                })
            })
            .collect(),
        results,
    };

    let report_path = report_dir.join(format!("shard_{}_report.json", config.shard_index));
    let _ = std::fs::write(
        &report_path,
        serde_json::to_string_pretty(&report).unwrap_or_default(),
    );

    // ── Write Markdown report ──
    let mut md = String::new();
    let _ = writeln!(
        md,
        "# Extension Conformance — Shard {}/{}\n",
        config.shard_index + 1,
        config.shard_total
    );
    let _ = writeln!(
        md,
        "> Generated: {}  \n> Duration: {}ms  \n> Parallelism: {} threads\n",
        Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        elapsed.as_millis(),
        config.parallelism,
    );
    md.push_str("## Summary\n\n");
    md.push_str("| Metric | Value |\n|--------|-------|\n");
    let _ = writeln!(md, "| Total in manifest | {total} |");
    let _ = writeln!(md, "| In this shard | {shard_count} |");
    let _ = writeln!(md, "| Tested | {tested} |");
    let _ = writeln!(md, "| Passed | {pass_count} |");
    let _ = writeln!(md, "| Failed | {fail_count} |");
    let _ = writeln!(md, "| Skipped | {skip_count} |");
    let _ = writeln!(md, "| Pass rate | {pass_rate:.1}% |");
    let _ = writeln!(md, "| Duration | {}ms |", elapsed.as_millis());
    md.push('\n');

    md.push_str("## By Tier\n\n");
    md.push_str("| Tier | Pass | Fail | Skip |\n|------|------|------|------|\n");
    for (tier, (p, f, s)) in &by_tier {
        let _ = writeln!(md, "| {tier} | {p} | {f} | {s} |");
    }
    md.push('\n');

    if !by_category.is_empty() {
        md.push_str("## Failure Categories\n\n");
        md.push_str("| Category | Count |\n|----------|-------|\n");
        for (cat, count) in &by_category {
            let _ = writeln!(md, "| {cat} | {count} |");
        }
        md.push('\n');
    }

    if fail_count > 0 {
        md.push_str("## Failures\n\n");
        md.push_str(
            "| Extension | Tier | Category | Reason |\n|-----------|------|----------|--------|\n",
        );
        for r in &report.failures {
            let _ = writeln!(
                md,
                "| {} | {} | {} | {} |",
                r.get("id").and_then(|v| v.as_str()).unwrap_or("?"),
                r.get("tier")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
                r.get("category")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown"),
                r.get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown"),
            );
        }
        md.push('\n');
    }

    let md_path = report_dir.join(format!("shard_{}_report.md", config.shard_index));
    let _ = std::fs::write(&md_path, &md);

    // ── Print summary ──
    eprintln!(
        "=== Shard {}/{} Results ===",
        config.shard_index + 1,
        config.shard_total
    );
    eprintln!("  Extensions: {shard_count}/{total}");
    eprintln!("  Tested:     {tested}");
    eprintln!("  Passed:     {pass_count}");
    eprintln!("  Failed:     {fail_count}");
    eprintln!("  Skipped:    {skip_count}");
    eprintln!("  Rate:       {pass_rate:.1}%");
    eprintln!("  Duration:   {}ms", elapsed.as_millis());
    eprintln!("  Parallelism:{} threads", config.parallelism);
    eprintln!();
    eprintln!("  Reports:");
    eprintln!("    JSON:  {}", report_path.display());
    eprintln!("    JSONL: {}", events_path.display());
    eprintln!("    MD:    {}", md_path.display());

    if !by_category.is_empty() {
        eprintln!();
        eprintln!("  Failure categories:");
        for (cat, count) in &by_category {
            eprintln!("    {cat}: {count}");
        }
    }
    eprintln!();

    if fail_count > 0 {
        eprintln!(
            "  WARNING: {fail_count} extension(s) failed. See {}",
            report_path.display()
        );
    }
}

/// Merge multiple shard reports into a consolidated report.
///
/// Run with:
/// `cargo test --test ext_conformance_generated --features ext-conformance -- conformance_merge_shard_reports --nocapture`
#[test]
#[allow(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss
)]
fn conformance_merge_shard_reports() {
    use chrono::{SecondsFormat, Utc};
    use std::fmt::Write as _;

    let report_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("ext_conformance")
        .join("reports")
        .join("sharded");

    // Find all shard report files.
    let mut shard_files: Vec<PathBuf> = std::fs::read_dir(&report_dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("shard_") && n.ends_with("_report.json"))
        })
        .collect();
    shard_files.sort();

    if shard_files.is_empty() {
        eprintln!("No shard reports found in {}", report_dir.display());
        return;
    }

    eprintln!("\n=== Merging {} shard reports ===\n", shard_files.len());

    // Parse all shard reports.
    let mut all_results: Vec<serde_json::Value> = Vec::new();
    let mut total_duration_ms: u64 = 0;
    let mut manifest_count = 0_usize;
    let mut shard_total = 0_usize;

    for path in &shard_files {
        let data = match std::fs::read_to_string(path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("  WARN: Failed to read {}: {e}", path.display());
                continue;
            }
        };
        let report: serde_json::Value = match serde_json::from_str(&data) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("  WARN: Failed to parse {}: {e}", path.display());
                continue;
            }
        };

        manifest_count = report["manifest_count"].as_u64().unwrap_or(0) as usize;
        shard_total = report["shard_total"].as_u64().unwrap_or(0) as usize;
        total_duration_ms =
            total_duration_ms.max(report["total_duration_ms"].as_u64().unwrap_or(0));

        if let Some(results) = report["results"].as_array() {
            all_results.extend(results.iter().cloned());
        }
    }

    // Sort merged results by extension ID.
    all_results.sort_by(|a, b| {
        let a_id = a.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let b_id = b.get("id").and_then(|v| v.as_str()).unwrap_or("");
        a_id.cmp(b_id)
    });

    // Compute merged statistics.
    let pass_count = all_results
        .iter()
        .filter(|r| r.get("status").and_then(|v| v.as_str()) == Some("pass"))
        .count();
    let fail_count = all_results
        .iter()
        .filter(|r| r.get("status").and_then(|v| v.as_str()) == Some("fail"))
        .count();
    let skip_count = all_results
        .iter()
        .filter(|r| r.get("status").and_then(|v| v.as_str()) == Some("skip"))
        .count();
    let tested = pass_count + fail_count;
    let pass_rate = if tested > 0 {
        (pass_count as f64) / (tested as f64) * 100.0
    } else {
        0.0
    };

    let merged = serde_json::json!({
        "schema": "pi.ext.conformance_merged_report.v1",
        "generated_at": Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        "shards_merged": shard_files.len(),
        "shard_total": shard_total,
        "manifest_count": manifest_count,
        "tested": tested,
        "passed": pass_count,
        "failed": fail_count,
        "skipped": skip_count,
        "pass_rate_pct": pass_rate,
        "wall_clock_ms": total_duration_ms,
        "failures": all_results.iter()
            .filter(|r| r.get("status").and_then(|v| v.as_str()) == Some("fail"))
            .cloned()
            .collect::<Vec<_>>(),
    });

    let merged_path = report_dir.join("merged_report.json");
    let _ = std::fs::write(
        &merged_path,
        serde_json::to_string_pretty(&merged).unwrap_or_default(),
    );

    // ── Merged Markdown ──
    let mut md = String::new();
    md.push_str("# Extension Conformance — Merged Report\n\n");
    let _ = writeln!(
        md,
        "> Generated: {}  \n> Shards merged: {}  \n> Wall-clock: {}ms\n",
        Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        shard_files.len(),
        total_duration_ms,
    );
    md.push_str("## Summary\n\n");
    md.push_str("| Metric | Value |\n|--------|-------|\n");
    let _ = writeln!(md, "| Total in manifest | {manifest_count} |");
    let _ = writeln!(md, "| Tested | {tested} |");
    let _ = writeln!(md, "| Passed | {pass_count} |");
    let _ = writeln!(md, "| Failed | {fail_count} |");
    let _ = writeln!(md, "| Skipped | {skip_count} |");
    let _ = writeln!(md, "| Pass rate | {pass_rate:.1}% |");
    md.push('\n');

    if fail_count > 0 {
        md.push_str("## Failures\n\n");
        md.push_str("| Extension | Tier | Category | Shard | Reason |\n|-----------|------|----------|-------|--------|\n");
        for r in all_results
            .iter()
            .filter(|r| r.get("status").and_then(|v| v.as_str()) == Some("fail"))
        {
            let _ = writeln!(
                md,
                "| {} | {} | {} | {} | {} |",
                r.get("id").and_then(|v| v.as_str()).unwrap_or("?"),
                r.get("tier")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
                r.get("failure_category")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown"),
                r.get("shard_index")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
                r.get("failure_reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown"),
            );
        }
    }

    let md_path = report_dir.join("merged_report.md");
    let _ = std::fs::write(&md_path, &md);

    eprintln!("=== Merged Report ===");
    eprintln!("  Shards:  {}", shard_files.len());
    eprintln!("  Total:   {}", all_results.len());
    eprintln!("  Tested:  {tested}");
    eprintln!("  Passed:  {pass_count}");
    eprintln!("  Failed:  {fail_count}");
    eprintln!("  Skipped: {skip_count}");
    eprintln!("  Rate:    {pass_rate:.1}%");
    eprintln!();
    eprintln!("  Reports:");
    eprintln!("    JSON: {}", merged_path.display());
    eprintln!("    MD:   {}\n", md_path.display());
}

// ─── Per-Extension Failure Dossier Generator (bd-1f42.4.8) ──────────────────
//
// Generates high-signal failure dossiers for each failing extension with
// one-command reproduce scripts.  Dossiers are structured JSON files suitable
// for CI artifact linking and rapid triage.
//
// Run:
//   cargo test --test ext_conformance_generated --features ext-conformance \
//     -- conformance_failure_dossiers --nocapture

/// A failure dossier for a single extension.
#[derive(Debug, serde::Serialize)]
struct FailureDossier {
    schema: String,
    generated_at: String,
    extension_id: String,
    extension_tier: u32,
    entry_path: String,
    failure_category: FailureCategory,
    failure_reason: String,
    duration_ms: u64,
    /// Registration state at time of failure (if the extension loaded at all).
    #[serde(skip_serializing_if = "Option::is_none")]
    registration_snapshot: Option<serde_json::Value>,
    /// Expected registrations from the manifest.
    expected_registrations: serde_json::Value,
    /// One-command reproduce script.
    reproduce_command: String,
    /// Cargo test command that targets just this extension.
    cargo_test_command: String,
    /// Environment for reproduction.
    reproduce_env: serde_json::Value,
}

/// Detailed result capturing registration state for dossier generation.
#[allow(clippy::too_many_lines, clippy::cast_possible_truncation)]
fn try_conformance_detailed(
    ext_id: &str,
) -> (ExtensionConformanceResult, Option<serde_json::Value>) {
    let manifest = load_manifest();
    let Some(entry) = manifest.find(ext_id) else {
        return (
            ExtensionConformanceResult {
                id: ext_id.to_string(),
                tier: 0,
                status: "skip".to_string(),
                failure_reason: Some("Not found in VALIDATED_MANIFEST.json".to_string()),
                artifact_path: None,
                commands_registered: 0,
                flags_registered: 0,
                tools_registered: 0,
                providers_registered: 0,
                duration_ms: 0,
            },
            None,
        );
    };

    let start = std::time::Instant::now();
    let cwd = conformance_work_dir(format!("pi-dossier-{}", ext_id.replace('/', "_")));

    let entry_file = artifacts_dir().join(&entry.entry_path);
    if !entry_file.exists() {
        return (
            ExtensionConformanceResult {
                id: ext_id.to_string(),
                tier: entry.conformance_tier,
                status: "fail".to_string(),
                failure_reason: Some(format!("Artifact not found: {}", entry_file.display())),
                artifact_path: None,
                commands_registered: 0,
                flags_registered: 0,
                tools_registered: 0,
                providers_registered: 0,
                duration_ms: start.elapsed().as_millis() as u64,
            },
            None,
        );
    }

    let spec = match JsExtensionLoadSpec::from_entry_path(&entry_file) {
        Ok(s) => s,
        Err(e) => {
            return (
                ExtensionConformanceResult {
                    id: ext_id.to_string(),
                    tier: entry.conformance_tier,
                    status: "fail".to_string(),
                    failure_reason: Some(format!("Load spec error: {e}")),
                    artifact_path: None,
                    commands_registered: 0,
                    flags_registered: 0,
                    tools_registered: 0,
                    providers_registered: 0,
                    duration_ms: start.elapsed().as_millis() as u64,
                },
                None,
            );
        }
    };

    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let js_config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        env: hermetic_conformance_env(&cwd),
        deny_env: false,
        ..Default::default()
    };

    let runtime_result = common::run_async({
        let manager = manager.clone();
        let tools = Arc::clone(&tools);
        async move { JsExtensionRuntimeHandle::start(js_config, tools, manager).await }
    });
    let runtime = match runtime_result {
        Ok(rt) => rt,
        Err(e) => {
            return (
                ExtensionConformanceResult {
                    id: ext_id.to_string(),
                    tier: entry.conformance_tier,
                    status: "fail".to_string(),
                    failure_reason: Some(format!("Runtime start error: {e}")),
                    artifact_path: None,
                    commands_registered: 0,
                    flags_registered: 0,
                    tools_registered: 0,
                    providers_registered: 0,
                    duration_ms: start.elapsed().as_millis() as u64,
                },
                None,
            );
        }
    };
    manager.set_js_runtime(runtime);

    let load_err = common::run_async({
        let manager = manager.clone();
        async move { manager.load_js_extensions(vec![spec]).await }
    });
    if let Err(e) = load_err {
        return (
            ExtensionConformanceResult {
                id: ext_id.to_string(),
                tier: entry.conformance_tier,
                status: "fail".to_string(),
                failure_reason: Some(format!("Load error: {e}")),
                artifact_path: None,
                commands_registered: 0,
                flags_registered: 0,
                tools_registered: 0,
                providers_registered: 0,
                duration_ms: start.elapsed().as_millis() as u64,
            },
            None,
        );
    }

    // Capture registration snapshot for the dossier.
    let snap = snapshot_registrations(&manager);

    // Validate registrations.
    let actual_commands = manager.list_commands();
    let actual_cmd_names: Vec<&str> = actual_commands
        .iter()
        .filter_map(|v| v.get("name").and_then(Value::as_str))
        .collect();

    for expected_cmd in &entry.registrations.commands {
        if !actual_cmd_names.contains(&expected_cmd.as_str()) {
            return (
                ExtensionConformanceResult {
                    id: ext_id.to_string(),
                    tier: entry.conformance_tier,
                    status: "fail".to_string(),
                    failure_reason: Some(format!(
                        "Missing command '{expected_cmd}'. Actual: {actual_cmd_names:?}"
                    )),
                    artifact_path: None,
                    commands_registered: actual_commands.len(),
                    flags_registered: manager.list_flags().len(),
                    tools_registered: manager.extension_tool_defs().len(),
                    providers_registered: manager.extension_providers().len(),
                    duration_ms: start.elapsed().as_millis() as u64,
                },
                Some(snap),
            );
        }
    }

    let actual_flags = manager.list_flags();

    if entry.capabilities.registers_tools && manager.extension_tool_defs().is_empty() {
        return (
            ExtensionConformanceResult {
                id: ext_id.to_string(),
                tier: entry.conformance_tier,
                status: "fail".to_string(),
                failure_reason: Some("Manifest expects tools but none registered".to_string()),
                artifact_path: None,
                commands_registered: actual_commands.len(),
                flags_registered: actual_flags.len(),
                tools_registered: 0,
                providers_registered: manager.extension_providers().len(),
                duration_ms: start.elapsed().as_millis() as u64,
            },
            Some(snap),
        );
    }

    if entry.capabilities.registers_providers && manager.extension_providers().is_empty() {
        return (
            ExtensionConformanceResult {
                id: ext_id.to_string(),
                tier: entry.conformance_tier,
                status: "fail".to_string(),
                failure_reason: Some("Manifest expects providers but none registered".to_string()),
                artifact_path: None,
                commands_registered: actual_commands.len(),
                flags_registered: actual_flags.len(),
                tools_registered: manager.extension_tool_defs().len(),
                providers_registered: 0,
                duration_ms: start.elapsed().as_millis() as u64,
            },
            Some(snap),
        );
    }

    (
        ExtensionConformanceResult {
            id: ext_id.to_string(),
            tier: entry.conformance_tier,
            status: "pass".to_string(),
            failure_reason: None,
            artifact_path: None,
            commands_registered: actual_commands.len(),
            flags_registered: actual_flags.len(),
            tools_registered: manager.extension_tool_defs().len(),
            providers_registered: manager.extension_providers().len(),
            duration_ms: start.elapsed().as_millis() as u64,
        },
        Some(snap),
    )
}

/// Generate failure dossiers for all failing extensions.
///
/// Run with:
/// `cargo test --test ext_conformance_generated --features ext-conformance -- conformance_failure_dossiers --nocapture`
#[test]
#[allow(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss
)]
fn conformance_failure_dossiers() {
    use chrono::{SecondsFormat, Utc};
    use std::fmt::Write as _;

    let manifest = load_manifest();

    let dossier_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("ext_conformance")
        .join("reports")
        .join("dossiers");
    let _ = std::fs::create_dir_all(&dossier_dir);

    let total = manifest.extensions.len();
    eprintln!("\n=== Failure Dossier Generator (bd-1f42.4.8) ===");
    eprintln!("  Extensions in manifest: {total}");
    eprintln!("  Generating dossiers for failures...\n");

    let mut dossiers: Vec<FailureDossier> = Vec::new();
    let mut pass_count = 0_usize;
    let mut fail_count = 0_usize;
    let mut skip_count = 0_usize;

    for (idx, entry) in manifest.extensions.iter().enumerate() {
        let entry_file = artifacts_dir().join(&entry.entry_path);
        if !entry_file.exists() {
            skip_count += 1;
            continue;
        }

        eprint!("  [{:>3}/{total}] {:<50} ", idx + 1, entry.id);
        let (result, snapshot) = try_conformance_detailed(&entry.id);

        match result.status.as_str() {
            "pass" => {
                eprintln!("PASS   ({}ms)", result.duration_ms);
                pass_count += 1;
            }
            "skip" => {
                eprintln!("SKIP");
                skip_count += 1;
            }
            _ => {
                eprintln!("FAIL   ({}ms)", result.duration_ms);
                fail_count += 1;

                let category =
                    FailureCategory::classify(result.failure_reason.as_deref().unwrap_or(""));

                // Build expected registrations from manifest.
                let expected = serde_json::json!({
                    "commands": entry.registrations.commands,
                    "flags": entry.registrations.flags,
                    "tools": entry.registrations.tools,
                    "event_handlers": entry.registrations.event_handlers,
                    "registers_tools": entry.capabilities.registers_tools,
                    "registers_commands": entry.capabilities.registers_commands,
                    "registers_flags": entry.capabilities.registers_flags,
                    "registers_providers": entry.capabilities.registers_providers,
                });

                // Build the sanitized extension ID for test function name.
                let test_fn_name = format!("ext_{}", entry.id.replace(['/', '-'], "_"));

                let reproduce_cmd = format!(
                    "cargo test --test ext_conformance_generated --features ext-conformance -- {test_fn_name} --nocapture --exact"
                );

                let cargo_test_cmd =
                    "cargo test --test ext_conformance_generated --features ext-conformance -- conformance_failure_dossiers --nocapture".to_string();

                let dossier = FailureDossier {
                    schema: "pi.ext.failure_dossier.v1".to_string(),
                    generated_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
                    extension_id: entry.id.clone(),
                    extension_tier: entry.conformance_tier,
                    entry_path: entry.entry_path.clone(),
                    failure_category: category,
                    failure_reason: result
                        .failure_reason
                        .clone()
                        .unwrap_or_else(|| "unknown".to_string()),
                    duration_ms: result.duration_ms,
                    registration_snapshot: snapshot,
                    expected_registrations: expected,
                    reproduce_command: reproduce_cmd.clone(),
                    cargo_test_command: cargo_test_cmd,
                    reproduce_env: serde_json::json!({
                        "RUST_LOG": "debug",
                        "RUST_BACKTRACE": "1",
                    }),
                };

                // Write individual dossier file.
                let dossier_filename = format!("{}.json", entry.id.replace('/', "__"));
                let dossier_path = dossier_dir.join(&dossier_filename);
                let _ = std::fs::write(
                    &dossier_path,
                    serde_json::to_string_pretty(&dossier).unwrap_or_default(),
                );

                dossiers.push(dossier);
            }
        }
    }

    let tested = pass_count + fail_count;
    let pass_rate = if tested > 0 {
        (pass_count as f64) / (tested as f64) * 100.0
    } else {
        0.0
    };

    // ── Write dossier index ──
    let by_category: std::collections::BTreeMap<String, Vec<&FailureDossier>> = {
        let mut m: std::collections::BTreeMap<String, Vec<&FailureDossier>> =
            std::collections::BTreeMap::new();
        for d in &dossiers {
            let key = serde_json::to_value(d.failure_category)
                .ok()
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_else(|| format!("{:?}", d.failure_category));
            m.entry(key).or_default().push(d);
        }
        m
    };

    let index = serde_json::json!({
        "schema": "pi.ext.dossier_index.v1",
        "generated_at": Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        "manifest_count": total,
        "tested": tested,
        "passed": pass_count,
        "failed": fail_count,
        "skipped": skip_count,
        "pass_rate_pct": pass_rate,
        "dossier_count": dossiers.len(),
        "by_category": by_category.iter().map(|(cat, ds)| {
            serde_json::json!({
                "category": cat,
                "count": ds.len(),
                "extensions": ds.iter().map(|d| &d.extension_id).collect::<Vec<_>>(),
            })
        }).collect::<Vec<_>>(),
        "dossiers": dossiers.iter().map(|d| {
            serde_json::json!({
                "extension_id": d.extension_id,
                "tier": d.extension_tier,
                "category": d.failure_category,
                "reason": d.failure_reason,
                "reproduce": d.reproduce_command,
                "file": format!("{}.json", d.extension_id.replace('/', "__")),
            })
        }).collect::<Vec<_>>(),
    });

    let index_path = dossier_dir.join("dossier_index.json");
    let _ = std::fs::write(
        &index_path,
        serde_json::to_string_pretty(&index).unwrap_or_default(),
    );

    // ── Write Markdown triage report ──
    let mut md = String::new();
    md.push_str("# Extension Failure Dossiers\n\n");
    let _ = writeln!(
        md,
        "> Generated: {}\n",
        Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
    );

    md.push_str("## Summary\n\n");
    md.push_str("| Metric | Value |\n|--------|-------|\n");
    let _ = writeln!(md, "| Manifest total | {total} |");
    let _ = writeln!(md, "| Tested | {tested} |");
    let _ = writeln!(md, "| Passed | {pass_count} |");
    let _ = writeln!(md, "| Failed (dossiers) | {fail_count} |");
    let _ = writeln!(md, "| Pass rate | {pass_rate:.1}% |");
    md.push('\n');

    if !by_category.is_empty() {
        md.push_str("## By Failure Category\n\n");
        md.push_str("| Category | Count | Extensions |\n|----------|-------|------------|\n");
        for (cat, ds) in &by_category {
            let ext_list: Vec<&str> = ds.iter().map(|d| d.extension_id.as_str()).collect();
            let display = if ext_list.len() > 3 {
                format!(
                    "{}, ... (+{})",
                    ext_list[..3].join(", "),
                    ext_list.len() - 3
                )
            } else {
                ext_list.join(", ")
            };
            let _ = writeln!(md, "| {cat} | {} | {display} |", ds.len());
        }
        md.push('\n');
    }

    if !dossiers.is_empty() {
        md.push_str("## Failure Details\n\n");
        for d in &dossiers {
            let _ = writeln!(md, "### {} (tier {})\n", d.extension_id, d.extension_tier);
            let _ = writeln!(md, "- **Category:** {:?}", d.failure_category);
            let _ = writeln!(md, "- **Reason:** {}", d.failure_reason);
            let _ = writeln!(md, "- **Duration:** {}ms", d.duration_ms);
            let _ = writeln!(md, "- **Entry path:** `{}`", d.entry_path);
            let _ = writeln!(md, "- **Reproduce:**");
            let _ = writeln!(md, "  ```bash\n  {}\n  ```", d.reproduce_command);
            md.push('\n');
        }
    }

    let md_path = dossier_dir.join("dossier_report.md");
    let _ = std::fs::write(&md_path, &md);

    // ── Print summary ──
    eprintln!("\n=== Dossier Summary ===");
    eprintln!("  Tested:   {tested}");
    eprintln!("  Passed:   {pass_count}");
    eprintln!("  Failed:   {fail_count}");
    eprintln!("  Skipped:  {skip_count}");
    eprintln!("  Rate:     {pass_rate:.1}%");
    eprintln!("  Dossiers: {}", dossiers.len());
    eprintln!();
    eprintln!("  Reports:");
    eprintln!("    Index:  {}", index_path.display());
    eprintln!("    MD:     {}", md_path.display());
    eprintln!("    Dir:    {}", dossier_dir.display());

    if !by_category.is_empty() {
        eprintln!();
        eprintln!("  Failure categories:");
        for (cat, ds) in &by_category {
            eprintln!("    {cat}: {}", ds.len());
        }
    }
    eprintln!();
}

// ─── CI Gate for the Canonical Must-Pass Extensions (bd-1f42.4.4) ───────────
//
// Hard CI gate: blocks merge if any extension in the authoritative inclusion
// list's tier1 + tier1_review sets fails conformance. Manifest tiers 1-5 remain
// attached to results; entries outside the authoritative set are non-blocking.
//
// Run:
//   `cargo test --test ext_conformance_generated --features ext-conformance \
//     -- conformance_must_pass_gate --nocapture`

/// Verdict for the must-pass gate.
#[derive(Debug, serde::Serialize)]
struct MustPassGateVerdict {
    schema: String,
    generated_at: String,
    run_id: String,
    correlation_id: String,
    git_commit: String,
    source_tree_sha256: String,
    inclusion_sha256: String,
    manifest_sha256: String,
    mode: String,
    status: String, // "pass" or "fail"
    thresholds: MustPassThresholds,
    observed: MustPassObserved,
    checks: Vec<serde_json::Value>,
    blocking_failures: Vec<serde_json::Value>,
    stretch_set_summary: serde_json::Value,
}

#[derive(Debug, serde::Serialize)]
struct MustPassThresholds {
    min_pass_rate_pct: f64,
    max_failures: usize,
}

#[derive(Debug, serde::Serialize)]
struct MustPassObserved {
    must_pass_total: usize,
    must_pass_tested: usize,
    must_pass_passed: usize,
    must_pass_failed: usize,
    must_pass_skipped: usize,
    must_pass_pass_rate_pct: f64,
    stretch_total: usize,
    stretch_tested: usize,
    stretch_passed: usize,
    stretch_failed: usize,
    stretch_skipped: usize,
}

fn normalize_optional_env(value: Option<String>) -> Option<String> {
    value
        .map(|candidate| candidate.trim().to_string())
        .filter(|candidate| !candidate.is_empty())
}

fn resolve_must_pass_gate_lineage(
    github_run_id: Option<String>,
    ci_run_id: Option<String>,
    ci_correlation_id: Option<String>,
    now: chrono::DateTime<chrono::Utc>,
) -> (String, String) {
    let run_id = normalize_optional_env(github_run_id)
        .or_else(|| normalize_optional_env(ci_run_id))
        .unwrap_or_else(|| format!("local-{}", now.format("%Y%m%dT%H%M%S%3fZ")));
    let correlation_id = normalize_optional_env(ci_correlation_id)
        .unwrap_or_else(|| format!("must-pass-gate-{run_id}"));
    (run_id, correlation_id)
}

fn must_pass_gate_passes(passed: usize, failed: usize, skipped: usize, total: usize) -> bool {
    total > 0 && passed == total && failed == 0 && skipped == 0
}

fn current_must_pass_gate_lineage(now: chrono::DateTime<chrono::Utc>) -> (String, String) {
    resolve_must_pass_gate_lineage(
        std::env::var("GITHUB_RUN_ID").ok(),
        std::env::var("CI_RUN_ID").ok(),
        std::env::var("CI_CORRELATION_ID").ok(),
        now,
    )
}

#[test]
fn must_pass_lineage_prefers_github_run_and_explicit_correlation() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-02-17T00:00:00Z")
        .expect("parse fixed RFC3339 timestamp")
        .with_timezone(&chrono::Utc);
    let (run_id, correlation_id) = resolve_must_pass_gate_lineage(
        Some(" 12345 ".to_string()),
        Some("fallback-run".to_string()),
        Some(" corr-abc ".to_string()),
        now,
    );
    assert_eq!(run_id, "12345");
    assert_eq!(correlation_id, "corr-abc");
}

#[test]
fn must_pass_lineage_uses_ci_run_id_when_github_run_id_missing() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-02-17T00:00:00Z")
        .expect("parse fixed RFC3339 timestamp")
        .with_timezone(&chrono::Utc);
    let (run_id, correlation_id) =
        resolve_must_pass_gate_lineage(None, Some("ci-777".to_string()), None, now);
    assert_eq!(run_id, "ci-777");
    assert_eq!(correlation_id, "must-pass-gate-ci-777");
}

#[test]
fn must_pass_lineage_falls_back_to_local_run_id_when_env_missing() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-02-17T01:02:03Z")
        .expect("parse fixed RFC3339 timestamp")
        .with_timezone(&chrono::Utc);
    let (run_id, correlation_id) = resolve_must_pass_gate_lineage(
        Some("   ".to_string()),
        Some(String::new()),
        Some("   ".to_string()),
        now,
    );
    assert!(
        run_id.starts_with("local-20260217T010203"),
        "unexpected local run_id format: {run_id}"
    );
    assert_eq!(correlation_id, format!("must-pass-gate-{run_id}"));
}

#[test]
fn must_pass_gate_rejects_skipped_entries_even_when_tested_entries_pass() {
    assert!(must_pass_gate_passes(2, 0, 0, 2));
    assert!(
        !must_pass_gate_passes(1, 0, 1, 2),
        "an untested must-pass entry must block the gate"
    );
    assert!(
        !must_pass_gate_passes(1, 1, 0, 2),
        "a failed must-pass entry must block the gate"
    );
    assert!(
        !must_pass_gate_passes(0, 0, 0, 0),
        "an empty must-pass set must block the gate"
    );
}

fn selection_test_entry(id: &str, conformance_tier: u32) -> ManifestEntry {
    ManifestEntry {
        id: id.to_string(),
        entry_path: format!("{id}.js"),
        conformance_tier,
        capabilities: Capabilities {
            registers_tools: false,
            registers_commands: false,
            registers_flags: false,
            registers_providers: false,
            subscribes_events: Vec::new(),
            is_multi_file: false,
            has_npm_deps: false,
        },
        registrations: Registrations {
            tools: Vec::new(),
            commands: Vec::new(),
            flags: Vec::new(),
            event_handlers: Vec::new(),
        },
    }
}

#[test]
fn authoritative_must_pass_parser_combines_sections_and_rejects_bad_ids() {
    let valid = br#"{
        "schema": "pi.ext.inclusion_list.v1",
        "summary": {"tier1_count": 2, "tier1_review_count": 1, "total_must_pass": 3},
        "tier1": [{"id": "alpha"}, {"id": "beta"}],
        "tier1_review": [{"id": "gamma"}]
    }"#;
    assert_eq!(
        parse_authoritative_must_pass_ids(valid, 3).expect("parse valid inclusion fixture"),
        ["alpha".to_string(), "beta".to_string(), "gamma".to_string()]
    );

    let duplicate = br#"{
        "schema": "pi.ext.inclusion_list.v1",
        "summary": {"tier1_count": 1, "tier1_review_count": 1, "total_must_pass": 2},
        "tier1": [{"id": "duplicate"}],
        "tier1_review": [{"id": "duplicate"}]
    }"#;
    let duplicate_error = parse_authoritative_must_pass_ids(duplicate, 2)
        .expect_err("duplicate inclusion ID must fail closed");
    assert!(duplicate_error.contains("duplicate must-pass id duplicate"));

    let missing = br#"{
        "schema": "pi.ext.inclusion_list.v1",
        "summary": {"tier1_count": 1, "tier1_review_count": 0, "total_must_pass": 1},
        "tier1": [{}],
        "tier1_review": []
    }"#;
    assert!(
        parse_authoritative_must_pass_ids(missing, 1).is_err(),
        "missing inclusion ID must fail closed"
    );

    let malformed = br#"{
        "schema": "pi.ext.inclusion_list.v1",
        "summary": {"tier1_count": 1, "tier1_review_count": 0, "total_must_pass": 1},
        "tier1": [{"id": " padded "}],
        "tier1_review": []
    }"#;
    assert!(
        parse_authoritative_must_pass_ids(malformed, 1).is_err(),
        "non-canonical inclusion ID must fail closed"
    );

    assert!(
        parse_authoritative_must_pass_ids(valid, 2).is_err(),
        "a self-consistent but smaller denominator must fail closed"
    );
    assert!(
        parse_authoritative_must_pass_ids(valid, 4).is_err(),
        "a self-consistent but larger denominator must fail closed"
    );
}

#[test]
fn authoritative_must_pass_selection_uses_exact_ids_and_retains_manifest_tiers() {
    let manifest = Manifest {
        schema: "pi.ext.validated-manifest.v1".to_string(),
        extensions: vec![
            selection_test_entry("alpha", 1),
            selection_test_entry("beta", 5),
            selection_test_entry("outside", 2),
        ],
    };
    let ids = vec!["beta".to_string(), "alpha".to_string()];
    let sets = select_authoritative_conformance_sets(&manifest, &ids)
        .expect("map authoritative IDs to manifest fixture");

    assert_eq!(
        sets.must_pass
            .iter()
            .map(|entry| (entry.id.as_str(), entry.conformance_tier))
            .collect::<Vec<_>>(),
        [("beta", 5), ("alpha", 1)]
    );
    assert_eq!(
        sets.stretch
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<Vec<_>>(),
        ["outside"]
    );

    let unmapped = vec!["missing".to_string()];
    let error = select_authoritative_conformance_sets(&manifest, &unmapped)
        .err()
        .expect("unmapped inclusion ID must fail closed");
    assert!(error.contains("canonical must-pass id missing"));

    let mut duplicate_path_entry = selection_test_entry("beta", 2);
    duplicate_path_entry.entry_path = "alpha.js".to_string();
    let duplicate_path_manifest = Manifest {
        schema: "pi.ext.validated-manifest.v1".to_string(),
        extensions: vec![selection_test_entry("alpha", 1), duplicate_path_entry],
    };
    let error = select_authoritative_conformance_sets(
        &duplicate_path_manifest,
        &["alpha".to_string(), "beta".to_string()],
    )
    .err()
    .expect("duplicate artifact entry paths must fail closed");
    assert!(error.contains("duplicate artifact entry_path"), "{error}");
}

#[test]
fn gate_hardening_manifest_contract_rejects_paths_and_registration_claim_drift() {
    for invalid_path in [
        "../escape.js",
        "nested//entry.js",
        "nested/./entry.js",
        "nested/entry.js/",
        "C:/entry.js",
    ] {
        let mut entry = selection_test_entry("alpha", 1);
        entry.entry_path = invalid_path.to_string();
        let error = validate_manifest_entry_contract(&entry)
            .expect_err("non-canonical artifact paths must fail closed");
        assert!(error.contains("normalized relative"), "{error}");
    }

    let mut entry = selection_test_entry("alpha", 1);
    entry.registrations.tools = vec!["declared-tool".to_string()];
    let error = validate_manifest_entry_contract(&entry)
        .expect_err("declared tool with a false capability must fail closed");
    assert!(error.contains("matching capability is false"), "{error}");

    let mut entry = selection_test_entry("alpha", 1);
    entry.capabilities.subscribes_events = vec!["session_start".to_string()];
    entry.registrations.event_handlers = vec!["session_end".to_string()];
    let error = validate_manifest_entry_contract(&entry)
        .expect_err("divergent event identities must fail closed");
    assert!(error.contains("divergent"), "{error}");
}

#[test]
fn gate_hardening_runtime_registration_contract_requires_capabilities_and_declared_identities() {
    let mut entry = selection_test_entry("alpha", 1);
    entry.capabilities.registers_tools = true;
    entry.registrations.tools = vec!["declared-tool".to_string()];

    let mut actual = RegistrationObservation::default();
    actual.tools.insert("different-tool".to_string());
    let error = validate_registration_observation(&entry, &actual)
        .expect_err("a different runtime tool must not satisfy a declared identity");
    assert!(error.contains("missing declared identities"), "{error}");

    entry.registrations.tools.clear();
    actual.tools.clear();
    let error = validate_registration_observation(&entry, &actual)
        .expect_err("a true capability must require at least one runtime registration");
    assert!(error.contains("capability=true"), "{error}");

    let entry = selection_test_entry("alpha", 1);
    actual.commands.insert("unexpected-command".to_string());
    let error = validate_registration_observation(&entry, &actual)
        .expect_err("a false capability must reject runtime registrations");
    assert!(error.contains("capability=false"), "{error}");
}

#[test]
fn gate_hardening_runtime_registration_contract_requires_exact_event_handlers() {
    let mut entry = selection_test_entry("alpha", 1);
    entry.capabilities.subscribes_events = vec!["session_start".to_string()];
    entry.registrations.event_handlers = vec!["session_start".to_string()];

    let mut actual = RegistrationObservation::default();
    actual.event_handlers.insert("session_end".to_string());
    let error = validate_registration_observation(&entry, &actual)
        .expect_err("a different runtime event handler must fail closed");
    assert!(error.contains("event-handler identities differ"), "{error}");

    actual.event_handlers.clear();
    actual.event_handlers.insert("session_start".to_string());
    validate_registration_observation(&entry, &actual)
        .expect("the exact declared runtime event set must pass");
}

#[test]
fn gate_hardening_runtime_registration_contract_rejects_undeclared_extras() {
    let mut entry = selection_test_entry("alpha", 1);
    entry.capabilities.registers_commands = true;
    entry.capabilities.registers_tools = true;
    entry.capabilities.registers_flags = true;
    entry.registrations.commands = vec!["declared-command".to_string()];
    entry.registrations.tools = vec!["declared-tool".to_string()];
    entry.registrations.flags = vec!["declared-flag".to_string()];

    let mut actual = RegistrationObservation::default();
    actual.commands.insert("declared-command".to_string());
    actual.tools.insert("declared-tool".to_string());
    actual.flags.insert("declared-flag".to_string());
    validate_registration_observation(&entry, &actual)
        .expect("exact declared runtime identities must pass");

    actual.commands.insert("undeclared-command".to_string());
    let error = validate_registration_observation(&entry, &actual)
        .expect_err("an undeclared runtime command must fail closed");
    assert!(
        error.contains("unexpected runtime identities=[\"undeclared-command\"]"),
        "{error}"
    );

    actual.commands.remove("undeclared-command");
    actual.tools.insert("undeclared-tool".to_string());
    let error = validate_registration_observation(&entry, &actual)
        .expect_err("an undeclared runtime tool must fail closed");
    assert!(
        error.contains("unexpected runtime identities=[\"undeclared-tool\"]"),
        "{error}"
    );

    actual.tools.remove("undeclared-tool");
    actual.flags.insert("undeclared-flag".to_string());
    let error = validate_registration_observation(&entry, &actual)
        .expect_err("an undeclared runtime flag must fail closed");
    assert!(
        error.contains("unexpected runtime identities=[\"undeclared-flag\"]"),
        "{error}"
    );

    let mut undeclared_only_entry = selection_test_entry("undeclared-only", 1);
    undeclared_only_entry.capabilities.registers_commands = true;
    undeclared_only_entry.capabilities.registers_tools = true;
    let mut undeclared_only_actual = RegistrationObservation::default();
    undeclared_only_actual
        .commands
        .insert("runtime-only-command".to_string());
    undeclared_only_actual
        .tools
        .insert("runtime-only-tool".to_string());
    let error = validate_registration_observation(&undeclared_only_entry, &undeclared_only_actual)
        .expect_err("capability=true must not implicitly authorize unnamed registrations");
    assert!(
        error.contains("unexpected runtime identities=[\"runtime-only-command\"]"),
        "{error}"
    );

    undeclared_only_entry.capabilities.registers_commands = false;
    undeclared_only_actual.commands.clear();
    let error = validate_registration_observation(&undeclared_only_entry, &undeclared_only_actual)
        .expect_err("capability=true must not implicitly authorize an unnamed tool");
    assert!(
        error.contains("unexpected runtime identities=[\"runtime-only-tool\"]"),
        "{error}"
    );
}

#[test]
fn canonical_contract_currently_maps_all_208_must_pass_ids() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let snapshot =
        capture_must_pass_source_snapshot(repo_root).expect("capture canonical source snapshot");
    let manifest = parse_manifest(&snapshot.manifest_contents).expect("parse canonical manifest");
    let ids = parse_authoritative_must_pass_ids(
        &snapshot.inclusion_contents,
        EXPECTED_CANONICAL_MUST_PASS_EXTENSIONS_V1,
    )
    .expect("validate canonical inclusion list");
    let sets = select_authoritative_conformance_sets(&manifest, &ids)
        .expect("every canonical inclusion ID must map to exactly one manifest entry");
    validate_snapshot_artifact_paths(&snapshot, &manifest)
        .expect("every manifest artifact entry must be tracked by the source commit");

    assert_eq!(
        ids.len(),
        EXPECTED_CANONICAL_MUST_PASS_EXTENSIONS_V1,
        "unexpected canonical must-pass denominator"
    );
    assert_eq!(sets.must_pass.len(), ids.len());
    assert_eq!(
        sets.must_pass.len() + sets.stretch.len(),
        manifest.extensions.len()
    );
}

#[test]
fn canonical_contract_binds_inclusion_list_and_rejects_source_drift() {
    assert!(MUST_PASS_SOURCE_PATHS.contains(&MUST_PASS_INCLUSION_PATH));

    let before = MustPassSourceProvenance {
        git_commit: "commit-a".to_string(),
        source_tree_sha256: "source-a".to_string(),
        inclusion_sha256: "inclusion-a".to_string(),
        manifest_sha256: "manifest-a".to_string(),
    };
    assert!(ensure_must_pass_source_unchanged(&before, &before).is_ok());

    let after = MustPassSourceProvenance {
        inclusion_sha256: "inclusion-b".to_string(),
        ..before.clone()
    };
    let error = ensure_must_pass_source_unchanged(&before, &after)
        .expect_err("inclusion-list source drift must fail closed");
    assert!(error.contains("inclusion_sha256"));
}

/// CI gate test that blocks merge if any canonical inclusion-list extension fails.
///
/// Run with:
/// `cargo test --test ext_conformance_generated --features ext-conformance -- conformance_must_pass_gate --nocapture`
#[test]
#[allow(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss
)]
fn conformance_must_pass_gate() {
    use chrono::{SecondsFormat, Utc};
    use std::fmt::Write as _;

    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let source_snapshot = capture_must_pass_source_snapshot(repo_root)
        .expect("capture coherent must-pass release source snapshot");
    let source_provenance = &source_snapshot.provenance;
    let git_commit = source_provenance.git_commit.clone();
    let manifest = parse_manifest(&source_snapshot.manifest_contents)
        .expect("parse canonical extension manifest from source commit");
    let authoritative_ids = parse_authoritative_must_pass_ids(
        &source_snapshot.inclusion_contents,
        EXPECTED_CANONICAL_MUST_PASS_EXTENSIONS_V1,
    )
    .expect("validate canonical extension inclusion list");
    let AuthoritativeConformanceSets { must_pass, stretch } =
        select_authoritative_conformance_sets(&manifest, &authoritative_ids)
            .expect("map canonical inclusion-list IDs to the validated manifest");
    validate_snapshot_artifact_paths(&source_snapshot, &manifest)
        .expect("bind every manifest artifact entry to the canonical source commit");

    // The canonical release-blocking corpus is deliberately not configurable:
    // every authoritative inclusion-list entry must run and pass.
    let min_pass_rate = 100.0;
    let max_failures = 0;
    let mode = "strict".to_string();
    let now = Utc::now();
    let generated_at = now.to_rfc3339_opts(SecondsFormat::Millis, true);
    let (run_id, correlation_id) = current_must_pass_gate_lineage(now);

    let report_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("ext_conformance")
        .join("reports")
        .join("gate");
    let _ = std::fs::create_dir_all(&report_dir);

    eprintln!("\n=== Must-Pass Extension CI Gate (bd-1f42.4.4) ===");
    eprintln!("  Mode:            {mode}");
    eprintln!("  Run ID:          {run_id}");
    eprintln!("  Correlation ID:  {correlation_id}");
    eprintln!("  Git commit:      {git_commit}");
    eprintln!(
        "  Source digest:   {}",
        source_provenance.source_tree_sha256
    );
    eprintln!("  Inclusion digest: {}", source_provenance.inclusion_sha256);
    eprintln!("  Manifest digest: {}", source_provenance.manifest_sha256);
    eprintln!(
        "  Must-pass set:   {} canonical inclusion-list extensions",
        must_pass.len()
    );
    eprintln!(
        "  Stretch set:     {} manifest extensions outside the canonical set",
        stretch.len()
    );
    eprintln!("  Min pass rate:   {min_pass_rate:.1}%");
    eprintln!("  Max failures:    {max_failures}");
    eprintln!();

    // ── Run must-pass extensions ──
    eprintln!("  Running must-pass extensions...");
    let mut mp_results: Vec<ExtensionConformanceResult> = Vec::with_capacity(must_pass.len());
    for (idx, entry) in must_pass.iter().enumerate() {
        let entry_file = artifacts_dir().join(&entry.entry_path);
        if !entry_file.exists() {
            mp_results.push(ExtensionConformanceResult {
                id: entry.id.clone(),
                tier: entry.conformance_tier,
                status: "skip".to_string(),
                failure_reason: Some("Artifact not available".to_string()),
                artifact_path: None,
                commands_registered: 0,
                flags_registered: 0,
                tools_registered: 0,
                providers_registered: 0,
                duration_ms: 0,
            });
            continue;
        }
        eprint!("  [{:>3}/{}] {:<50} ", idx + 1, must_pass.len(), entry.id);
        let result = try_conformance_with_manifest(&manifest, &entry.id);
        eprintln!(
            "{:<6} ({}ms)",
            result.status.to_uppercase(),
            result.duration_ms
        );
        mp_results.push(result);
    }

    // ── Run stretch-set extensions (non-blocking) ──
    eprintln!("\n  Running stretch-set extensions (non-blocking)...");
    let mut stretch_results: Vec<ExtensionConformanceResult> = Vec::with_capacity(stretch.len());
    for (idx, entry) in stretch.iter().enumerate() {
        let entry_file = artifacts_dir().join(&entry.entry_path);
        if !entry_file.exists() {
            stretch_results.push(ExtensionConformanceResult {
                id: entry.id.clone(),
                tier: entry.conformance_tier,
                status: "skip".to_string(),
                failure_reason: Some("Artifact not available".to_string()),
                artifact_path: None,
                commands_registered: 0,
                flags_registered: 0,
                tools_registered: 0,
                providers_registered: 0,
                duration_ms: 0,
            });
            continue;
        }
        eprint!("  [{:>3}/{}] {:<50} ", idx + 1, stretch.len(), entry.id);
        let result = try_conformance_with_manifest(&manifest, &entry.id);
        eprintln!(
            "{:<6} ({}ms)",
            result.status.to_uppercase(),
            result.duration_ms
        );
        stretch_results.push(result);
    }

    let source_snapshot_after = capture_must_pass_source_snapshot(repo_root)
        .expect("must-pass source inputs changed or became untracked during execution");
    ensure_must_pass_source_unchanged(source_provenance, &source_snapshot_after.provenance)
        .expect("must-pass source provenance changed during extension execution");

    // ── Compute statistics ──
    let mp_pass = mp_results.iter().filter(|r| r.status == "pass").count();
    let mp_fail = mp_results.iter().filter(|r| r.status == "fail").count();
    let mp_skip = mp_results.iter().filter(|r| r.status == "skip").count();
    let mp_tested = mp_pass + mp_fail;
    let mp_pass_rate = if mp_tested > 0 {
        (mp_pass as f64) / (mp_tested as f64) * 100.0
    } else {
        0.0
    };

    let st_pass = stretch_results
        .iter()
        .filter(|r| r.status == "pass")
        .count();
    let st_fail = stretch_results
        .iter()
        .filter(|r| r.status == "fail")
        .count();
    let st_skip = stretch_results
        .iter()
        .filter(|r| r.status == "skip")
        .count();
    let st_tested = st_pass + st_fail;

    // ── Gate checks ──
    let rate_ok = mp_pass_rate == min_pass_rate;
    let count_ok = mp_fail == max_failures;
    let coverage_ok = mp_tested == must_pass.len();
    let gate_pass = must_pass_gate_passes(mp_pass, mp_fail, mp_skip, must_pass.len());

    let status = if gate_pass { "pass" } else { "fail" }.to_string();

    // ── Build blocking failure list with reproduce commands ──
    let blocking_failures: Vec<serde_json::Value> = mp_results
        .iter()
        .filter(|r| r.status != "pass")
        .map(|r| {
            let test_fn = format!("ext_{}", r.id.replace(['/', '-'], "_"));
            let category = FailureCategory::classify(r.failure_reason.as_deref().unwrap_or(""));
            serde_json::json!({
                "extension_id": r.id,
                "tier": r.tier,
                "failure_category": category,
                "failure_reason": r.failure_reason,
                "reproduce_command": format!(
                    "cargo test --test ext_conformance_generated --features ext-conformance -- {test_fn} --nocapture --exact"
                ),
                "dossier_command": "cargo test --test ext_conformance_generated --features ext-conformance -- conformance_failure_dossiers --nocapture",
                "duration_ms": r.duration_ms,
            })
        })
        .collect();
    let has_blocking_failures = !blocking_failures.is_empty();

    let checks = vec![
        serde_json::json!({
            "id": "must_pass_rate",
            "description": "Must-pass set pass rate meets minimum threshold",
            "actual": mp_pass_rate,
            "threshold": min_pass_rate,
            "ok": rate_ok,
        }),
        serde_json::json!({
            "id": "must_pass_failure_count",
            "description": "Must-pass set failure count within maximum",
            "actual": mp_fail,
            "threshold": max_failures,
            "ok": count_ok,
        }),
        serde_json::json!({
            "id": "must_pass_complete_coverage",
            "description": "Every canonical must-pass entry was exercised",
            "actual": mp_tested,
            "threshold": must_pass.len(),
            "ok": coverage_ok,
        }),
    ];

    let verdict = MustPassGateVerdict {
        schema: "pi.ext.must_pass_gate.v1".to_string(),
        generated_at,
        run_id: run_id.clone(),
        correlation_id: correlation_id.clone(),
        git_commit: git_commit.clone(),
        source_tree_sha256: source_provenance.source_tree_sha256.clone(),
        inclusion_sha256: source_provenance.inclusion_sha256.clone(),
        manifest_sha256: source_provenance.manifest_sha256.clone(),
        mode: mode.clone(),
        status: status.clone(),
        thresholds: MustPassThresholds {
            min_pass_rate_pct: min_pass_rate,
            max_failures,
        },
        observed: MustPassObserved {
            must_pass_total: must_pass.len(),
            must_pass_tested: mp_tested,
            must_pass_passed: mp_pass,
            must_pass_failed: mp_fail,
            must_pass_skipped: mp_skip,
            must_pass_pass_rate_pct: mp_pass_rate,
            stretch_total: stretch.len(),
            stretch_tested: st_tested,
            stretch_passed: st_pass,
            stretch_failed: st_fail,
            stretch_skipped: st_skip,
        },
        checks,
        blocking_failures,
        stretch_set_summary: serde_json::json!({
            "note": "Stretch-set results are informational and non-blocking",
            "total": stretch.len(),
            "tested": st_tested,
            "passed": st_pass,
            "failed": st_fail,
            "skipped": st_skip,
        }),
    };

    // ── Write gate verdict JSON ──
    let verdict_path = report_dir.join("must_pass_gate_verdict.json");
    let verdict_json =
        serde_json::to_string_pretty(&verdict).expect("serialize must-pass gate verdict JSON");
    assert!(
        !verdict_json.trim().is_empty(),
        "must-pass gate verdict serialization unexpectedly empty"
    );
    std::fs::write(&verdict_path, verdict_json).expect("write must-pass gate verdict JSON");

    // ── Write JSONL events for each must-pass result ──
    let events_path = report_dir.join("must_pass_events.jsonl");
    let mut event_lines: Vec<String> = Vec::new();
    for r in &mp_results {
        let line = serde_json::json!({
            "schema": "pi.ext.gate_event.v1",
            "set": "must_pass",
            "run_id": run_id.clone(),
            "correlation_id": correlation_id.clone(),
            "git_commit": git_commit.clone(),
            "source_tree_sha256": source_provenance.source_tree_sha256.clone(),
            "inclusion_sha256": source_provenance.inclusion_sha256.clone(),
            "manifest_sha256": source_provenance.manifest_sha256.clone(),
            "id": r.id,
            "tier": r.tier,
            "status": r.status,
            "failure_reason": r.failure_reason,
            "duration_ms": r.duration_ms,
            "ts": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        });
        event_lines
            .push(serde_json::to_string(&line).expect("serialize must-pass gate event JSONL row"));
    }
    for r in &stretch_results {
        let line = serde_json::json!({
            "schema": "pi.ext.gate_event.v1",
            "set": "stretch",
            "run_id": run_id.clone(),
            "correlation_id": correlation_id.clone(),
            "git_commit": git_commit.clone(),
            "source_tree_sha256": source_provenance.source_tree_sha256.clone(),
            "inclusion_sha256": source_provenance.inclusion_sha256.clone(),
            "manifest_sha256": source_provenance.manifest_sha256.clone(),
            "id": r.id,
            "tier": r.tier,
            "status": r.status,
            "failure_reason": r.failure_reason,
            "duration_ms": r.duration_ms,
            "ts": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        });
        event_lines
            .push(serde_json::to_string(&line).expect("serialize stretch gate event JSONL row"));
    }
    let events_payload = event_lines.join("\n") + "\n";
    assert!(
        !events_payload.trim().is_empty(),
        "must-pass gate event payload unexpectedly empty"
    );
    std::fs::write(&events_path, events_payload).expect("write must-pass gate events JSONL");

    // ── Write Markdown gate report ──
    let mut md = String::new();
    md.push_str("# Must-Pass Extension CI Gate Report\n\n");
    let _ = writeln!(
        md,
        "> Generated: {}",
        Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
    );
    let _ = writeln!(md, "> Run ID: {run_id}");
    let _ = writeln!(md, "> Correlation ID: {correlation_id}");
    let _ = writeln!(md, "> Git commit: `{git_commit}`");
    let _ = writeln!(
        md,
        "> Source tree SHA-256: `{}`",
        source_provenance.source_tree_sha256
    );
    let _ = writeln!(
        md,
        "> Inclusion-list SHA-256: `{}`",
        source_provenance.inclusion_sha256
    );
    let _ = writeln!(
        md,
        "> Manifest SHA-256: `{}`",
        source_provenance.manifest_sha256
    );
    let _ = writeln!(md, "> Mode: {mode}\n");

    md.push_str("## Gate Verdict\n\n");
    let _ = writeln!(md, "**Status: {}**\n", status.to_uppercase());

    md.push_str(
        "| Check | Actual | Threshold | Result |\n|-------|--------|-----------|--------|\n",
    );
    let _ = writeln!(
        md,
        "| Pass rate | {mp_pass_rate:.1}% | >={min_pass_rate:.1}% | {} |",
        if rate_ok { "PASS" } else { "FAIL" }
    );
    let _ = writeln!(
        md,
        "| Failure count | {mp_fail} | <={max_failures} | {} |",
        if count_ok { "PASS" } else { "FAIL" }
    );
    let _ = writeln!(
        md,
        "| Complete coverage | {mp_tested}/{} | {}/{} | {} |",
        must_pass.len(),
        must_pass.len(),
        must_pass.len(),
        if coverage_ok { "PASS" } else { "FAIL" }
    );
    md.push('\n');

    md.push_str("## Canonical Must-Pass Set (Tier-1 + Tier-1 Review)\n\n");
    md.push_str("| Metric | Value |\n|--------|-------|\n");
    let _ = writeln!(md, "| Total | {} |", must_pass.len());
    let _ = writeln!(md, "| Tested | {mp_tested} |");
    let _ = writeln!(md, "| Passed | {mp_pass} |");
    let _ = writeln!(md, "| Failed | {mp_fail} |");
    let _ = writeln!(md, "| Skipped | {mp_skip} |");
    let _ = writeln!(md, "| Pass rate | {mp_pass_rate:.1}% |");
    md.push('\n');

    if has_blocking_failures {
        md.push_str("## Blocking Failures\n\n");
        for r in mp_results.iter().filter(|r| r.status != "pass") {
            let test_fn = format!("ext_{}", r.id.replace(['/', '-'], "_"));
            let _ = writeln!(md, "### {}\n", r.id);
            let _ = writeln!(md, "- **Tier:** {}", r.tier);
            let _ = writeln!(
                md,
                "- **Reason:** {}",
                r.failure_reason.as_deref().unwrap_or("unknown")
            );
            let _ = writeln!(
                md,
                "- **Category:** {:?}",
                FailureCategory::classify(r.failure_reason.as_deref().unwrap_or(""))
            );
            let _ = writeln!(md, "- **Duration:** {}ms", r.duration_ms);
            let _ = writeln!(md, "- **Reproduce:**");
            let _ = writeln!(
                md,
                "  ```bash\n  cargo test --test ext_conformance_generated --features ext-conformance -- {test_fn} --nocapture --exact\n  ```"
            );
            md.push('\n');
        }
    }

    md.push_str("## Manifest Entries Outside the Canonical Set — Non-Blocking\n\n");
    md.push_str("| Metric | Value |\n|--------|-------|\n");
    let _ = writeln!(md, "| Total | {} |", stretch.len());
    let _ = writeln!(md, "| Tested | {st_tested} |");
    let _ = writeln!(md, "| Passed | {st_pass} |");
    let _ = writeln!(md, "| Failed | {st_fail} |");
    let _ = writeln!(md, "| Skipped | {st_skip} |");
    md.push('\n');

    let md_path = report_dir.join("must_pass_gate_report.md");
    std::fs::write(&md_path, &md).expect("write must-pass gate report Markdown");

    let verdict_len = std::fs::metadata(&verdict_path)
        .expect("stat must-pass gate verdict JSON")
        .len();
    let events_len = std::fs::metadata(&events_path)
        .expect("stat must-pass gate events JSONL")
        .len();
    let md_len = std::fs::metadata(&md_path)
        .expect("stat must-pass gate report Markdown")
        .len();
    assert!(
        verdict_len > 0,
        "must-pass gate verdict JSON is empty after write"
    );
    assert!(
        events_len > 0,
        "must-pass gate events JSONL is empty after write"
    );
    assert!(
        md_len > 0,
        "must-pass gate report Markdown is empty after write"
    );

    // ── Print summary ──
    eprintln!("\n=== Must-Pass Gate Verdict ===");
    eprintln!("  Status:     {}", status.to_uppercase());
    eprintln!("  Mode:       {mode}");
    eprintln!("  Must-pass:  {mp_pass}/{mp_tested} passed ({mp_pass_rate:.1}%)");
    eprintln!("  Stretch:    {st_pass}/{st_tested} passed (non-blocking)");
    if has_blocking_failures {
        eprintln!("  Blocking failures:");
        for r in mp_results.iter().filter(|r| r.status != "pass") {
            eprintln!(
                "    - {} ({})",
                r.id,
                r.failure_reason.as_deref().unwrap_or("unknown")
            );
        }
    }
    eprintln!();
    eprintln!("  Reports:");
    eprintln!("    Verdict: {}", verdict_path.display());
    eprintln!("    Events:  {}", events_path.display());
    eprintln!("    MD:      {}", md_path.display());
    eprintln!();

    let final_source_snapshot = capture_must_pass_source_snapshot(repo_root)
        .expect("must-pass source inputs changed while writing gate evidence");
    ensure_must_pass_source_unchanged(source_provenance, &final_source_snapshot.provenance)
        .expect("must-pass source provenance changed while writing gate evidence");

    // ── Hard gate: every canonical must-pass extension must pass ──
    assert!(
        gate_pass,
        "GATE BLOCKED: Must-pass extension conformance failed.\n\
         Pass rate: {mp_pass_rate:.1}% (min: {min_pass_rate:.1}%)\n\
         Failures: {mp_fail} (max: {max_failures})\n\
         Skipped: {mp_skip} (must be 0)\n\
         See: {}\n\
         Run dossiers: cargo test --test ext_conformance_generated --features ext-conformance -- conformance_failure_dossiers --nocapture",
        verdict_path.display()
    );
}

// ─── Provider Compatibility Matrix (bd-1f42.4.6) ────────────────────────────
//
// Validates extension behavior across simulated provider backend configurations.
// Each provider mode defines a different set of available host capabilities,
// reflecting what a real provider backend would expose to extensions.
//
// Run:
//   `cargo test --test ext_conformance_generated --features ext-conformance \
//     -- conformance_provider_compat_matrix --nocapture`

/// A simulated provider mode with a name and the capabilities it exposes.
#[derive(Debug, Clone)]
struct ProviderMode {
    /// Identifier for this provider mode (e.g., `anthropic_streaming`).
    name: &'static str,
    /// Description of what this mode simulates.
    description: &'static str,
    /// Environment overrides applied when running extensions in this mode.
    env_overrides: Vec<(&'static str, &'static str)>,
}

/// Result of testing one extension in one provider mode.
#[derive(Debug, serde::Serialize)]
struct CompatibilityCell {
    extension_id: String,
    extension_tier: u32,
    provider_mode: String,
    status: String, // "pass", "fail", "skip"
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_category: Option<FailureCategory>,
    duration_ms: u64,
    commands_registered: usize,
    flags_registered: usize,
    tools_registered: usize,
    providers_registered: usize,
}

/// Full provider compatibility matrix report.
#[derive(Debug, serde::Serialize)]
struct ProviderCompatReport {
    schema: String,
    generated_at: String,
    provider_modes: Vec<serde_json::Value>,
    must_pass_count: usize,
    mode_count: usize,
    total_cells: usize,
    passed_cells: usize,
    failed_cells: usize,
    skipped_cells: usize,
    cell_pass_rate_pct: f64,
    provider_failures: Vec<serde_json::Value>,
    per_mode_summary: Vec<serde_json::Value>,
}

/// Define the provider modes for the compatibility matrix.
///
/// Each mode applies different environment overrides that influence the
/// extension runtime behavior (e.g., different provider prefixes, API
/// styles, or capability restrictions).
fn provider_modes() -> Vec<ProviderMode> {
    vec![
        ProviderMode {
            name: "default",
            description: "Default configuration (no provider-specific overrides)",
            env_overrides: vec![],
        },
        ProviderMode {
            name: "anthropic_streaming",
            description: "Anthropic Messages API with SSE streaming",
            env_overrides: vec![
                ("PI_DETERMINISTIC_PROVIDER_HINT", "anthropic"),
                ("PI_DETERMINISTIC_API_STYLE", "anthropic_messages"),
            ],
        },
        ProviderMode {
            name: "openai_completions",
            description: "OpenAI Chat Completions API",
            env_overrides: vec![
                ("PI_DETERMINISTIC_PROVIDER_HINT", "openai"),
                ("PI_DETERMINISTIC_API_STYLE", "openai_completions"),
            ],
        },
        ProviderMode {
            name: "openai_responses",
            description: "OpenAI Responses API (reasoning models)",
            env_overrides: vec![
                ("PI_DETERMINISTIC_PROVIDER_HINT", "openai"),
                ("PI_DETERMINISTIC_API_STYLE", "openai_responses"),
            ],
        },
        ProviderMode {
            name: "gemini_generative",
            description: "Google Gemini / GenerativeAI",
            env_overrides: vec![
                ("PI_DETERMINISTIC_PROVIDER_HINT", "google"),
                ("PI_DETERMINISTIC_API_STYLE", "google_generative_ai"),
            ],
        },
        ProviderMode {
            name: "openai_compatible",
            description: "Generic OpenAI-compatible endpoint (e.g., groq, deepseek, xai)",
            env_overrides: vec![
                ("PI_DETERMINISTIC_PROVIDER_HINT", "openai_compatible"),
                ("PI_DETERMINISTIC_API_STYLE", "openai_completions"),
            ],
        },
    ]
}

/// Run conformance for a single extension with runtime environment overrides.
///
/// Unlike [`try_conformance`], this creates a `PiJsRuntimeConfig` with the
/// given env overrides passed through the config `env` map, so no process-level
/// environment mutation is needed.
#[allow(clippy::too_many_lines, clippy::cast_possible_truncation)]
fn try_conformance_with_env(
    ext_id: &str,
    env_overrides: &[(&str, &str)],
) -> ExtensionConformanceResult {
    let manifest = load_manifest();
    let Some(entry) = manifest.find(ext_id) else {
        return ExtensionConformanceResult {
            id: ext_id.to_string(),
            tier: 0,
            status: "skip".to_string(),
            failure_reason: Some("Not found in VALIDATED_MANIFEST.json".to_string()),
            artifact_path: None,
            commands_registered: 0,
            flags_registered: 0,
            tools_registered: 0,
            providers_registered: 0,
            duration_ms: 0,
        };
    };

    let start = std::time::Instant::now();
    let cwd = conformance_work_dir(format!("pi-compat-matrix-{}", ext_id.replace('/', "_")));

    let entry_file = artifacts_dir().join(&entry.entry_path);
    if !entry_file.exists() {
        return ExtensionConformanceResult {
            id: ext_id.to_string(),
            tier: entry.conformance_tier,
            status: "skip".to_string(),
            failure_reason: Some(format!("Artifact not found: {}", entry_file.display())),
            artifact_path: None,
            commands_registered: 0,
            flags_registered: 0,
            tools_registered: 0,
            providers_registered: 0,
            duration_ms: start.elapsed().as_millis() as u64,
        };
    }

    let spec = match JsExtensionLoadSpec::from_entry_path(&entry_file) {
        Ok(s) => s,
        Err(e) => {
            return ExtensionConformanceResult {
                id: ext_id.to_string(),
                tier: entry.conformance_tier,
                status: "fail".to_string(),
                failure_reason: Some(format!("Load spec error: {e}")),
                artifact_path: None,
                commands_registered: 0,
                flags_registered: 0,
                tools_registered: 0,
                providers_registered: 0,
                duration_ms: start.elapsed().as_millis() as u64,
            };
        }
    };

    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));

    // Build env map with overrides.
    let mut env = hermetic_conformance_env(&cwd);
    env.extend(
        env_overrides
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string())),
    );

    let js_config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        env,
        deny_env: false,
        ..Default::default()
    };

    let runtime_result = common::run_async({
        let manager = manager.clone();
        let tools = Arc::clone(&tools);
        async move { JsExtensionRuntimeHandle::start(js_config, tools, manager).await }
    });
    let runtime = match runtime_result {
        Ok(rt) => rt,
        Err(e) => {
            return ExtensionConformanceResult {
                id: ext_id.to_string(),
                tier: entry.conformance_tier,
                status: "fail".to_string(),
                failure_reason: Some(format!("Runtime start error: {e}")),
                artifact_path: None,
                commands_registered: 0,
                flags_registered: 0,
                tools_registered: 0,
                providers_registered: 0,
                duration_ms: start.elapsed().as_millis() as u64,
            };
        }
    };
    manager.set_js_runtime(runtime);

    let load_err = common::run_async({
        let manager = manager.clone();
        async move { manager.load_js_extensions(vec![spec]).await }
    });
    if let Err(e) = load_err {
        return ExtensionConformanceResult {
            id: ext_id.to_string(),
            tier: entry.conformance_tier,
            status: "fail".to_string(),
            failure_reason: Some(format!("Load error: {e}")),
            artifact_path: None,
            commands_registered: 0,
            flags_registered: 0,
            tools_registered: 0,
            providers_registered: 0,
            duration_ms: start.elapsed().as_millis() as u64,
        };
    }

    // Validate registrations against manifest.
    let actual_commands = manager.list_commands();
    let actual_cmd_names: Vec<&str> = actual_commands
        .iter()
        .filter_map(|v| v.get("name").and_then(Value::as_str))
        .collect();

    for expected_cmd in &entry.registrations.commands {
        if !actual_cmd_names.contains(&expected_cmd.as_str()) {
            return ExtensionConformanceResult {
                id: ext_id.to_string(),
                tier: entry.conformance_tier,
                status: "fail".to_string(),
                failure_reason: Some(format!(
                    "Missing command '{expected_cmd}'. Actual: {actual_cmd_names:?}"
                )),
                artifact_path: None,
                commands_registered: actual_commands.len(),
                flags_registered: manager.list_flags().len(),
                tools_registered: manager.extension_tool_defs().len(),
                providers_registered: manager.extension_providers().len(),
                duration_ms: start.elapsed().as_millis() as u64,
            };
        }
    }

    let actual_flags = manager.list_flags();
    let actual_flag_names: Vec<&str> = actual_flags
        .iter()
        .filter_map(|v| v.get("name").and_then(Value::as_str))
        .collect();

    for expected_flag in &entry.registrations.flags {
        if !actual_flag_names.contains(&expected_flag.as_str()) {
            return ExtensionConformanceResult {
                id: ext_id.to_string(),
                tier: entry.conformance_tier,
                status: "fail".to_string(),
                failure_reason: Some(format!(
                    "Missing flag '{expected_flag}'. Actual: {actual_flag_names:?}"
                )),
                artifact_path: None,
                commands_registered: actual_commands.len(),
                flags_registered: actual_flags.len(),
                tools_registered: manager.extension_tool_defs().len(),
                providers_registered: manager.extension_providers().len(),
                duration_ms: start.elapsed().as_millis() as u64,
            };
        }
    }

    if entry.capabilities.registers_tools && manager.extension_tool_defs().is_empty() {
        return ExtensionConformanceResult {
            id: ext_id.to_string(),
            tier: entry.conformance_tier,
            status: "fail".to_string(),
            failure_reason: Some("Manifest expects tools but none registered".to_string()),
            artifact_path: None,
            commands_registered: actual_commands.len(),
            flags_registered: actual_flags.len(),
            tools_registered: 0,
            providers_registered: manager.extension_providers().len(),
            duration_ms: start.elapsed().as_millis() as u64,
        };
    }

    if entry.capabilities.registers_providers && manager.extension_providers().is_empty() {
        return ExtensionConformanceResult {
            id: ext_id.to_string(),
            tier: entry.conformance_tier,
            status: "fail".to_string(),
            failure_reason: Some("Manifest expects providers but none registered".to_string()),
            artifact_path: None,
            commands_registered: actual_commands.len(),
            flags_registered: actual_flags.len(),
            tools_registered: manager.extension_tool_defs().len(),
            providers_registered: 0,
            duration_ms: start.elapsed().as_millis() as u64,
        };
    }

    ExtensionConformanceResult {
        id: ext_id.to_string(),
        tier: entry.conformance_tier,
        status: "pass".to_string(),
        failure_reason: None,
        artifact_path: None,
        commands_registered: actual_commands.len(),
        flags_registered: actual_flags.len(),
        tools_registered: manager.extension_tool_defs().len(),
        providers_registered: manager.extension_providers().len(),
        duration_ms: start.elapsed().as_millis() as u64,
    }
}

/// Provider compatibility matrix test.
///
/// Run with:
/// `cargo test --test ext_conformance_generated --features ext-conformance -- conformance_provider_compat_matrix --nocapture`
#[test]
#[allow(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss
)]
fn conformance_provider_compat_matrix() {
    use chrono::{SecondsFormat, Utc};
    use std::fmt::Write as _;

    let manifest = load_manifest();
    let modes = provider_modes();

    let report_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("ext_conformance")
        .join("reports")
        .join("provider_compat");
    let _ = std::fs::create_dir_all(&report_dir);

    // Only test must-pass (tier 1-2) extensions with available artifacts.
    let must_pass: Vec<&ManifestEntry> = manifest
        .extensions
        .iter()
        .filter(|e| e.conformance_tier <= 2)
        .filter(|e| artifacts_dir().join(&e.entry_path).exists())
        .collect();

    let total_cells = must_pass.len() * modes.len();

    eprintln!("\n=== Provider Compatibility Matrix (bd-1f42.4.6) ===");
    eprintln!(
        "  Extensions:    {} must-pass with artifacts",
        must_pass.len()
    );
    eprintln!("  Provider modes: {}", modes.len());
    eprintln!("  Total cells:   {total_cells}");
    eprintln!();

    let mut cells: Vec<CompatibilityCell> = Vec::with_capacity(total_cells);

    for mode in &modes {
        eprintln!("  --- Mode: {} ({}) ---", mode.name, mode.description);

        for (idx, entry) in must_pass.iter().enumerate() {
            eprint!("  [{:>3}/{}] {:<50} ", idx + 1, must_pass.len(), entry.id);
            let result = try_conformance_with_env(&entry.id, &mode.env_overrides);
            let fc = if result.status == "pass" {
                None
            } else {
                Some(FailureCategory::classify(
                    result.failure_reason.as_deref().unwrap_or(""),
                ))
            };
            eprintln!(
                "{:<6} ({}ms)",
                result.status.to_uppercase(),
                result.duration_ms
            );

            cells.push(CompatibilityCell {
                extension_id: entry.id.clone(),
                extension_tier: entry.conformance_tier,
                provider_mode: mode.name.to_string(),
                status: result.status,
                failure_reason: result.failure_reason,
                failure_category: fc,
                duration_ms: result.duration_ms,
                commands_registered: result.commands_registered,
                flags_registered: result.flags_registered,
                tools_registered: result.tools_registered,
                providers_registered: result.providers_registered,
            });
        }
        eprintln!();
    }

    // ── Compute statistics ──
    let passed_cells = cells.iter().filter(|c| c.status == "pass").count();
    let failed_cells = cells.iter().filter(|c| c.status == "fail").count();
    let skipped_cells = cells.iter().filter(|c| c.status == "skip").count();
    let tested_cells = passed_cells + failed_cells;
    let cell_pass_rate = if tested_cells > 0 {
        (passed_cells as f64) / (tested_cells as f64) * 100.0
    } else {
        0.0
    };

    // ── Per-mode summary ──
    let per_mode_summary: Vec<serde_json::Value> = modes
        .iter()
        .map(|mode| {
            let mode_cells: Vec<&CompatibilityCell> = cells
                .iter()
                .filter(|c| c.provider_mode == mode.name)
                .collect();
            let mp = mode_cells.iter().filter(|c| c.status == "pass").count();
            let mf = mode_cells.iter().filter(|c| c.status == "fail").count();
            let ms = mode_cells.iter().filter(|c| c.status == "skip").count();
            let mt = mp + mf;
            let mr = if mt > 0 {
                (mp as f64) / (mt as f64) * 100.0
            } else {
                0.0
            };
            serde_json::json!({
                "mode": mode.name,
                "description": mode.description,
                "tested": mt,
                "passed": mp,
                "failed": mf,
                "skipped": ms,
                "pass_rate_pct": mr,
            })
        })
        .collect();

    // ── Identify provider-specific failures (fail in one mode, pass in default) ──
    let provider_failures: Vec<serde_json::Value> = cells
        .iter()
        .filter(|c| c.status == "fail" && c.provider_mode != "default")
        .filter(|c| {
            cells.iter().any(|d| {
                d.extension_id == c.extension_id
                    && d.provider_mode == "default"
                    && d.status == "pass"
            })
        })
        .map(|c| {
            serde_json::json!({
                "extension_id": c.extension_id,
                "tier": c.extension_tier,
                "provider_mode": c.provider_mode,
                "failure_reason": c.failure_reason,
                "failure_category": c.failure_category,
                "note": "Passes in default mode but fails in this provider mode",
            })
        })
        .collect();

    // ── Build report ──
    let report = ProviderCompatReport {
        schema: "pi.ext.provider_compat_matrix.v1".to_string(),
        generated_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        provider_modes: modes
            .iter()
            .map(|m| {
                serde_json::json!({
                    "name": m.name,
                    "description": m.description,
                    "env_overrides": m.env_overrides.iter()
                        .map(|(k, v)| serde_json::json!({ "key": k, "value": v }))
                        .collect::<Vec<_>>(),
                })
            })
            .collect(),
        must_pass_count: must_pass.len(),
        mode_count: modes.len(),
        total_cells,
        passed_cells,
        failed_cells,
        skipped_cells,
        cell_pass_rate_pct: cell_pass_rate,
        provider_failures,
        per_mode_summary,
    };

    // ── Write JSON report ──
    let report_path = report_dir.join("provider_compat_report.json");
    let _ = std::fs::write(
        &report_path,
        serde_json::to_string_pretty(&report).unwrap_or_default(),
    );

    // ── Write JSONL events ──
    let events_path = report_dir.join("provider_compat_events.jsonl");
    let mut event_lines: Vec<String> = Vec::new();
    for c in &cells {
        let line = serde_json::json!({
            "schema": "pi.ext.provider_compat_event.v1",
            "extension_id": c.extension_id,
            "tier": c.extension_tier,
            "provider_mode": c.provider_mode,
            "status": c.status,
            "failure_reason": c.failure_reason,
            "failure_category": c.failure_category,
            "duration_ms": c.duration_ms,
            "ts": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        });
        event_lines.push(serde_json::to_string(&line).unwrap_or_default());
    }
    let _ = std::fs::write(&events_path, event_lines.join("\n") + "\n");

    // ── Write per-cell artifact directory ──
    let cells_dir = report_dir.join("cells");
    let _ = std::fs::create_dir_all(&cells_dir);
    for c in &cells {
        let cell_file = cells_dir.join(format!(
            "{}_{}.json",
            c.extension_id.replace('/', "__"),
            c.provider_mode
        ));
        let _ = std::fs::write(
            &cell_file,
            serde_json::to_string_pretty(c).unwrap_or_default(),
        );
    }

    // ── Write Markdown report ──
    let mut md = String::new();
    md.push_str("# Provider Compatibility Matrix Report\n\n");
    let _ = writeln!(
        md,
        "> Generated: {}\n",
        Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
    );

    md.push_str("## Summary\n\n");
    md.push_str("| Metric | Value |\n|--------|-------|\n");
    let _ = writeln!(md, "| Extensions | {} |", must_pass.len());
    let _ = writeln!(md, "| Provider modes | {} |", modes.len());
    let _ = writeln!(md, "| Total cells | {total_cells} |");
    let _ = writeln!(md, "| Passed | {passed_cells} |");
    let _ = writeln!(md, "| Failed | {failed_cells} |");
    let _ = writeln!(md, "| Skipped | {skipped_cells} |");
    let _ = writeln!(md, "| Cell pass rate | {cell_pass_rate:.1}% |");
    md.push('\n');

    md.push_str("## Per-Mode Results\n\n");
    md.push_str("| Mode | Tested | Passed | Failed | Skipped | Rate |\n");
    md.push_str("|------|--------|--------|--------|---------|------|\n");
    for ms in &report.per_mode_summary {
        let _ = writeln!(
            md,
            "| {} | {} | {} | {} | {} | {:.1}% |",
            ms["mode"].as_str().unwrap_or(""),
            ms["tested"],
            ms["passed"],
            ms["failed"],
            ms["skipped"],
            ms["pass_rate_pct"].as_f64().unwrap_or(0.0),
        );
    }
    md.push('\n');

    if !report.provider_failures.is_empty() {
        md.push_str("## Provider-Specific Failures\n\n");
        md.push_str(
            "These extensions pass in default mode but fail in a specific provider mode:\n\n",
        );
        md.push_str("| Extension | Tier | Mode | Reason |\n|-----------|------|------|--------|\n");
        for pf in &report.provider_failures {
            let _ = writeln!(
                md,
                "| {} | {} | {} | {} |",
                pf["extension_id"].as_str().unwrap_or(""),
                pf["tier"],
                pf["provider_mode"].as_str().unwrap_or(""),
                pf["failure_reason"]
                    .as_str()
                    .unwrap_or("unknown")
                    .chars()
                    .take(80)
                    .collect::<String>(),
            );
        }
        md.push('\n');
    }

    let md_path = report_dir.join("provider_compat_report.md");
    let _ = std::fs::write(&md_path, &md);

    // ── Print summary ──
    eprintln!("=== Provider Compatibility Matrix Summary ===");
    eprintln!("  Total cells:        {total_cells}");
    eprintln!("  Passed:             {passed_cells}");
    eprintln!("  Failed:             {failed_cells}");
    eprintln!("  Skipped:            {skipped_cells}");
    eprintln!("  Cell pass rate:     {cell_pass_rate:.1}%");
    if !report.provider_failures.is_empty() {
        eprintln!(
            "  Provider-specific:  {} failures",
            report.provider_failures.len()
        );
    }
    eprintln!();
    eprintln!("  Reports:");
    eprintln!("    JSON:   {}", report_path.display());
    eprintln!("    JSONL:  {}", events_path.display());
    eprintln!("    MD:     {}", md_path.display());
    eprintln!("    Cells:  {}", cells_dir.display());
    eprintln!();
}

// ─── End-User CLI Extension Journeys (bd-1f42.4.7) ──────────────────────────
//
// Validates extension behavior through user-realistic CLI journeys.
// Each extension category (Tool, Command, EventHook, Provider, Configuration)
// has a journey template that tests the registration-to-invocation lifecycle.
//
// Run:
//   `cargo test --test ext_conformance_generated --features ext-conformance \
//     -- conformance_extension_journeys --nocapture`

/// Journey category that maps to how users interact with extensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum JourneyCategory {
    /// Extension registers tools for the agent to invoke.
    ToolProvider,
    /// Extension registers slash commands for user interaction.
    CommandProvider,
    /// Extension subscribes to lifecycle events.
    EventSubscriber,
    /// Extension registers as an LLM provider.
    ModelProvider,
    /// Extension registers CLI flags or shortcuts.
    ConfigProvider,
    /// Extension registers multiple types of capabilities.
    MultiCapability,
    /// Extension with no specific registration pattern.
    Passive,
}

impl JourneyCategory {
    /// Classify an extension into a journey category based on its capabilities.
    fn classify(entry: &ManifestEntry) -> Self {
        let has_tools = entry.capabilities.registers_tools;
        let has_commands = entry.capabilities.registers_commands;
        let has_providers = entry.capabilities.registers_providers;
        let has_flags = entry.capabilities.registers_flags;
        let has_events = !entry.capabilities.subscribes_events.is_empty();

        let cap_count = usize::from(has_tools)
            + usize::from(has_commands)
            + usize::from(has_providers)
            + usize::from(has_flags)
            + usize::from(has_events);

        if cap_count >= 2 {
            Self::MultiCapability
        } else if has_providers {
            Self::ModelProvider
        } else if has_tools {
            Self::ToolProvider
        } else if has_commands {
            Self::CommandProvider
        } else if has_events {
            Self::EventSubscriber
        } else if has_flags {
            Self::ConfigProvider
        } else {
            Self::Passive
        }
    }

    /// Description of what the journey tests for this category.
    const fn journey_description(self) -> &'static str {
        match self {
            Self::ToolProvider => "Load extension -> verify tool registration -> check tool schema",
            Self::CommandProvider => {
                "Load extension -> verify command registration -> check command metadata"
            }
            Self::EventSubscriber => {
                "Load extension -> verify event handler registration -> check subscriptions"
            }
            Self::ModelProvider => {
                "Load extension -> verify provider registration -> check model entries"
            }
            Self::ConfigProvider => {
                "Load extension -> verify flag/shortcut registration -> check flag metadata"
            }
            Self::MultiCapability => {
                "Load extension -> verify all registration types -> cross-check capabilities"
            }
            Self::Passive => {
                "Load extension -> verify basic activation -> check no registration errors"
            }
        }
    }
}

/// Result of a single extension journey.
#[derive(Debug, serde::Serialize)]
struct JourneyResult {
    extension_id: String,
    extension_tier: u32,
    journey_category: JourneyCategory,
    journey_description: String,
    status: String, // "pass", "fail", "skip"
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_step: Option<String>,
    reproduce_command: String,
    duration_ms: u64,
    steps_completed: usize,
    steps_total: usize,
    registrations: serde_json::Value,
}

/// Run the category-specific journey for an extension.
#[allow(clippy::too_many_lines, clippy::cast_possible_truncation)]
fn run_extension_journey(ext_id: &str) -> JourneyResult {
    let manifest = load_manifest();
    let Some(entry) = manifest.find(ext_id) else {
        return JourneyResult {
            extension_id: ext_id.to_string(),
            extension_tier: 0,
            journey_category: JourneyCategory::Passive,
            journey_description: String::new(),
            status: "skip".to_string(),
            failure_reason: Some("Not in manifest".to_string()),
            failure_step: None,
            reproduce_command: String::new(),
            duration_ms: 0,
            steps_completed: 0,
            steps_total: 0,
            registrations: serde_json::json!({}),
        };
    };

    let category = JourneyCategory::classify(entry);
    let test_fn = format!("ext_{}", ext_id.replace(['/', '-'], "_"));
    let reproduce = format!(
        "cargo test --test ext_conformance_generated --features ext-conformance -- {test_fn} --nocapture --exact"
    );

    let start = std::time::Instant::now();

    // Step 1: Load extension (via try_conformance).
    let base_result = try_conformance(ext_id);

    if base_result.status != "pass" {
        return JourneyResult {
            extension_id: ext_id.to_string(),
            extension_tier: entry.conformance_tier,
            journey_category: category,
            journey_description: category.journey_description().to_string(),
            status: "fail".to_string(),
            failure_reason: base_result.failure_reason,
            failure_step: Some("load_extension".to_string()),
            reproduce_command: reproduce,
            duration_ms: start.elapsed().as_millis() as u64,
            steps_completed: 0,
            steps_total: category_step_count(category),
            registrations: serde_json::json!({}),
        };
    }

    // Step 2+: Category-specific journey validation.
    let (step_failures, steps_ok, regs) = run_category_journey(ext_id, entry, category);

    let total_steps = 1 + category_step_count(category);
    let completed = 1 + steps_ok;

    if let Some((step_name, reason)) = step_failures {
        JourneyResult {
            extension_id: ext_id.to_string(),
            extension_tier: entry.conformance_tier,
            journey_category: category,
            journey_description: category.journey_description().to_string(),
            status: "fail".to_string(),
            failure_reason: Some(reason),
            failure_step: Some(step_name),
            reproduce_command: reproduce,
            duration_ms: start.elapsed().as_millis() as u64,
            steps_completed: completed,
            steps_total: total_steps,
            registrations: regs,
        }
    } else {
        JourneyResult {
            extension_id: ext_id.to_string(),
            extension_tier: entry.conformance_tier,
            journey_category: category,
            journey_description: category.journey_description().to_string(),
            status: "pass".to_string(),
            failure_reason: None,
            failure_step: None,
            reproduce_command: reproduce,
            duration_ms: start.elapsed().as_millis() as u64,
            steps_completed: completed,
            steps_total: total_steps,
            registrations: regs,
        }
    }
}

/// Number of validation steps for a journey category (excluding the load step).
const fn category_step_count(cat: JourneyCategory) -> usize {
    match cat {
        JourneyCategory::ToolProvider
        | JourneyCategory::CommandProvider
        | JourneyCategory::EventSubscriber
        | JourneyCategory::ModelProvider
        | JourneyCategory::ConfigProvider => 2,
        JourneyCategory::MultiCapability => 3,
        JourneyCategory::Passive => 1,
    }
}

/// Run the category-specific journey steps after successful load.
///
/// Returns `(first_failure, steps_completed_before_failure, registrations)`.
#[allow(clippy::too_many_lines)]
fn run_category_journey(
    ext_id: &str,
    entry: &ManifestEntry,
    category: JourneyCategory,
) -> (Option<(String, String)>, usize, serde_json::Value) {
    // Re-load extension to get fresh registration state for journey checks.
    let cwd = conformance_work_dir(format!("pi-journey-{}", ext_id.replace('/', "_")));
    prepare_extension_fixture(ext_id, &cwd);

    let entry_file = artifacts_dir().join(&entry.entry_path);
    let spec = match JsExtensionLoadSpec::from_entry_path(&entry_file) {
        Ok(s) => s,
        Err(e) => {
            return (
                Some(("load_spec".to_string(), format!("Load spec error: {e}"))),
                0,
                serde_json::json!({}),
            );
        }
    };

    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let js_config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        env: hermetic_conformance_env(&cwd),
        deny_env: false,
        ..Default::default()
    };

    let runtime_result = common::run_async({
        let manager = manager.clone();
        let tools = Arc::clone(&tools);
        async move { JsExtensionRuntimeHandle::start(js_config, tools, manager).await }
    });
    let runtime = match runtime_result {
        Ok(rt) => rt,
        Err(e) => {
            return (
                Some((
                    "runtime_start".to_string(),
                    format!("Runtime start error: {e}"),
                )),
                0,
                serde_json::json!({}),
            );
        }
    };
    manager.set_js_runtime(runtime);

    if let Err(e) = common::run_async({
        let manager = manager.clone();
        async move { manager.load_js_extensions(vec![spec]).await }
    }) {
        return (
            Some(("load_extension".to_string(), format!("Load error: {e}"))),
            0,
            serde_json::json!({}),
        );
    }

    // Capture registration state.
    let regs = snapshot_registrations(&manager);
    let commands = manager.list_commands();
    let flags = manager.list_flags();
    let tool_defs = manager.extension_tool_defs();
    let providers = manager.extension_providers();

    // Run category-specific checks.
    match category {
        JourneyCategory::ToolProvider => {
            // Step: verify tool registration
            if entry.capabilities.registers_tools && tool_defs.is_empty() {
                return (
                    Some((
                        "verify_tool_registration".to_string(),
                        "Manifest expects tools but none registered".to_string(),
                    )),
                    0,
                    regs,
                );
            }
            // Step: check tool schemas have required fields
            for td in &tool_defs {
                if td.get("name").and_then(Value::as_str).is_none() {
                    return (
                        Some((
                            "check_tool_schema".to_string(),
                            format!("Tool definition missing 'name' field: {td}"),
                        )),
                        1,
                        regs,
                    );
                }
            }
            (None, 2, regs)
        }
        JourneyCategory::CommandProvider => {
            // Step: verify command registration
            for expected_cmd in &entry.registrations.commands {
                let found = commands
                    .iter()
                    .any(|c| c.get("name").and_then(Value::as_str) == Some(expected_cmd.as_str()));
                if !found {
                    return (
                        Some((
                            "verify_command_registration".to_string(),
                            format!("Missing command '{expected_cmd}'"),
                        )),
                        0,
                        regs,
                    );
                }
            }
            // Step: check command metadata has name field
            for cmd in &commands {
                if cmd.get("name").and_then(Value::as_str).is_none() {
                    return (
                        Some((
                            "check_command_metadata".to_string(),
                            format!("Command missing 'name': {cmd}"),
                        )),
                        1,
                        regs,
                    );
                }
            }
            (None, 2, regs)
        }
        JourneyCategory::EventSubscriber => {
            // Step: verify event handler registration
            let handlers = regs
                .get("event_handlers")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            if handlers == 0 && !entry.capabilities.subscribes_events.is_empty() {
                return (
                    Some((
                        "verify_event_registration".to_string(),
                        "Manifest expects event subscriptions but none registered".to_string(),
                    )),
                    0,
                    regs,
                );
            }
            // Step: check subscription names are non-empty
            if let Some(arr) = regs.get("event_handlers").and_then(Value::as_array) {
                for h in arr {
                    if h.as_str().is_none_or(str::is_empty) {
                        return (
                            Some((
                                "check_event_subscriptions".to_string(),
                                format!("Empty event handler name: {h}"),
                            )),
                            1,
                            regs,
                        );
                    }
                }
            }
            (None, 2, regs)
        }
        JourneyCategory::ModelProvider => {
            // Step: verify provider registration
            if entry.capabilities.registers_providers && providers.is_empty() {
                return (
                    Some((
                        "verify_provider_registration".to_string(),
                        "Manifest expects providers but none registered".to_string(),
                    )),
                    0,
                    regs,
                );
            }
            // Step: check provider entries have model field
            for p in &providers {
                if p.get("models").and_then(Value::as_array).is_none()
                    && p.get("model").and_then(Value::as_str).is_none()
                {
                    return (
                        Some((
                            "check_provider_model_entries".to_string(),
                            format!("Provider missing models: {p}"),
                        )),
                        1,
                        regs,
                    );
                }
            }
            (None, 2, regs)
        }
        JourneyCategory::ConfigProvider => {
            // Step: verify flag registration
            for expected_flag in &entry.registrations.flags {
                let found = flags
                    .iter()
                    .any(|f| f.get("name").and_then(Value::as_str) == Some(expected_flag.as_str()));
                if !found {
                    return (
                        Some((
                            "verify_flag_registration".to_string(),
                            format!("Missing flag '{expected_flag}'"),
                        )),
                        0,
                        regs,
                    );
                }
            }
            // Step: check flag metadata has name
            for f in &flags {
                if f.get("name").and_then(Value::as_str).is_none() {
                    return (
                        Some((
                            "check_flag_metadata".to_string(),
                            format!("Flag missing 'name': {f}"),
                        )),
                        1,
                        regs,
                    );
                }
            }
            (None, 2, regs)
        }
        JourneyCategory::MultiCapability => {
            // Step: verify all registration types present
            let expected_types: Vec<&str> = [
                (entry.capabilities.registers_tools, "tools"),
                (entry.capabilities.registers_commands, "commands"),
                (entry.capabilities.registers_flags, "flags"),
                (entry.capabilities.registers_providers, "providers"),
            ]
            .iter()
            .filter(|(has, _)| *has)
            .map(|(_, name)| *name)
            .collect();

            for typ in &expected_types {
                let present = match *typ {
                    "tools" => !tool_defs.is_empty(),
                    "commands" => !commands.is_empty(),
                    "flags" => !flags.is_empty(),
                    "providers" => !providers.is_empty(),
                    _ => true,
                };
                if !present {
                    return (
                        Some((
                            "verify_all_registrations".to_string(),
                            format!("Expected {typ} registration but none found"),
                        )),
                        0,
                        regs,
                    );
                }
            }
            // Step: cross-check - each registered name is non-empty
            // (already validated by try_conformance, but journey confirms)
            // Step: consistency - manifest counts match actual counts
            let cmd_match = entry.registrations.commands.len() <= commands.len();
            let flag_match = entry.registrations.flags.len() <= flags.len();
            if !cmd_match || !flag_match {
                return (
                    Some((
                        "consistency_check".to_string(),
                        format!(
                            "Registration count mismatch: commands({}/{}), flags({}/{})",
                            entry.registrations.commands.len(),
                            commands.len(),
                            entry.registrations.flags.len(),
                            flags.len()
                        ),
                    )),
                    2,
                    regs,
                );
            }
            (None, 3, regs)
        }
        JourneyCategory::Passive => {
            // Step: verify no errors (extension loaded, that's the test)
            (None, 1, regs)
        }
    }
}

/// End-user CLI extension journey test for the canonical tier 1-2 must-pass set.
///
/// Run with:
/// `cargo test --test ext_conformance_generated --features ext-conformance -- conformance_extension_journeys --nocapture`
#[test]
#[allow(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss
)]
fn conformance_extension_journeys() {
    use chrono::{SecondsFormat, Utc};
    use std::fmt::Write as _;

    let manifest = load_manifest();
    let report_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("ext_conformance")
        .join("reports")
        .join("journeys");
    let _ = std::fs::create_dir_all(&report_dir);

    let must_pass: Vec<&ManifestEntry> = manifest
        .extensions
        .iter()
        .filter(|e| e.conformance_tier <= 2)
        .collect();

    eprintln!("\n=== End-User CLI Extension Journeys (bd-1f42.4.7) ===");
    eprintln!("  Extensions: {} must-pass (tier 1-2)", must_pass.len());
    eprintln!();

    let mut results: Vec<JourneyResult> = Vec::with_capacity(must_pass.len());

    for (idx, entry) in must_pass.iter().enumerate() {
        let category = JourneyCategory::classify(entry);
        eprint!(
            "  [{:>3}/{}] {:<45} {:?} ",
            idx + 1,
            must_pass.len(),
            entry.id,
            category,
        );

        let entry_file = artifacts_dir().join(&entry.entry_path);
        if !entry_file.exists() {
            eprintln!("SKIP  (no artifact)");
            let test_fn = format!("ext_{}", entry.id.replace(['/', '-'], "_"));
            results.push(JourneyResult {
                extension_id: entry.id.clone(),
                extension_tier: entry.conformance_tier,
                journey_category: category,
                journey_description: category.journey_description().to_string(),
                status: "skip".to_string(),
                failure_reason: Some("Artifact not available".to_string()),
                failure_step: None,
                reproduce_command: format!(
                    "cargo test --test ext_conformance_generated --features ext-conformance -- {test_fn} --nocapture --exact"
                ),
                duration_ms: 0,
                steps_completed: 0,
                steps_total: 0,
                registrations: serde_json::json!({}),
            });
            continue;
        }

        let result = run_extension_journey(&entry.id);
        eprintln!(
            "{:<6} ({}/{} steps, {}ms)",
            result.status.to_uppercase(),
            result.steps_completed,
            result.steps_total,
            result.duration_ms,
        );
        results.push(result);
    }

    // ── Statistics ──
    let pass_count = results.iter().filter(|r| r.status == "pass").count();
    let fail_count = results.iter().filter(|r| r.status == "fail").count();
    let skip_count = results.iter().filter(|r| r.status == "skip").count();
    let tested = pass_count + fail_count;
    let pass_rate = if tested > 0 {
        (pass_count as f64) / (tested as f64) * 100.0
    } else {
        0.0
    };

    // ── By category ──
    let by_category: std::collections::BTreeMap<String, (usize, usize, usize)> = {
        let mut m = std::collections::BTreeMap::new();
        for r in &results {
            let key = serde_json::to_value(r.journey_category)
                .ok()
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_else(|| format!("{:?}", r.journey_category));
            let e = m.entry(key).or_insert((0, 0, 0));
            match r.status.as_str() {
                "pass" => e.0 += 1,
                "fail" => e.1 += 1,
                _ => e.2 += 1,
            }
        }
        m
    };

    // ── Write JSONL events ──
    let events_path = report_dir.join("journey_events.jsonl");
    let mut lines: Vec<String> = Vec::new();
    for r in &results {
        let line = serde_json::json!({
            "schema": "pi.ext.journey_event.v1",
            "extension_id": r.extension_id,
            "tier": r.extension_tier,
            "journey_category": r.journey_category,
            "status": r.status,
            "failure_reason": r.failure_reason,
            "failure_step": r.failure_step,
            "steps_completed": r.steps_completed,
            "steps_total": r.steps_total,
            "duration_ms": r.duration_ms,
            "ts": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        });
        lines.push(serde_json::to_string(&line).unwrap_or_default());
    }
    let _ = std::fs::write(&events_path, lines.join("\n") + "\n");

    // ── Write JSON report ──
    let report = serde_json::json!({
        "schema": "pi.ext.journey_report.v1",
        "generated_at": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        "must_pass_count": must_pass.len(),
        "tested": tested,
        "passed": pass_count,
        "failed": fail_count,
        "skipped": skip_count,
        "pass_rate_pct": pass_rate,
        "by_category": by_category.iter().map(|(cat, (p, f, s))| {
            serde_json::json!({
                "category": cat,
                "passed": p,
                "failed": f,
                "skipped": s,
            })
        }).collect::<Vec<_>>(),
        "failures": results.iter()
            .filter(|r| r.status == "fail")
            .map(|r| serde_json::json!({
                "extension_id": r.extension_id,
                "tier": r.extension_tier,
                "category": r.journey_category,
                "failure_step": r.failure_step,
                "failure_reason": r.failure_reason,
                "reproduce": r.reproduce_command,
            }))
            .collect::<Vec<_>>(),
    });
    let report_path = report_dir.join("journey_report.json");
    let _ = std::fs::write(
        &report_path,
        serde_json::to_string_pretty(&report).unwrap_or_default(),
    );

    // ── Write per-extension journey artifacts ──
    let details_dir = report_dir.join("details");
    let _ = std::fs::create_dir_all(&details_dir);
    for r in &results {
        let detail_file = details_dir.join(format!("{}.json", r.extension_id.replace('/', "__")));
        let _ = std::fs::write(
            &detail_file,
            serde_json::to_string_pretty(r).unwrap_or_default(),
        );
    }

    // ── Write Markdown report ──
    let mut md = String::new();
    md.push_str("# End-User CLI Extension Journey Report\n\n");
    let _ = writeln!(
        md,
        "> Generated: {}\n",
        Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
    );

    md.push_str("## Summary\n\n");
    md.push_str("| Metric | Value |\n|--------|-------|\n");
    let _ = writeln!(md, "| Must-pass total | {} |", must_pass.len());
    let _ = writeln!(md, "| Tested | {tested} |");
    let _ = writeln!(md, "| Passed | {pass_count} |");
    let _ = writeln!(md, "| Failed | {fail_count} |");
    let _ = writeln!(md, "| Skipped | {skip_count} |");
    let _ = writeln!(md, "| Pass rate | {pass_rate:.1}% |");
    md.push('\n');

    md.push_str("## By Journey Category\n\n");
    md.push_str("| Category | Pass | Fail | Skip |\n|----------|------|------|------|\n");
    for (cat, (p, f, s)) in &by_category {
        let _ = writeln!(md, "| {cat} | {p} | {f} | {s} |");
    }
    md.push('\n');

    if fail_count > 0 {
        md.push_str("## Journey Failures\n\n");
        for r in results.iter().filter(|r| r.status == "fail") {
            let _ = writeln!(md, "### {} (tier {})\n", r.extension_id, r.extension_tier);
            let _ = writeln!(md, "- **Category:** {:?}", r.journey_category);
            let _ = writeln!(md, "- **Journey:** {}", r.journey_description);
            let _ = writeln!(
                md,
                "- **Failed at:** {}",
                r.failure_step.as_deref().unwrap_or("unknown")
            );
            let _ = writeln!(
                md,
                "- **Reason:** {}",
                r.failure_reason.as_deref().unwrap_or("unknown")
            );
            let _ = writeln!(
                md,
                "- **Progress:** {}/{} steps",
                r.steps_completed, r.steps_total
            );
            let _ = writeln!(md, "- **Reproduce:**");
            let _ = writeln!(md, "  ```bash\n  {}\n  ```", r.reproduce_command);
            md.push('\n');
        }
    }

    let md_path = report_dir.join("journey_report.md");
    let _ = std::fs::write(&md_path, &md);

    // ── Print summary ──
    eprintln!("\n=== Journey Summary ===");
    eprintln!("  Tested:   {tested}");
    eprintln!("  Passed:   {pass_count}");
    eprintln!("  Failed:   {fail_count}");
    eprintln!("  Skipped:  {skip_count}");
    eprintln!("  Rate:     {pass_rate:.1}%");
    eprintln!();
    eprintln!("  By category:");
    for (cat, (p, f, s)) in &by_category {
        eprintln!("    {cat}: {p}P / {f}F / {s}S");
    }
    eprintln!();
    eprintln!("  Reports:");
    eprintln!("    JSON:    {}", report_path.display());
    eprintln!("    JSONL:   {}", events_path.display());
    eprintln!("    MD:      {}", md_path.display());
    eprintln!("    Details: {}", details_dir.display());
    eprintln!();
}

// ─── Daily Extension Health & Regression Delta (bd-1f42.4.5) ────────────────
//
// Compares current conformance results against the committed baseline
// (`tests/ext_conformance/reports/conformance_baseline.json`) to detect
// regressions (was passing, now failing), fixes (was failing, now passing),
// and new extensions.  Designed to run daily in CI or on-demand during
// development.
//
// Environment variables:
//   PI_HEALTH_BASELINE_PATH  — override baseline file (default: auto-detected)
//   PI_HEALTH_UPDATE_BASELINE — "true" to write a per-extension snapshot for
//                               future comparisons (default: false)
//   PI_HEALTH_FAIL_ON_REGRESSION — "true" to fail the test on regressions
//                                  (default: false)
//
// Run:
//   `cargo test --test ext_conformance_generated --features ext-conformance \
//     -- conformance_health_delta --nocapture`

/// Per-extension status for delta comparison.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ExtHealthStatus {
    id: String,
    tier: u32,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_reason: Option<String>,
}

/// Delta classification for a single extension.
#[derive(Debug, Clone, serde::Serialize)]
struct ExtDelta {
    id: String,
    tier: u32,
    delta_type: String, // "regression", "fix", "new_failure", "new_pass", "unchanged_pass", "unchanged_fail", "removed"
    current_status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    baseline_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    baseline_failure_reason: Option<String>,
}

/// Intentional exclusions from release-facing full-manifest health claims.
#[derive(Debug, Clone, serde::Serialize)]
struct ExtHealthExclusion {
    id: String,
    tier: u32,
    reason: String,
}

/// Full health delta report.
#[derive(Debug, serde::Serialize)]
struct HealthDeltaReport {
    schema: String,
    generated_at: String,
    baseline_path: String,
    baseline_date: String,
    current_summary: serde_json::Value,
    baseline_summary: serde_json::Value,
    aggregate_delta: serde_json::Value,
    regressions: Vec<ExtDelta>,
    fixes: Vec<ExtDelta>,
    new_extensions: Vec<ExtDelta>,
    removed_extensions: Vec<ExtDelta>,
    unchanged_failures: Vec<ExtDelta>,
    excluded_extensions: Vec<ExtHealthExclusion>,
    total_regressions: usize,
    total_fixes: usize,
    net_change: i64,
}

fn health_delta_exclusion_reason(ext_id: &str) -> Option<&'static str> {
    match ext_id {
        "base_fixtures" => Some(
            "test-only fixture cluster with intentional negative entries; covered by shape tests, excluded from release-facing extension pass-rate claims",
        ),
        _ => None,
    }
}

/// Daily extension health and regression delta test.
///
/// Run with:
/// `cargo test --test ext_conformance_generated --features ext-conformance -- conformance_health_delta --nocapture`
#[test]
#[allow(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap
)]
fn conformance_health_delta() {
    use chrono::{SecondsFormat, Utc};
    use std::collections::HashMap;
    use std::fmt::Write as _;

    let manifest = load_manifest();

    let report_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("ext_conformance")
        .join("reports")
        .join("health_delta");
    let _ = std::fs::create_dir_all(&report_dir);

    // ── Load baseline ──
    let baseline_path = std::env::var("PI_HEALTH_BASELINE_PATH").ok().map_or_else(
        || {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join("ext_conformance")
                .join("reports")
                .join("conformance_baseline.json")
        },
        PathBuf::from,
    );

    let fail_on_regression =
        std::env::var("PI_HEALTH_FAIL_ON_REGRESSION").is_ok_and(|v| v == "true" || v == "1");
    let update_baseline =
        std::env::var("PI_HEALTH_UPDATE_BASELINE").is_ok_and(|v| v == "true" || v == "1");

    let baseline_json: serde_json::Value = if baseline_path.exists() {
        let data = std::fs::read_to_string(&baseline_path).expect("Failed to read baseline");
        serde_json::from_str(&data).expect("Failed to parse baseline JSON")
    } else {
        eprintln!(
            "  WARN: No baseline file found at {}",
            baseline_path.display()
        );
        serde_json::json!({})
    };

    let baseline_date = baseline_json
        .get("generated_at")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    // Build per-extension map from baseline failure classification.
    let mut baseline_status_map: HashMap<String, ExtHealthStatus> = HashMap::new();
    if let Some(fc) = baseline_json.get("failure_classification") {
        // All extensions listed under any failure category are "fail".
        for (_category, info) in fc.as_object().into_iter().flatten() {
            if let Some(exts) = info.get("extensions").and_then(|v| v.as_array()) {
                for ext in exts {
                    if let Some(id) = ext.as_str() {
                        baseline_status_map.insert(
                            id.to_string(),
                            ExtHealthStatus {
                                id: id.to_string(),
                                tier: 0, // tier not in baseline
                                status: "fail".to_string(),
                                failure_reason: info
                                    .get("description")
                                    .and_then(|v| v.as_str())
                                    .map(String::from),
                            },
                        );
                    }
                }
            }
        }
    }

    // Check for per-extension baseline snapshot (from previous health delta run).
    let per_ext_baseline_path = report_dir.join("per_extension_baseline.json");
    if per_ext_baseline_path.exists() {
        if let Ok(data) = std::fs::read_to_string(&per_ext_baseline_path) {
            if let Ok(statuses) = serde_json::from_str::<Vec<ExtHealthStatus>>(&data) {
                // More precise per-extension data overrides the coarse baseline.
                for s in statuses {
                    baseline_status_map.insert(s.id.clone(), s);
                }
            }
        }
    }

    // Baseline aggregate stats.
    let bl_ext = &baseline_json["extension_conformance"];
    let bl_passed = bl_ext["passed"].as_u64().unwrap_or(0);
    let bl_failed = bl_ext["failed"].as_u64().unwrap_or(0);
    let bl_tested = bl_ext["tested"].as_u64().unwrap_or(0);
    let bl_rate = bl_ext["pass_rate_pct"].as_f64().unwrap_or(0.0);

    eprintln!("\n=== Extension Health & Regression Delta (bd-1f42.4.5) ===");
    eprintln!("  Baseline:    {}", baseline_path.display());
    eprintln!("  Baseline at: {baseline_date}");
    eprintln!("  Baseline:    {bl_passed}/{bl_tested} passed ({bl_rate:.1}%)");
    eprintln!(
        "  Extensions in current manifest: {}",
        manifest.extensions.len()
    );
    eprintln!();

    // ── Run current conformance ──
    eprintln!("  Running current conformance...");
    let mut current_statuses: Vec<ExtHealthStatus> = Vec::with_capacity(manifest.extensions.len());
    let mut cur_pass = 0_usize;
    let mut cur_fail = 0_usize;
    let mut cur_skip = 0_usize;
    let mut excluded_extensions: Vec<ExtHealthExclusion> = Vec::new();

    for (idx, entry) in manifest.extensions.iter().enumerate() {
        if let Some(reason) = health_delta_exclusion_reason(&entry.id) {
            cur_skip += 1;
            eprintln!(
                "  [{:>3}/{}] {:<50} EXCLUDE ({reason})",
                idx + 1,
                manifest.extensions.len(),
                entry.id
            );
            current_statuses.push(ExtHealthStatus {
                id: entry.id.clone(),
                tier: entry.conformance_tier,
                status: "skip".to_string(),
                failure_reason: Some(reason.to_string()),
            });
            excluded_extensions.push(ExtHealthExclusion {
                id: entry.id.clone(),
                tier: entry.conformance_tier,
                reason: reason.to_string(),
            });
            continue;
        }

        let entry_file = artifacts_dir().join(&entry.entry_path);
        if !entry_file.exists() {
            cur_skip += 1;
            current_statuses.push(ExtHealthStatus {
                id: entry.id.clone(),
                tier: entry.conformance_tier,
                status: "skip".to_string(),
                failure_reason: Some("Artifact not available".to_string()),
            });
            continue;
        }
        eprint!(
            "  [{:>3}/{}] {:<50} ",
            idx + 1,
            manifest.extensions.len(),
            entry.id
        );
        let result = try_conformance(&entry.id);
        match result.status.as_str() {
            "pass" => {
                eprintln!("PASS   ({}ms)", result.duration_ms);
                cur_pass += 1;
            }
            "fail" => {
                eprintln!("FAIL   ({}ms)", result.duration_ms);
                cur_fail += 1;
            }
            _ => {
                eprintln!("SKIP");
                cur_skip += 1;
            }
        }
        current_statuses.push(ExtHealthStatus {
            id: entry.id.clone(),
            tier: entry.conformance_tier,
            status: result.status,
            failure_reason: result.failure_reason,
        });
    }

    let cur_tested = cur_pass + cur_fail;
    let cur_rate = if cur_tested > 0 {
        (cur_pass as f64) / (cur_tested as f64) * 100.0
    } else {
        0.0
    };

    // ── Compute deltas ──
    let current_map: HashMap<String, &ExtHealthStatus> =
        current_statuses.iter().map(|s| (s.id.clone(), s)).collect();

    let mut regressions: Vec<ExtDelta> = Vec::new();
    let mut fixes: Vec<ExtDelta> = Vec::new();
    let mut new_extensions: Vec<ExtDelta> = Vec::new();
    let mut unchanged_failures: Vec<ExtDelta> = Vec::new();

    for s in &current_statuses {
        if s.status == "skip" {
            continue;
        }

        if let Some(bl) = baseline_status_map.get(&s.id) {
            if bl.status == "pass" && s.status == "fail" {
                regressions.push(ExtDelta {
                    id: s.id.clone(),
                    tier: s.tier,
                    delta_type: "regression".to_string(),
                    current_status: s.status.clone(),
                    baseline_status: Some(bl.status.clone()),
                    failure_reason: s.failure_reason.clone(),
                    baseline_failure_reason: None,
                });
            } else if bl.status == "fail" && s.status == "pass" {
                fixes.push(ExtDelta {
                    id: s.id.clone(),
                    tier: s.tier,
                    delta_type: "fix".to_string(),
                    current_status: s.status.clone(),
                    baseline_status: Some(bl.status.clone()),
                    failure_reason: None,
                    baseline_failure_reason: bl.failure_reason.clone(),
                });
            } else if bl.status == "fail" && s.status == "fail" {
                unchanged_failures.push(ExtDelta {
                    id: s.id.clone(),
                    tier: s.tier,
                    delta_type: "unchanged_fail".to_string(),
                    current_status: s.status.clone(),
                    baseline_status: Some(bl.status.clone()),
                    failure_reason: s.failure_reason.clone(),
                    baseline_failure_reason: bl.failure_reason.clone(),
                });
            }
            // unchanged pass: nothing to report
        } else {
            // Extension not in baseline = new
            let dt = if s.status == "pass" {
                "new_pass"
            } else {
                "new_failure"
            };
            new_extensions.push(ExtDelta {
                id: s.id.clone(),
                tier: s.tier,
                delta_type: dt.to_string(),
                current_status: s.status.clone(),
                baseline_status: None,
                failure_reason: s.failure_reason.clone(),
                baseline_failure_reason: None,
            });
        }
    }

    // Extensions in baseline but not in current manifest.
    let removed_extensions: Vec<ExtDelta> = baseline_status_map
        .keys()
        .filter(|id| !current_map.contains_key(*id))
        .map(|id| {
            let bl = &baseline_status_map[id];
            ExtDelta {
                id: id.clone(),
                tier: bl.tier,
                delta_type: "removed".to_string(),
                current_status: "absent".to_string(),
                baseline_status: Some(bl.status.clone()),
                failure_reason: None,
                baseline_failure_reason: bl.failure_reason.clone(),
            }
        })
        .collect();

    let total_regressions = regressions.len();
    let total_fixes = fixes.len();
    let net_change = (total_fixes as i64) - (total_regressions as i64);

    // ── Build report ──
    let report = HealthDeltaReport {
        schema: "pi.ext.health_delta.v1".to_string(),
        generated_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        baseline_path: baseline_path.display().to_string(),
        baseline_date: baseline_date.clone(),
        current_summary: serde_json::json!({
            "manifest_count": manifest.extensions.len(),
            "tested": cur_tested,
            "passed": cur_pass,
            "failed": cur_fail,
            "skipped": cur_skip,
            "excluded": excluded_extensions.len(),
            "pass_rate_pct": cur_rate,
        }),
        baseline_summary: serde_json::json!({
            "tested": bl_tested,
            "passed": bl_passed,
            "failed": bl_failed,
            "pass_rate_pct": bl_rate,
        }),
        aggregate_delta: serde_json::json!({
            "pass_delta": (cur_pass as i64) - (bl_passed as i64),
            "fail_delta": (cur_fail as i64) - (bl_failed as i64),
            "rate_delta_pct": cur_rate - bl_rate,
            "net_change": net_change,
        }),
        regressions: regressions.clone(),
        fixes: fixes.clone(),
        new_extensions: new_extensions.clone(),
        removed_extensions: removed_extensions.clone(),
        unchanged_failures: unchanged_failures.clone(),
        excluded_extensions: excluded_extensions.clone(),
        total_regressions,
        total_fixes,
        net_change,
    };

    // ── Write JSON report ──
    let report_path = report_dir.join("health_delta_report.json");
    let _ = std::fs::write(
        &report_path,
        serde_json::to_string_pretty(&report).unwrap_or_default(),
    );

    // ── Write JSONL events ──
    let events_path = report_dir.join("health_delta_events.jsonl");
    let mut event_lines: Vec<String> = Vec::new();
    let all_deltas: Vec<&ExtDelta> = regressions
        .iter()
        .chain(fixes.iter())
        .chain(new_extensions.iter())
        .chain(removed_extensions.iter())
        .chain(unchanged_failures.iter())
        .collect();
    for d in &all_deltas {
        let line = serde_json::json!({
            "schema": "pi.ext.health_delta_event.v1",
            "id": d.id,
            "tier": d.tier,
            "delta_type": d.delta_type,
            "current_status": d.current_status,
            "baseline_status": d.baseline_status,
            "failure_reason": d.failure_reason,
            "ts": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        });
        event_lines.push(serde_json::to_string(&line).unwrap_or_default());
    }
    let _ = std::fs::write(&events_path, event_lines.join("\n") + "\n");

    // ── Write per-extension snapshot for future baseline ──
    if update_baseline {
        let _ = std::fs::write(
            &per_ext_baseline_path,
            serde_json::to_string_pretty(&current_statuses).unwrap_or_default(),
        );
        eprintln!(
            "  Updated per-extension baseline: {}",
            per_ext_baseline_path.display()
        );
    }

    // ── Write Markdown report ──
    let mut md = String::new();
    md.push_str("# Extension Health & Regression Delta Report\n\n");
    let _ = writeln!(
        md,
        "> Generated: {}",
        Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
    );
    let _ = writeln!(md, "> Baseline: {baseline_date}\n");

    md.push_str("## Aggregate Comparison\n\n");
    md.push_str(
        "| Metric | Baseline | Current | Delta |\n|--------|----------|---------|-------|\n",
    );
    let _ = writeln!(
        md,
        "| Tested | {bl_tested} | {cur_tested} | {:+} |",
        (cur_tested as i64) - (bl_tested as i64)
    );
    let _ = writeln!(
        md,
        "| Passed | {bl_passed} | {cur_pass} | {:+} |",
        (cur_pass as i64) - (bl_passed as i64)
    );
    let _ = writeln!(
        md,
        "| Failed | {bl_failed} | {cur_fail} | {:+} |",
        (cur_fail as i64) - (bl_failed as i64)
    );
    let _ = writeln!(
        md,
        "| Pass rate | {bl_rate:.1}% | {cur_rate:.1}% | {:+.1}pp |",
        cur_rate - bl_rate
    );
    md.push('\n');

    md.push_str("## Delta Summary\n\n");
    md.push_str("| Category | Count |\n|----------|-------|\n");
    let _ = writeln!(md, "| Regressions | {} |", regressions.len());
    let _ = writeln!(md, "| Fixes | {} |", fixes.len());
    let _ = writeln!(md, "| New extensions | {} |", new_extensions.len());
    let _ = writeln!(md, "| Removed | {} |", removed_extensions.len());
    let _ = writeln!(md, "| Unchanged failures | {} |", unchanged_failures.len());
    let _ = writeln!(md, "| Excluded fixtures | {} |", excluded_extensions.len());
    let _ = writeln!(md, "| **Net change** | **{net_change:+}** |");
    md.push('\n');

    if !excluded_extensions.is_empty() {
        md.push_str("## Excluded Test Fixtures\n\n");
        md.push_str("| Extension | Tier | Reason |\n|-----------|------|--------|\n");
        for excluded in &excluded_extensions {
            let _ = writeln!(
                md,
                "| {} | {} | {} |",
                excluded.id, excluded.tier, excluded.reason
            );
        }
        md.push('\n');
    }

    if !regressions.is_empty() {
        md.push_str("## Regressions (was passing, now failing)\n\n");
        md.push_str("| Extension | Tier | Reason |\n|-----------|------|--------|\n");
        for d in &regressions {
            let _ = writeln!(
                md,
                "| {} | {} | {} |",
                d.id,
                d.tier,
                d.failure_reason.as_deref().unwrap_or("unknown")
            );
        }
        md.push('\n');
    }

    if !fixes.is_empty() {
        md.push_str("## Fixes (was failing, now passing)\n\n");
        md.push_str(
            "| Extension | Tier | Previous Reason |\n|-----------|------|-----------------|\n",
        );
        for d in &fixes {
            let _ = writeln!(
                md,
                "| {} | {} | {} |",
                d.id,
                d.tier,
                d.baseline_failure_reason.as_deref().unwrap_or("unknown")
            );
        }
        md.push('\n');
    }

    let md_path = report_dir.join("health_delta_report.md");
    let _ = std::fs::write(&md_path, &md);

    // ── Print summary ──
    eprintln!("\n=== Health Delta Summary ===");
    eprintln!("  Current:     {cur_pass}/{cur_tested} passed ({cur_rate:.1}%)");
    eprintln!("  Baseline:    {bl_passed}/{bl_tested} passed ({bl_rate:.1}%)");
    eprintln!("  Regressions: {total_regressions}");
    eprintln!("  Fixes:       {total_fixes}");
    eprintln!("  Net change:  {net_change:+}");
    if !new_extensions.is_empty() {
        eprintln!("  New exts:    {}", new_extensions.len());
    }
    if !removed_extensions.is_empty() {
        eprintln!("  Removed:     {}", removed_extensions.len());
    }
    eprintln!();
    eprintln!("  Reports:");
    eprintln!("    JSON:  {}", report_path.display());
    eprintln!("    JSONL: {}", events_path.display());
    eprintln!("    MD:    {}", md_path.display());
    eprintln!();

    // ── Regression gate ──
    if fail_on_regression && total_regressions > 0 {
        let mut msg =
            format!("HEALTH DELTA BLOCKED: {total_regressions} regression(s) detected.\n");
        for d in &regressions {
            let _ = writeln!(
                msg,
                "  - {} (tier {}): {}",
                d.id,
                d.tier,
                d.failure_reason.as_deref().unwrap_or("unknown")
            );
        }
        let _ = writeln!(msg, "\nSee: {}", report_path.display());
        panic!("{msg}");
    }
}

// ─── Macro ──────────────────────────────────────────────────────────────────

/// Generate a conformance test for a single extension.
macro_rules! conformance_test {
    ($name:ident, $ext_id:literal $(,)?) => {
        #[test]
        fn $name() {
            run_conformance_test($ext_id);
        }
    };
    ($name:ident, $ext_id:literal, ignore $(,)?) => {
        #[test]
        #[ignore = "bd-8t27h.17: tracked stretch conformance tier; run with --include-ignored"]
        fn $name() {
            run_conformance_test($ext_id);
        }
    };
}

#[test]
fn ignored_generated_extension_tests_are_tracked_opt_in_lane() {
    let source = include_str!("ext_conformance_generated.rs");
    let old_reason = [
        "unvendored",
        "extension",
        "not",
        "yet",
        "onboarded",
        "into",
        "conformance",
        "corpus",
    ]
    .join(" ");

    assert!(
        !source.contains(&old_reason),
        "generated extension conformance ignores must be owner-tracked opt-in lanes"
    );
    let ignore_attrs: Vec<&str> = source
        .lines()
        .filter(|line| line.trim_start().starts_with("#[ignore"))
        .collect();
    assert!(
        !ignore_attrs.is_empty(),
        "expected generated extension conformance to keep an explicit opt-in tier"
    );
    for attr in ignore_attrs {
        assert!(
            attr.contains("bd-8t27h.17:")
                && attr.contains("tracked stretch conformance tier")
                && attr.contains("--include-ignored"),
            "generated extension conformance ignores need an owner bead reason and opt-in command: {attr}"
        );
    }
}

// ─── Tier 1 — Single-file, no events, no deps (37 extensions) ──────────────

conformance_test!(ext_antigravity_image_gen, "antigravity-image-gen");
conformance_test!(ext_bash_spawn_hook, "bash-spawn-hook");
conformance_test!(ext_bookmark, "bookmark");
conformance_test!(
    ext_community_hjanuschka_clipboard,
    "community/hjanuschka-clipboard"
);
conformance_test!(
    ext_community_hjanuschka_cost_tracker,
    "community/hjanuschka-cost-tracker"
);
conformance_test!(
    ext_community_hjanuschka_flicker_corp,
    "community/hjanuschka-flicker-corp"
);
conformance_test!(
    ext_community_hjanuschka_handoff,
    "community/hjanuschka-handoff"
);
conformance_test!(
    ext_community_hjanuschka_memory_mode,
    "community/hjanuschka-memory-mode"
);
conformance_test!(
    ext_community_hjanuschka_oracle,
    "community/hjanuschka-oracle"
);
conformance_test!(ext_community_mitsuhiko_answer, "community/mitsuhiko-answer");
conformance_test!(
    ext_community_tmustier_arcade_mario_not,
    "community/tmustier-arcade-mario-not"
);
conformance_test!(
    ext_community_tmustier_arcade_picman,
    "community/tmustier-arcade-picman"
);
conformance_test!(
    ext_community_tmustier_arcade_ping,
    "community/tmustier-arcade-ping"
);
conformance_test!(
    ext_community_tmustier_arcade_spice_invaders,
    "community/tmustier-arcade-spice-invaders"
);
conformance_test!(
    ext_community_tmustier_arcade_tetris,
    "community/tmustier-arcade-tetris"
);
conformance_test!(
    ext_community_tmustier_tab_status,
    "community/tmustier-tab-status"
);
conformance_test!(
    ext_community_tmustier_usage_extension,
    "community/tmustier-usage-extension"
);
conformance_test!(ext_custom_footer, "custom-footer");
conformance_test!(ext_handoff, "handoff");
conformance_test!(ext_hello, "hello");
conformance_test!(ext_message_renderer, "message-renderer");
conformance_test!(
    ext_npm_benvargas_pi_ancestor_discovery,
    "npm/benvargas-pi-ancestor-discovery"
);
conformance_test!(
    ext_npm_benvargas_pi_antigravity_image_gen,
    "npm/benvargas-pi-antigravity-image-gen"
);
conformance_test!(ext_npm_pi_command_center, "npm/pi-command-center");
conformance_test!(ext_npm_pi_model_switch, "npm/pi-model-switch");
conformance_test!(ext_npm_pi_threads, "npm/pi-threads");
conformance_test!(ext_overlay_test, "overlay-test");
conformance_test!(ext_qna, "qna");
conformance_test!(ext_question, "question");
conformance_test!(ext_questionnaire, "questionnaire");
conformance_test!(ext_send_user_message, "send-user-message");
conformance_test!(ext_session_name, "session-name");
conformance_test!(ext_snake, "snake");
conformance_test!(ext_space_invaders, "space-invaders");
conformance_test!(ext_summarize, "summarize");
conformance_test!(
    ext_third_party_rytswd_questionnaire,
    "third-party/rytswd-questionnaire"
);
conformance_test!(ext_redraws, "redraws");
conformance_test!(ext_timed_confirm, "timed-confirm");

// ─── Tier 2 — Single-file with events (83 extensions) ──────────────────────

conformance_test!(ext_auto_commit_on_exit, "auto-commit-on-exit");
conformance_test!(ext_claude_rules, "claude-rules");
conformance_test!(ext_diff, "diff");
conformance_test!(ext_files, "files");
conformance_test!(
    ext_community_ferologics_notify,
    "community/ferologics-notify"
);
conformance_test!(
    ext_community_hjanuschka_funny_working_message,
    "community/hjanuschka-funny-working-message"
);
conformance_test!(ext_community_hjanuschka_loop, "community/hjanuschka-loop");
conformance_test!(
    ext_community_hjanuschka_plan_mode,
    "community/hjanuschka-plan-mode"
);
conformance_test!(
    ext_community_hjanuschka_resistance,
    "community/hjanuschka-resistance"
);
conformance_test!(
    ext_community_hjanuschka_speedreading,
    "community/hjanuschka-speedreading"
);
conformance_test!(
    ext_community_hjanuschka_status_widget,
    "community/hjanuschka-status-widget"
);
conformance_test!(
    ext_community_hjanuschka_ultrathink,
    "community/hjanuschka-ultrathink"
);
conformance_test!(
    ext_community_hjanuschka_usage_bar,
    "community/hjanuschka-usage-bar"
);
conformance_test!(
    ext_community_mitsuhiko_cwd_history,
    "community/mitsuhiko-cwd-history"
);
conformance_test!(ext_community_mitsuhiko_files, "community/mitsuhiko-files");
conformance_test!(ext_community_mitsuhiko_loop, "community/mitsuhiko-loop");
conformance_test!(ext_community_mitsuhiko_notify, "community/mitsuhiko-notify");
conformance_test!(ext_community_mitsuhiko_review, "community/mitsuhiko-review");
conformance_test!(ext_community_mitsuhiko_todos, "community/mitsuhiko-todos");
conformance_test!(ext_community_mitsuhiko_uv, "community/mitsuhiko-uv");
conformance_test!(
    ext_community_mitsuhiko_whimsical,
    "community/mitsuhiko-whimsical"
);
conformance_test!(
    ext_community_nicobailon_rewind_hook,
    "community/nicobailon-rewind-hook"
);
conformance_test!(
    ext_community_ogulcancelik_ghostty_theme_sync,
    "community/ogulcancelik-ghostty-theme-sync"
);
conformance_test!(
    ext_community_prateekmedia_token_rate,
    "community/prateekmedia-token-rate"
);
conformance_test!(
    ext_community_qualisero_background_notify,
    "community/qualisero-background-notify",
    ignore
);
conformance_test!(
    ext_community_qualisero_compact_config,
    "community/qualisero-compact-config"
);
conformance_test!(
    ext_community_qualisero_safe_git,
    "community/qualisero-safe-git",
    ignore
);
conformance_test!(
    ext_community_qualisero_safe_rm,
    "community/qualisero-safe-rm"
);
conformance_test!(
    ext_community_qualisero_session_color,
    "community/qualisero-session-color"
);
conformance_test!(
    ext_community_qualisero_session_emoji,
    "community/qualisero-session-emoji"
);
conformance_test!(
    ext_community_tmustier_agent_guidance,
    "community/tmustier-agent-guidance"
);
conformance_test!(
    ext_community_tmustier_ralph_wiggum,
    "community/tmustier-ralph-wiggum"
);
conformance_test!(
    ext_community_tmustier_raw_paste,
    "community/tmustier-raw-paste"
);
conformance_test!(ext_negative_denied_caps, "negative-denied-caps");
conformance_test!(ext_confirm_destructive, "confirm-destructive");
conformance_test!(ext_custom_compaction, "custom-compaction");
conformance_test!(ext_custom_header, "custom-header");
conformance_test!(ext_dirty_repo_guard, "dirty-repo-guard");
conformance_test!(ext_dynamic_resources, "dynamic-resources");
conformance_test!(ext_event_bus, "event-bus");
conformance_test!(ext_file_trigger, "file-trigger");
conformance_test!(ext_git_checkpoint, "git-checkpoint");
conformance_test!(ext_inline_bash, "inline-bash");
conformance_test!(ext_input_transform, "input-transform");
conformance_test!(ext_interactive_shell, "interactive-shell");
conformance_test!(ext_mac_system_theme, "mac-system-theme");
conformance_test!(ext_modal_editor, "modal-editor");
conformance_test!(ext_model_status, "model-status");
conformance_test!(ext_notify, "notify");
conformance_test!(ext_npm_ogulcancelik_pi_sketch, "npm/ogulcancelik-pi-sketch");
conformance_test!(ext_npm_pi_ephemeral, "npm/pi-ephemeral");
conformance_test!(ext_npm_pi_ghostty_theme_sync, "npm/pi-ghostty-theme-sync");
conformance_test!(ext_npm_pi_md_export, "npm/pi-md-export");
conformance_test!(ext_npm_pi_notify, "npm/pi-notify");
conformance_test!(ext_npm_pi_poly_notify, "npm/pi-poly-notify");
conformance_test!(
    ext_npm_pi_prompt_template_model,
    "npm/pi-prompt-template-model"
);
conformance_test!(ext_npm_pi_session_ask, "npm/pi-session-ask");
conformance_test!(ext_npm_pi_skill_palette, "npm/pi-skill-palette");
conformance_test!(ext_npm_pi_voice_of_god, "npm/pi-voice-of-god");
conformance_test!(ext_npm_token_rate_pi, "npm/token-rate-pi");
conformance_test!(ext_npm_vpellegrino_pi_skills, "npm/vpellegrino-pi-skills");
conformance_test!(ext_overlay_qa_tests, "overlay-qa-tests");
conformance_test!(ext_permission_gate, "permission-gate");
conformance_test!(ext_pirate, "pirate");
conformance_test!(ext_preset, "preset");
conformance_test!(ext_prompt_url_widget, "prompt-url-widget");
conformance_test!(ext_protected_paths, "protected-paths");
conformance_test!(ext_rainbow_editor, "rainbow-editor");
conformance_test!(ext_rpc_demo, "rpc-demo");
conformance_test!(ext_shutdown_command, "shutdown-command");
conformance_test!(ext_ssh, "ssh");
conformance_test!(ext_status_line, "status-line");
conformance_test!(ext_system_prompt_header, "system-prompt-header");
conformance_test!(
    ext_third_party_graffioh_pi_screenshots_picker,
    "third-party/graffioh-pi-screenshots-picker"
);
conformance_test!(
    ext_third_party_graffioh_pi_super_curl,
    "third-party/graffioh-pi-super-curl"
);
conformance_test!(
    ext_third_party_jyaunches_pi_canvas,
    "third-party/jyaunches-pi-canvas"
);
conformance_test!(
    ext_third_party_lsj5031_pi_notification_extension,
    "third-party/lsj5031-pi-notification-extension"
);
conformance_test!(
    ext_third_party_ogulcancelik_pi_sketch,
    "third-party/ogulcancelik-pi-sketch"
);
conformance_test!(
    ext_third_party_raunovillberg_pi_stuffed,
    "third-party/raunovillberg-pi-stuffed"
);
conformance_test!(ext_third_party_rytswd_direnv, "third-party/rytswd-direnv");
conformance_test!(ext_titlebar_spinner, "titlebar-spinner");
conformance_test!(ext_todo, "todo");
conformance_test!(ext_tool_override, "tool-override");
conformance_test!(ext_tools, "tools");
conformance_test!(ext_trigger_compact, "trigger-compact");
conformance_test!(ext_truncated_tool, "truncated-tool");
conformance_test!(ext_widget_placement, "widget-placement");

// ─── Tier 3 — Multi-file / npm deps (79 extensions) ────────────────────────

conformance_test!(ext_community_jyaunches_canvas, "community/jyaunches-canvas");
conformance_test!(
    ext_community_nicobailon_interactive_shell,
    "community/nicobailon-interactive-shell",
    ignore
);
conformance_test!(
    ext_community_nicobailon_mcp_adapter,
    "community/nicobailon-mcp-adapter"
);
conformance_test!(
    ext_community_nicobailon_powerline_footer,
    "community/nicobailon-powerline-footer"
);
conformance_test!(
    ext_community_nicobailon_subagents,
    "community/nicobailon-subagents"
);
conformance_test!(
    ext_community_prateekmedia_checkpoint,
    "community/prateekmedia-checkpoint"
);
conformance_test!(ext_community_prateekmedia_lsp, "community/prateekmedia-lsp",);
conformance_test!(
    ext_community_prateekmedia_permission,
    "community/prateekmedia-permission"
);
conformance_test!(
    ext_community_prateekmedia_ralph_loop,
    "community/prateekmedia-ralph-loop"
);
conformance_test!(
    ext_community_prateekmedia_repeat,
    "community/prateekmedia-repeat"
);
conformance_test!(
    ext_community_qualisero_pi_agent_scip,
    "community/qualisero-pi-agent-scip",
    ignore
);
conformance_test!(
    ext_community_tmustier_code_actions,
    "community/tmustier-code-actions"
);
conformance_test!(
    ext_community_tmustier_files_widget,
    "community/tmustier-files-widget"
);
conformance_test!(ext_custom_provider_anthropic, "custom-provider-anthropic");
conformance_test!(ext_custom_provider_gitlab_duo, "custom-provider-gitlab-duo");
conformance_test!(ext_custom_provider_qwen_cli, "custom-provider-qwen-cli");
conformance_test!(ext_doom_overlay, "doom-overlay");
conformance_test!(ext_npm_aliou_pi_extension_dev, "npm/aliou-pi-extension-dev");
conformance_test!(
    ext_npm_aliou_pi_guardrails,
    "npm/aliou-pi-guardrails",
    ignore
);
conformance_test!(ext_npm_aliou_pi_linkup, "npm/aliou-pi-linkup");
conformance_test!(ext_npm_aliou_pi_processes, "npm/aliou-pi-processes", ignore);
conformance_test!(ext_npm_aliou_pi_synthetic, "npm/aliou-pi-synthetic");
conformance_test!(ext_npm_aliou_pi_toolchain, "npm/aliou-pi-toolchain", ignore);
conformance_test!(
    ext_npm_benvargas_pi_synthetic_provider,
    "npm/benvargas-pi-synthetic-provider"
);
conformance_test!(ext_npm_checkpoint_pi, "npm/checkpoint-pi");
conformance_test!(
    ext_npm_imsus_pi_extension_minimax_coding_plan_mcp,
    "npm/imsus-pi-extension-minimax-coding-plan-mcp"
);
conformance_test!(
    ext_npm_juanibiapina_pi_extension_settings,
    "npm/juanibiapina-pi-extension-settings"
);
conformance_test!(
    ext_npm_juanibiapina_pi_files,
    "npm/juanibiapina-pi-files",
    ignore
);
conformance_test!(ext_npm_juanibiapina_pi_gob, "npm/juanibiapina-pi-gob");
conformance_test!(ext_npm_lsp_pi, "npm/lsp-pi");
conformance_test!(
    ext_npm_marckrenn_pi_sub_bar,
    "npm/marckrenn-pi-sub-bar",
    ignore
);
conformance_test!(
    ext_npm_marckrenn_pi_sub_core,
    "npm/marckrenn-pi-sub-core",
    ignore
);
conformance_test!(ext_npm_permission_pi, "npm/permission-pi");
conformance_test!(ext_npm_pi_agentic_compaction, "npm/pi-agentic-compaction");
conformance_test!(ext_npm_pi_amplike, "npm/pi-amplike");
conformance_test!(ext_npm_pi_bash_confirm, "npm/pi-bash-confirm");
conformance_test!(ext_npm_pi_brave_search, "npm/pi-brave-search", ignore);
conformance_test!(ext_npm_pi_mermaid, "npm/pi-mermaid", ignore);
conformance_test!(ext_npm_pi_messenger, "npm/pi-messenger");
conformance_test!(ext_npm_pi_moonshot, "npm/pi-moonshot");
conformance_test!(ext_npm_pi_multicodex, "npm/pi-multicodex");
conformance_test!(ext_npm_pi_repoprompt_mcp, "npm/pi-repoprompt-mcp");
conformance_test!(ext_npm_pi_review_loop, "npm/pi-review-loop");
conformance_test!(ext_npm_pi_screenshots_picker, "npm/pi-screenshots-picker");
conformance_test!(ext_npm_pi_search_agent, "npm/pi-search-agent", ignore);
conformance_test!(ext_npm_pi_shadow_git, "npm/pi-shadow-git");
conformance_test!(ext_npm_pi_shell_completions, "npm/pi-shell-completions");
conformance_test!(ext_npm_pi_subdir_context, "npm/pi-subdir-context");
conformance_test!(ext_npm_pi_super_curl, "npm/pi-super-curl");
conformance_test!(ext_npm_pi_telemetry_otel, "npm/pi-telemetry-otel", ignore);
conformance_test!(ext_npm_pi_wakatime, "npm/pi-wakatime", ignore);
conformance_test!(ext_npm_pi_watch, "npm/pi-watch", ignore);
conformance_test!(ext_npm_pi_web_access, "npm/pi-web-access", ignore);
conformance_test!(ext_npm_ralph_loop_pi, "npm/ralph-loop-pi");
conformance_test!(ext_npm_repeat_pi, "npm/repeat-pi");
conformance_test!(ext_npm_vaayne_agent_kit, "npm/vaayne-agent-kit");
conformance_test!(ext_npm_vaayne_pi_mcp, "npm/vaayne-pi-mcp");
conformance_test!(ext_npm_vaayne_pi_subagent, "npm/vaayne-pi-subagent");
conformance_test!(
    ext_npm_vaayne_pi_web_tools,
    "npm/vaayne-pi-web-tools",
    ignore
);
conformance_test!(ext_npm_walterra_pi_charts, "npm/walterra-pi-charts");
conformance_test!(ext_npm_walterra_pi_graphviz, "npm/walterra-pi-graphviz");
conformance_test!(ext_npm_zenobius_pi_dcp, "npm/zenobius-pi-dcp");
conformance_test!(ext_plan_mode, "plan-mode");
conformance_test!(ext_sandbox, "sandbox");
conformance_test!(ext_subagent, "subagent");
conformance_test!(
    ext_third_party_aliou_pi_extensions,
    "third-party/aliou-pi-extensions",
);
conformance_test!(
    ext_third_party_ben_vargas_pi_packages,
    "third-party/ben-vargas-pi-packages",
);
conformance_test!(
    ext_third_party_charles_cooper_pi_extensions,
    "third-party/charles-cooper-pi-extensions",
);
conformance_test!(
    ext_third_party_cv_pi_ssh_remote,
    "third-party/cv-pi-ssh-remote"
);
conformance_test!(
    ext_third_party_limouren_agent_things,
    "third-party/limouren-agent-things"
);
conformance_test!(
    ext_third_party_lll9p_pi_better_compaction,
    "third-party/lll9p-pi-better-compaction"
);
conformance_test!(
    ext_third_party_marckrenn_pi_sub,
    "third-party/marckrenn-pi-sub",
    ignore
);
conformance_test!(
    ext_third_party_michalvavra_agents,
    "third-party/michalvavra-agents"
);
conformance_test!(
    ext_third_party_openclaw_openclaw,
    "third-party/openclaw-openclaw",
);
conformance_test!(
    ext_third_party_pasky_pi_amplike,
    "third-party/pasky-pi-amplike",
);
conformance_test!(
    ext_third_party_qualisero_pi_agent_scip,
    "third-party/qualisero-pi-agent-scip",
    ignore
);
conformance_test!(
    ext_third_party_rytswd_slow_mode,
    "third-party/rytswd-slow-mode"
);
conformance_test!(
    ext_third_party_w_winter_dot314,
    "third-party/w-winter-dot314",
);
conformance_test!(
    ext_third_party_zenobi_us_pi_dcp,
    "third-party/zenobi-us-pi-dcp"
);
conformance_test!(ext_with_deps, "with-deps");
conformance_test!(ext_base_fixtures, "base_fixtures", ignore);
conformance_test!(ext_npm_oh_my_pi_basics, "npm/oh-my-pi-basics", ignore);
conformance_test!(ext_npm_pi_extensions, "npm/pi-extensions", ignore);
conformance_test!(
    ext_npm_pi_interactive_shell,
    "npm/pi-interactive-shell",
    ignore
);
conformance_test!(ext_npm_pi_mcp_adapter, "npm/pi-mcp-adapter", ignore);
conformance_test!(ext_npm_pi_package_test, "npm/pi-package-test");
conformance_test!(
    ext_npm_pi_powerline_footer,
    "npm/pi-powerline-footer",
    ignore
);
conformance_test!(
    ext_npm_qualisero_pi_agent_scip,
    "npm/qualisero-pi-agent-scip"
);
conformance_test!(ext_npm_shitty_extensions, "npm/shitty-extensions", ignore);
conformance_test!(ext_npm_tmustier_pi_arcade, "npm/tmustier-pi-arcade", ignore);
conformance_test!(
    ext_npm_verioussmith_pi_openrouter,
    "npm/verioussmith-pi-openrouter",
    ignore
);

// ─── Tier 4 — UI-heavy extensions (2 extensions) ───────────────────────────

conformance_test!(
    ext_community_nicobailon_interview_tool,
    "community/nicobailon-interview-tool",
    ignore
);
conformance_test!(ext_npm_pi_interview, "npm/pi-interview", ignore);
conformance_test!(
    ext_third_party_vtemian_pi_config,
    "third-party/vtemian-pi-config"
);

// ─── Tier 5 — Platform-specific (4 extensions) ─────────────────────────────

conformance_test!(
    ext_agents_mikeastock_extensions,
    "agents-mikeastock/extensions",
);
conformance_test!(
    ext_community_mitsuhiko_control,
    "community/mitsuhiko-control"
);
conformance_test!(ext_npm_pi_annotate, "npm/pi-annotate");
conformance_test!(ext_npm_mitsupi, "npm/mitsupi", ignore);
conformance_test!(
    ext_third_party_kcosr_pi_extensions,
    "third-party/kcosr-pi-extensions"
);
