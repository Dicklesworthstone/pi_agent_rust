#![forbid(unsafe_code)]
#![allow(
    clippy::must_use_candidate,
    clippy::too_many_arguments,
    clippy::uninlined_format_args,
    clippy::if_not_else,
    clippy::map_unwrap_or,
    clippy::too_many_lines
)]

use anyhow::{Context, Result, bail};
use chrono::Utc;
use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

pub const RELEASE_EVIDENCE_LEDGER_SCHEMA: &str = "pi.release_evidence.ledger.v1";
pub const GENESIS_PREV_HASH: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Debug, Parser)]
#[command(name = "release_evidence_ledger")]
#[command(about = "Build, verify, and replay proof-carrying release evidence ledgers")]
struct Cli {
    #[command(subcommand)]
    command: CommandMode,
}

#[derive(Debug, Subcommand)]
enum CommandMode {
    Build(BuildArgs),
    Verify(VerifyArgs),
    Replay(ReplayArgs),
}

#[derive(Debug, Args)]
struct BuildArgs {
    #[arg(long, default_value = ".")]
    repo_root: PathBuf,
    #[arg(long, default_value = "docs/evidence/release-evidence-ledger.json")]
    output: PathBuf,
}

#[derive(Debug, Args)]
struct VerifyArgs {
    #[arg(long, default_value = "docs/evidence/release-evidence-ledger.json")]
    input: PathBuf,
    #[arg(long, default_value = ".")]
    repo_root: PathBuf,
}

#[derive(Debug, Args)]
struct ReplayArgs {
    #[arg(long, default_value = "docs/evidence/release-evidence-ledger.json")]
    input: PathBuf,
}

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

#[derive(Debug, Clone, Serialize)]
pub struct ReplayStep<'a> {
    pub index: usize,
    pub path: &'a str,
    pub prev_hash: &'a str,
    pub entry_hash: &'a str,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReplayReport<'a> {
    pub schema: String,
    pub total_steps: usize,
    pub final_state_hash: String,
    pub head_hash_matched: bool,
    pub trace: Vec<ReplayStep<'a>>,
}

#[must_use]
pub fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[must_use]
pub fn compute_sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_encode(&hasher.finalize())
}

pub fn compute_sha256_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path)
        .with_context(|| format!("failed to read file for sha256 at {}", path.display()))?;
    Ok(compute_sha256_bytes(&bytes))
}

#[allow(clippy::too_many_arguments)]
#[must_use]
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

#[must_use]
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
                if file_path.exists() {
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
                } else {
                    raw_issues.push(VerifyIssue::MissingFile(&entry.path));
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

pub fn replay_ledger(ledger: &ReleaseEvidenceLedgerArtifact) -> Result<ReplayReport<'_>> {
    let mut trace = Vec::with_capacity(ledger.entries.len());
    let mut expected_prev_hash = GENESIS_PREV_HASH;

    for (idx, entry) in ledger.entries.iter().enumerate() {
        if entry.prev_hash != expected_prev_hash {
            bail!(
                "replay failed: broken chain at index {idx}: expected prev_hash {expected_prev_hash}, got {}",
                entry.prev_hash
            );
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
            bail!(
                "replay failed: invalid entry hash at index {idx}: computed {computed_hash}, got {}",
                entry.entry_hash
            );
        }

        trace.push(ReplayStep {
            index: entry.index,
            path: &entry.path,
            prev_hash: &entry.prev_hash,
            entry_hash: &entry.entry_hash,
        });

        expected_prev_hash = &entry.entry_hash;
    }

    let final_last_hash = ledger
        .entries
        .last()
        .map_or(GENESIS_PREV_HASH, |e| e.entry_hash.as_str());
    let head_hash_matched = final_last_hash == ledger.head_hash;

    Ok(ReplayReport {
        schema: "pi.release_evidence.replay_report.v1".to_string(),
        total_steps: trace.len(),
        final_state_hash: expected_prev_hash.to_string(),
        head_hash_matched,
        trace,
    })
}

fn collect_json_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let dir_entries = fs::read_dir(dir)?;
    for entry_res in dir_entries {
        let entry = entry_res?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.ends_with(".json") && name_str != "release-evidence-ledger.json" {
            out.push(dir.join(&name));
        }
    }
    out.sort();
    Ok(out)
}

fn build_single_entry(
    index: usize,
    file_path: &Path,
    root: &Path,
    prev_hash: &str,
) -> Result<ReleaseEvidenceLedgerEntry> {
    let rel_path = file_path
        .strip_prefix(root)
        .unwrap_or(file_path)
        .to_string_lossy()
        .to_string();

    let sha256 = compute_sha256_file(file_path)?;
    let text = fs::read_to_string(file_path)?;
    let json_val: Value = serde_json::from_str(&text).unwrap_or(Value::Null);

    let schema = json_val
        .get("schema")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();

    let correlation_id = json_val
        .get("correlation_id")
        .or_else(|| json_val.get("run_id"))
        .and_then(Value::as_str)
        .map(String::from);

    let source_commit = json_val
        .get("source_commit")
        .or_else(|| json_val.get("git_commit"))
        .and_then(Value::as_str)
        .map(String::from);

    let owning_bead = json_val
        .get("bead_id")
        .or_else(|| json_val.get("source_bead"))
        .or_else(|| json_val.get("bead"))
        .or_else(|| json_val.get("generated_by_bead"))
        .or_else(|| json_val.get("policy_owner_issue"))
        .and_then(Value::as_str)
        .unwrap_or("bd-sog97")
        .to_string();

    let entry_hash = compute_entry_hash(
        index,
        &rel_path,
        &sha256,
        correlation_id.as_deref(),
        source_commit.as_deref(),
        &owning_bead,
        &schema,
        prev_hash,
    );

    Ok(ReleaseEvidenceLedgerEntry {
        index,
        path: rel_path,
        sha256,
        correlation_id,
        source_commit,
        owning_bead,
        schema,
        prev_hash: prev_hash.to_string(),
        entry_hash,
    })
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        CommandMode::Build(args) => {
            let root = &args.repo_root;
            let evidence_dir = root.join("docs/evidence");
            if !evidence_dir.exists() {
                bail!("evidence directory not found at {}", evidence_dir.display());
            }

            let files = collect_json_files(&evidence_dir)?;
            let mut entries = Vec::with_capacity(files.len());

            for (index, file_path) in files.iter().enumerate() {
                let prev_ref = entries
                    .last()
                    .map_or(GENESIS_PREV_HASH, |e: &ReleaseEvidenceLedgerEntry| {
                        e.entry_hash.as_str()
                    });
                let entry = build_single_entry(index, file_path, root, prev_ref)?;
                entries.push(entry);
            }

            let head_hash = entries
                .last()
                .map_or_else(|| GENESIS_PREV_HASH.to_string(), |e| e.entry_hash.clone());
            let summary = ReleaseEvidenceLedgerSummary {
                total_artifacts: entries.len(),
                verified_intact: entries.len(),
                missing_artifacts: 0,
                sha_mismatches: 0,
            };

            let artifact = ReleaseEvidenceLedgerArtifact {
                schema: RELEASE_EVIDENCE_LEDGER_SCHEMA.to_string(),
                generated_at: Utc::now().to_rfc3339(),
                contract_path: "docs/contracts/release-evidence-ledger-contract.json".to_string(),
                head_hash,
                entry_count: entries.len(),
                entries,
                summary,
            };

            let json_out = serde_json::to_string_pretty(&artifact)?;
            let out_path = root.join(&args.output);
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&out_path, json_out)?;
            println!(
                "Successfully built release evidence ledger with {} entries to {}",
                artifact.entry_count,
                out_path.display()
            );
        }
        CommandMode::Verify(args) => {
            let ledger_text = fs::read_to_string(&args.input)
                .with_context(|| format!("failed to read ledger from {}", args.input.display()))?;
            let ledger: ReleaseEvidenceLedgerArtifact = serde_json::from_str(&ledger_text)?;
            let report = verify_ledger(&ledger, &args.repo_root);
            println!("{}", serde_json::to_string_pretty(&report)?);
            if report.status != "pass" {
                bail!("Release evidence ledger verification failed");
            }
        }
        CommandMode::Replay(args) => {
            let ledger_text = fs::read_to_string(&args.input)
                .with_context(|| format!("failed to read ledger from {}", args.input.display()))?;
            let ledger: ReleaseEvidenceLedgerArtifact = serde_json::from_str(&ledger_text)?;
            let report = replay_ledger(&ledger)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
    }

    Ok(())
}
