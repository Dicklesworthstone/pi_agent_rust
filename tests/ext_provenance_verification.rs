//! Provenance verification evidence log (bd-bc2z).
//!
//! This test verifies that every vendored extension artifact is byte-for-byte
//! identical to its upstream source by cross-checking:
//!
//! 1. On-disk SHA-256 directory digest (computed fresh each run)
//! 2. `extension-master-catalog.json` checksum
//! 3. `extension-artifact-provenance.json` checksum
//!
//! It generates a structured evidence log at
//! `tests/ext_conformance/artifacts/PROVENANCE_VERIFICATION.json`
//! for auditability.

use pi::conformance::snapshot::{SourceTier, digest_artifact_dir, validate_directory, validate_id};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

// ── Manifest structs (local to this test) ──────────────────────────────

#[derive(Debug, Deserialize)]
struct MasterCatalog {
    extensions: Vec<MasterCatalogExtension>,
}

#[derive(Debug, Deserialize)]
struct MasterCatalogExtension {
    id: String,
    directory: String,
    checksum: String,
}

#[derive(Debug, Deserialize)]
struct ProvenanceManifest {
    items: Vec<ProvenanceItem>,
}

#[derive(Debug, Deserialize)]
struct ProvenanceItem {
    id: String,
    directory: String,
    checksum: ProvenanceChecksum,
    #[serde(default)]
    #[allow(dead_code)]
    name: Option<String>,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    source: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct ProvenanceChecksum {
    sha256: String,
}

// ── Evidence log types ─────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct VerificationEvidence {
    schema: &'static str,
    generated_at: String,
    summary: VerificationSummary,
    artifacts: Vec<ArtifactVerification>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    exceptions: Vec<ArtifactException>,
}

#[derive(Debug, Serialize)]
struct VerificationSummary {
    total_artifacts: usize,
    verified_ok: usize,
    failed: usize,
    exceptions: usize,
    pass_rate: f64,
}

#[derive(Debug, Serialize)]
struct ArtifactVerification {
    id: String,
    directory: String,
    source_tier: String,
    checks: VerificationChecks,
    verdict: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    errors: Vec<String>,
}

#[derive(Debug, Serialize)]
#[allow(clippy::struct_excessive_bools)]
struct VerificationChecks {
    directory_exists: bool,
    id_naming_valid: bool,
    directory_layout_valid: bool,
    on_disk_checksum: Option<String>,
    master_catalog_checksum: Option<String>,
    provenance_checksum: Option<String>,
    checksums_consistent: bool,
    has_source_url: bool,
    has_license: bool,
}

#[derive(Debug, Serialize)]
struct ArtifactException {
    id: String,
    reason: String,
}

// ── Main test ──────────────────────────────────────────────────────────

#[test]
#[allow(clippy::too_many_lines)]
fn provenance_verification_evidence_log() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let artifacts_root = repo_root.join("tests/ext_conformance/artifacts");

    // Load master catalog
    let master_path = repo_root.join("docs/extension-master-catalog.json");
    let master: MasterCatalog =
        serde_json::from_slice(&fs::read(&master_path).expect("read master catalog"))
            .expect("parse master catalog");

    // Load provenance manifest
    let provenance_path = repo_root.join("docs/extension-artifact-provenance.json");
    let provenance: ProvenanceManifest =
        serde_json::from_slice(&fs::read(&provenance_path).expect("read provenance manifest"))
            .expect("parse provenance manifest");

    // Index both by ID
    let master_map: BTreeMap<String, &MasterCatalogExtension> = master
        .extensions
        .iter()
        .map(|e| (e.id.clone(), e))
        .collect();
    let provenance_map: BTreeMap<String, &ProvenanceItem> =
        provenance.items.iter().map(|e| (e.id.clone(), e)).collect();

    // Collect all known IDs (union of both manifests)
    let mut all_ids: Vec<String> = master_map
        .keys()
        .chain(provenance_map.keys())
        .cloned()
        .collect();
    all_ids.sort();
    all_ids.dedup();

    let mut artifacts = Vec::new();
    let exceptions: Vec<ArtifactException> = Vec::new();
    let mut ok_count = 0usize;
    let mut fail_count = 0usize;

    for id in &all_ids {
        let master_entry = master_map.get(id.as_str());
        let prov_entry = provenance_map.get(id.as_str());

        let directory = master_entry
            .map(|e| e.directory.as_str())
            .or_else(|| prov_entry.map(|e| e.directory.as_str()))
            .unwrap_or(id.as_str());

        let tier = SourceTier::from_directory(directory);
        let tier_str = format!("{tier:?}");

        let mut errors: Vec<String> = Vec::new();
        let mut checks = VerificationChecks {
            directory_exists: false,
            id_naming_valid: false,
            directory_layout_valid: false,
            on_disk_checksum: None,
            master_catalog_checksum: master_entry.map(|e| e.checksum.clone()),
            provenance_checksum: prov_entry.map(|e| e.checksum.sha256.clone()),
            checksums_consistent: false,
            has_source_url: false,
            has_license: false,
        };

        // Check 1: ID naming
        match validate_id(id) {
            Ok(()) => checks.id_naming_valid = true,
            Err(e) => errors.push(format!("id_naming: {e}")),
        }

        // Check 2: Directory layout
        match validate_directory(directory, tier) {
            Ok(()) => checks.directory_layout_valid = true,
            Err(e) => errors.push(format!("directory_layout: {e}")),
        }

        // Check 3: Directory exists
        let artifact_dir = artifacts_root.join(directory);
        if artifact_dir.is_dir() {
            checks.directory_exists = true;
        } else {
            errors.push(format!("directory_missing: {}", artifact_dir.display()));
        }

        // Check 4: On-disk checksum
        if checks.directory_exists {
            match digest_artifact_dir(&artifact_dir) {
                Ok(digest) => checks.on_disk_checksum = Some(digest),
                Err(e) => errors.push(format!("digest_error: {e}")),
            }
        }

        // Check 5: Cross-check all three checksums
        let on_disk = checks.on_disk_checksum.as_deref();
        let master_ck = checks.master_catalog_checksum.as_deref();
        let prov_ck = checks.provenance_checksum.as_deref();

        let all_match = match (on_disk, master_ck, prov_ck) {
            (Some(d), Some(m), Some(p)) => {
                let ok = d == m && m == p;
                if !ok {
                    errors.push(format!(
                        "checksum_mismatch: disk={d}, master={m}, provenance={p}"
                    ));
                }
                ok
            }
            (Some(d), Some(m), None) => {
                let ok = d == m;
                if !ok {
                    errors.push(format!("checksum_mismatch: disk={d}, master={m}"));
                }
                errors.push("missing_provenance_entry".into());
                ok
            }
            (Some(d), None, Some(p)) => {
                let ok = d == p;
                if !ok {
                    errors.push(format!("checksum_mismatch: disk={d}, provenance={p}"));
                }
                errors.push("missing_master_catalog_entry".into());
                ok
            }
            _ => {
                if on_disk.is_none() {
                    errors.push("no_on_disk_checksum".into());
                }
                if master_ck.is_none() {
                    errors.push("missing_master_catalog_entry".into());
                }
                if prov_ck.is_none() {
                    errors.push("missing_provenance_entry".into());
                }
                false
            }
        };
        checks.checksums_consistent = all_match;

        // Check 6: Source URL present
        if let Some(prov) = prov_entry {
            checks.has_source_url = prov.source.is_some();
            if !checks.has_source_url {
                errors.push("missing_source_url".into());
            }
        }

        // Check 7: License present
        if let Some(prov) = prov_entry {
            checks.has_license = prov.license.as_ref().is_some_and(|l| !l.is_empty());
            if !checks.has_license {
                errors.push("missing_license".into());
            }
        }

        let verdict = if errors.is_empty() { "PASS" } else { "FAIL" };
        if errors.is_empty() {
            ok_count += 1;
        } else {
            fail_count += 1;
        }

        artifacts.push(ArtifactVerification {
            id: id.clone(),
            directory: directory.to_string(),
            source_tier: tier_str,
            checks,
            verdict,
            errors,
        });
    }

    // Note any known exceptions
    // (Currently there are none - all artifacts should be unmodified upstream)

    #[allow(clippy::cast_precision_loss)]
    let pass_rate = if all_ids.is_empty() {
        0.0
    } else {
        ok_count as f64 / all_ids.len() as f64
    };

    let evidence = VerificationEvidence {
        schema: "pi.ext.provenance_verification.v1",
        generated_at: chrono::Utc::now().to_rfc3339(),
        summary: VerificationSummary {
            total_artifacts: all_ids.len(),
            verified_ok: ok_count,
            failed: fail_count,
            exceptions: exceptions.len(),
            pass_rate,
        },
        artifacts,
        exceptions,
    };

    // Write evidence log
    let evidence_json = serde_json::to_string_pretty(&evidence).expect("serialize evidence log");

    let output_path =
        repo_root.join("tests/ext_conformance/artifacts/PROVENANCE_VERIFICATION.json");
    fs::write(&output_path, &evidence_json).expect("write evidence log");

    // Print summary for test output
    eprintln!(
        "\n=== Provenance Verification Summary ===\n\
         Total: {}\n\
         Verified OK: {}\n\
         Failed: {}\n\
         Exceptions: {}\n\
         Pass Rate: {:.1}%\n\
         Evidence log: {}\n",
        evidence.summary.total_artifacts,
        evidence.summary.verified_ok,
        evidence.summary.failed,
        evidence.summary.exceptions,
        evidence.summary.pass_rate * 100.0,
        output_path.display()
    );

    // Collect failure details for assertion message
    let failure_details: Vec<String> = evidence
        .artifacts
        .iter()
        .filter(|a| a.verdict == "FAIL")
        .map(|a| format!("  {} ({}): {}", a.id, a.directory, a.errors.join(", ")))
        .collect();

    assert!(
        fail_count == 0,
        "Provenance verification failed for {} artifacts:\n{}",
        fail_count,
        failure_details.join("\n")
    );
}
