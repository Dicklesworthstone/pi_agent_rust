#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

pub const RELEASE_EVIDENCE_LEDGER_SCHEMA: &str = "pi.release_evidence.ledger.v1";
pub const GENESIS_PREV_HASH: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReleaseEvidenceLedgerEntry {
    pub index: usize,
    pub path: String,
    pub sha256: String,
    pub correlation_id: Option<String>,
    pub source_commit: Option<String>,
    pub owning_bead: String,
    pub schema: String,
    pub prev_hash: String,
    pub entry_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseEvidenceLedgerSummary {
    pub total_artifacts: usize,
    pub verified_intact: usize,
    pub missing_artifacts: usize,
    pub sha_mismatches: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseEvidenceLedgerArtifact {
    pub schema: String,
    pub generated_at: String,
    pub contract_path: String,
    pub head_hash: String,
    pub entry_count: usize,
    pub entries: Vec<ReleaseEvidenceLedgerEntry>,
    pub summary: ReleaseEvidenceLedgerSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationReport {
    pub schema: String,
    pub status: String,
    pub entry_count: usize,
    pub head_hash: String,
    pub errors: Vec<String>,
}

pub fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        let _ = write!(s, "{:02x}", b);
    }
    s
}

pub fn compute_sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_encode(&hasher.finalize())
}

pub fn compute_sha256_file(path: &Path) -> Result<String, Box<dyn Error>> {
    let bytes = fs::read(path)?;
    Ok(compute_sha256_bytes(&bytes))
}

pub fn compute_entry_hash(
    index: usize,
    path: &str,
    sha256: &str,
    correlation_id: Option<&str>,
    source_commit: Option<&str>,
    owning_bead: &str,
    schema: &str,
    prev_hash: &str,
) -> String {
    let payload = format!(
        "{index}:{path}:{sha256}:{}:{}:{owning_bead}:{schema}:{prev_hash}",
        correlation_id.unwrap_or(""),
        source_commit.unwrap_or("")
    );
    compute_sha256_bytes(payload.as_bytes())
}

fn resolve_safe_path(root: &Path, rel: &str) -> Option<PathBuf> {
    if rel.contains("..") || rel.starts_with('/') {
        return None;
    }
    let mut p = root.to_path_buf();
    for seg in rel.split('/') {
        p.push(seg);
    }
    Some(p)
}

enum VerifyIssue<'a> {
    IndexMismatch(usize, usize),
    BrokenChain(usize),
    HashMismatch(usize),
    MissingFile(&'a str),
    ChecksumMismatch(&'a str),
    ChecksumError(&'a str),
    InvalidPath(usize, &'a str),
}

impl std::fmt::Display for VerifyIssue<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IndexMismatch(pos, idx) => write!(f, "entry index mismatch at pos {pos}: {idx}"),
            Self::BrokenChain(idx) => write!(f, "hash chain broken at entry {idx}"),
            Self::HashMismatch(idx) => write!(f, "entry_hash mismatch at entry {idx}"),
            Self::MissingFile(p) => write!(f, "artifact file missing on disk: {p}"),
            Self::ChecksumMismatch(p) => write!(f, "sha256 checksum mismatch for {p}"),
            Self::ChecksumError(p) => write!(f, "failed to read sha256 checksum for {p}"),
            Self::InvalidPath(idx, p) => write!(f, "invalid relative path at entry {idx}: {p}"),
        }
    }
}

pub fn verify_ledger(
    ledger: &ReleaseEvidenceLedgerArtifact,
    repo_root: &Path,
) -> VerificationReport {
    let mut errors = Vec::new();
    let mut raw_issues = Vec::new();

    if ledger.schema != RELEASE_EVIDENCE_LEDGER_SCHEMA {
        errors.push(format!(
            "invalid schema: expected {}, got {}",
            RELEASE_EVIDENCE_LEDGER_SCHEMA, ledger.schema
        ));
    }

    if ledger.entries.len() != ledger.entry_count {
        errors.push(format!(
            "entry count mismatch: declared {}, actual {}",
            ledger.entry_count,
            ledger.entries.len()
        ));
    }

    let mut expected_prev_hash = GENESIS_PREV_HASH;

    for (idx, entry) in ledger.entries.iter().enumerate() {
        if entry.index != idx {
            raw_issues.push(VerifyIssue::IndexMismatch(idx, entry.index));
        }

        if entry.prev_hash != expected_prev_hash {
            raw_issues.push(VerifyIssue::BrokenChain(idx));
        }

        let computed_hash = compute_entry_hash(
            entry.index,
            &entry.path,
            &entry.sha256,
            entry.correlation_id.as_deref(),
            entry.source_commit.as_deref(),
            &entry.owning_bead,
            &entry.schema,
            &entry.prev_hash,
        );

        if computed_hash != entry.entry_hash {
            raw_issues.push(VerifyIssue::HashMismatch(idx));
        }

        match resolve_safe_path(repo_root, &entry.path) {
            Some(file_path) => {
                if !file_path.exists() {
                    raw_issues.push(VerifyIssue::MissingFile(&entry.path));
                } else {
                    match compute_sha256_file(&file_path) {
                        Ok(actual_sha) => {
                            if actual_sha != entry.sha256 {
                                raw_issues.push(VerifyIssue::ChecksumMismatch(&entry.path));
                            }
                        }
                        Err(_) => {
                            raw_issues.push(VerifyIssue::ChecksumError(&entry.path));
                        }
                    }
                }
            }
            None => {
                raw_issues.push(VerifyIssue::InvalidPath(idx, &entry.path));
            }
        }

        expected_prev_hash = &entry.entry_hash;
    }

    errors.extend(raw_issues.into_iter().map(|i| i.to_string()));

    if let Some(last) = ledger.entries.last() {
        if last.entry_hash != ledger.head_hash {
            errors.push(format!(
                "head_hash mismatch: last entry is {}, ledger head_hash is {}",
                last.entry_hash, ledger.head_hash
            ));
        }
    } else if ledger.head_hash != GENESIS_PREV_HASH {
        errors.push(format!(
            "empty ledger must have genesis head_hash, got {}",
            ledger.head_hash
        ));
    }

    let status = if errors.is_empty() {
        "pass".to_string()
    } else {
        "fail".to_string()
    };

    VerificationReport {
        schema: "pi.release_evidence.verification_report.v1".to_string(),
        status,
        entry_count: ledger.entries.len(),
        head_hash: ledger.head_hash.clone(),
        errors,
    }
}

#[test]
fn contract_file_matches_schema_and_policy() -> Result<(), Box<dyn Error>> {
    let contract_path = repo_root().join("docs/contracts/release-evidence-ledger-contract.json");
    assert!(
        contract_path.exists(),
        "release evidence contract file must exist"
    );

    let text = std::fs::read_to_string(&contract_path)?;
    let contract: Value = serde_json::from_str(&text)?;

    assert_eq!(
        contract.get("schema").and_then(Value::as_str),
        Some("pi.release_evidence.ledger.contract.v1"),
        "contract schema must be pi.release_evidence.ledger.contract.v1"
    );
    assert_eq!(
        contract.get("bead_id").and_then(Value::as_str),
        Some("bd-sog97.13"),
        "bead_id must be bd-sog97.13"
    );

    let required_fields = contract
        .get("required_entry_fields")
        .and_then(Value::as_array)
        .ok_or("required_entry_fields must be an array")?;
    let field_names: Vec<&str> = required_fields.iter().filter_map(Value::as_str).collect();
    assert!(field_names.contains(&"index"));
    assert!(field_names.contains(&"path"));
    assert!(field_names.contains(&"sha256"));
    assert!(field_names.contains(&"owning_bead"));
    assert!(field_names.contains(&"schema"));
    assert!(field_names.contains(&"prev_hash"));
    assert!(field_names.contains(&"entry_hash"));
    Ok(())
}

#[test]
fn live_ledger_file_passes_verification() -> Result<(), Box<dyn Error>> {
    let root = repo_root();
    let ledger_path = root.join("docs/evidence/release-evidence-ledger.json");
    assert!(
        ledger_path.exists(),
        "release evidence ledger file must exist"
    );

    let text = std::fs::read_to_string(&ledger_path)?;
    let ledger: ReleaseEvidenceLedgerArtifact = serde_json::from_str(&text)?;

    let report = verify_ledger(&ledger, &root);
    assert_eq!(
        report.status, "pass",
        "ledger verification must pass: {:?}",
        report.errors
    );
    assert!(report.errors.is_empty());
    assert!(report.entry_count > 0, "ledger must contain entries");
    Ok(())
}

#[test]
fn tamper_detection_checksum_mutation() -> Result<(), Box<dyn Error>> {
    let root = repo_root();
    let ledger_path = root.join("docs/evidence/release-evidence-ledger.json");
    let text = std::fs::read_to_string(&ledger_path)?;
    let mut ledger: ReleaseEvidenceLedgerArtifact = serde_json::from_str(&text)?;

    if let Some(first) = ledger.entries.get_mut(0) {
        first.sha256 =
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string();
    }

    let report = verify_ledger(&ledger, &root);
    assert_eq!(report.status, "fail");
    assert!(!report.errors.is_empty());
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.contains("entry_hash mismatch") || e.contains("checksum mismatch"))
    );
    Ok(())
}

#[test]
fn tamper_detection_prev_hash_disruption() -> Result<(), Box<dyn Error>> {
    let root = repo_root();
    let ledger_path = root.join("docs/evidence/release-evidence-ledger.json");
    let text = std::fs::read_to_string(&ledger_path)?;
    let mut ledger: ReleaseEvidenceLedgerArtifact = serde_json::from_str(&text)?;

    if let Some(third) = ledger.entries.get_mut(2) {
        third.prev_hash =
            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".to_string();
    }
    let report = verify_ledger(&ledger, &root);
    assert_eq!(report.status, "fail");
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.contains("hash chain broken"))
    );
    Ok(())
}

#[test]
fn tamper_detection_swapped_entries() -> Result<(), Box<dyn Error>> {
    let root = repo_root();
    let ledger_path = root.join("docs/evidence/release-evidence-ledger.json");
    let text = std::fs::read_to_string(&ledger_path)?;
    let mut ledger: ReleaseEvidenceLedgerArtifact = serde_json::from_str(&text)?;

    if ledger.entries.len() > 3 {
        ledger.entries.swap(1, 2);
        let report = verify_ledger(&ledger, &root);
        assert_eq!(report.status, "fail");
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("entry index mismatch") || e.contains("hash chain broken"))
        );
    }
    Ok(())
}

#[test]
fn bead_cross_references_resolve() -> Result<(), Box<dyn Error>> {
    let root = repo_root();
    let ledger_path = root.join("docs/evidence/release-evidence-ledger.json");
    let text = std::fs::read_to_string(&ledger_path)?;
    let ledger: ReleaseEvidenceLedgerArtifact = serde_json::from_str(&text)?;

    let issues_path = root.join(".beads/issues.jsonl");
    let issues_text = std::fs::read_to_string(&issues_path)?;
    let issues_lines: Vec<&str> = issues_text.lines().collect();
    let mut parsed_issues = Vec::new();
    for line in &issues_lines {
        if let Ok(issue) = serde_json::from_str::<Value>(line) {
            parsed_issues.push(issue);
        }
    }
    let mut known_beads = HashSet::new();
    for issue in &parsed_issues {
        if let Some(id) = issue.get("id").and_then(Value::as_str) {
            known_beads.insert(id);
        }
    }

    for entry in &ledger.entries {
        assert!(
            known_beads.contains(entry.owning_bead.as_str()),
            "owning_bead '{}' for artifact '{}' must exist in .beads/issues.jsonl",
            entry.owning_bead,
            entry.path
        );
    }
    Ok(())
}

#[test]
fn replay_reconstructs_head_hash_deterministically() -> Result<(), Box<dyn Error>> {
    let root = repo_root();
    let ledger_path = root.join("docs/evidence/release-evidence-ledger.json");
    let text = std::fs::read_to_string(&ledger_path)?;
    let ledger: ReleaseEvidenceLedgerArtifact = serde_json::from_str(&text)?;

    let mut expected_prev = GENESIS_PREV_HASH;
    for (idx, entry) in ledger.entries.iter().enumerate() {
        assert_eq!(entry.index, idx);
        assert_eq!(entry.prev_hash, expected_prev);
        let computed = compute_entry_hash(
            entry.index,
            &entry.path,
            &entry.sha256,
            entry.correlation_id.as_deref(),
            entry.source_commit.as_deref(),
            &entry.owning_bead,
            &entry.schema,
            &entry.prev_hash,
        );
        assert_eq!(computed, entry.entry_hash);
        expected_prev = &entry.entry_hash;
    }
    assert_eq!(expected_prev, ledger.head_hash);
    Ok(())
}
