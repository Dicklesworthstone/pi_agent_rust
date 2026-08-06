//! Permission snapshot drift classification and evidence.

use super::{
    Capability, ExtensionPermissionDriftClass, ExtensionPermissionDriftVerdict,
    ExtensionPermissionProvenanceStatus, ExtensionPermissionRiskLevel, ExtensionPermissionSnapshot,
    ExtensionPermissionTrust,
};
use std::collections::{BTreeMap, BTreeSet};

impl Default for ExtensionPermissionSnapshot {
    fn default() -> Self {
        Self {
            extension_id: String::new(),
            capabilities: Vec::new(),
            capability_manifest: None,
            policy_profile: None,
            manifest_checksum: None,
            provenance_snapshot_checksum: None,
            catalog_capabilities: Vec::new(),
            catalog_policy_profile: None,
            catalog_manifest_checksum: None,
            catalog_provenance_checksum: None,
            trust: ExtensionPermissionTrust::Untrusted,
        }
    }
}

#[must_use]
pub(super) fn normalize_capability_token(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_ascii_lowercase())
    }
}

#[must_use]
pub(super) fn snapshot_capability_set(snapshot: &ExtensionPermissionSnapshot) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for capability in &snapshot.capabilities {
        if let Some(capability) = normalize_capability_token(capability) {
            out.insert(capability);
        }
    }
    if let Some(manifest) = &snapshot.capability_manifest {
        for requirement in &manifest.capabilities {
            if let Some(capability) = normalize_capability_token(&requirement.capability) {
                out.insert(capability);
            }
        }
    }
    out
}

#[must_use]
pub(super) fn snapshot_catalog_capability_set(
    snapshot: &ExtensionPermissionSnapshot,
) -> BTreeSet<String> {
    snapshot
        .catalog_capabilities
        .iter()
        .filter_map(|capability| normalize_capability_token(capability))
        .collect()
}

#[must_use]
pub(super) fn snapshot_provenance_map(
    snapshot: &ExtensionPermissionSnapshot,
) -> BTreeMap<String, String> {
    let Some(manifest) = &snapshot.capability_manifest else {
        return BTreeMap::new();
    };
    let mut out = BTreeMap::new();
    for requirement in &manifest.capabilities {
        let Some(capability) = normalize_capability_token(&requirement.capability) else {
            continue;
        };
        if let Some(provenance) = &requirement.provenance {
            out.insert(
                capability,
                format!(
                    "{}|{}|{}|{}|{}",
                    provenance.source.trim(),
                    provenance.integrity.algorithm.trim(),
                    provenance.integrity.digest.trim().to_ascii_lowercase(),
                    provenance.publisher.id.trim(),
                    provenance.publisher.verification.trim()
                ),
            );
        }
    }
    out
}

#[must_use]
pub(super) fn capability_set_has_dangerous(caps: &BTreeSet<String>) -> bool {
    caps.iter()
        .any(|capability| Capability::parse(capability).is_some_and(Capability::is_dangerous))
}

#[must_use]
pub(super) fn capability_expansion_missing_provenance(
    added_capabilities: &BTreeSet<String>,
    current_provenance: &BTreeMap<String, String>,
) -> bool {
    added_capabilities
        .iter()
        .any(|capability| !current_provenance.contains_key(capability))
}

#[must_use]
pub(super) fn policy_profile_mismatch(
    previous: &ExtensionPermissionSnapshot,
    current: &ExtensionPermissionSnapshot,
    added_capabilities: &BTreeSet<String>,
) -> bool {
    let profile_changed = previous
        .policy_profile
        .zip(current.policy_profile)
        .is_some_and(|(previous, current)| previous != current);
    let catalog_profile_mismatch = current
        .catalog_policy_profile
        .zip(current.policy_profile)
        .is_some_and(|(catalog, current)| catalog != current);
    let missing_policy_for_expansion = !added_capabilities.is_empty()
        && (current.policy_profile.is_none() || current.catalog_policy_profile.is_none());

    profile_changed || catalog_profile_mismatch || missing_policy_for_expansion
}

#[must_use]
pub(super) fn checksum_mismatch(observed: Option<&String>, expected: Option<&String>) -> bool {
    observed
        .zip(expected)
        .is_some_and(|(observed, expected)| observed.trim() != expected.trim())
}

#[must_use]
pub(super) const fn permission_drift_recommended_action(
    verdict: ExtensionPermissionDriftVerdict,
    primary_class: ExtensionPermissionDriftClass,
) -> &'static str {
    match verdict {
        ExtensionPermissionDriftVerdict::Allow => "launch_extension",
        ExtensionPermissionDriftVerdict::AllowWithAudit => "launch_extension_and_record_audit",
        ExtensionPermissionDriftVerdict::ReviewRequired => match primary_class {
            ExtensionPermissionDriftClass::AddedDangerousCapabilities => {
                "require_explicit_operator_approval"
            }
            ExtensionPermissionDriftClass::AddedCapabilities => {
                "review_capability_expansion_before_launch"
            }
            ExtensionPermissionDriftClass::PolicyProfileMismatch => {
                "reconcile_policy_profile_before_launch"
            }
            _ => "review_permission_drift_before_launch",
        },
        ExtensionPermissionDriftVerdict::FailClosed => match primary_class {
            ExtensionPermissionDriftClass::MissingProvenance => "block_launch_refresh_provenance",
            ExtensionPermissionDriftClass::StaleManifest => "block_launch_refresh_manifest",
            ExtensionPermissionDriftClass::ProvenanceMismatch => {
                "block_launch_reconcile_provenance"
            }
            ExtensionPermissionDriftClass::PolicyProfileMismatch => {
                "block_launch_reconcile_policy_profile"
            }
            _ => "block_launch_until_evidence_matches",
        },
    }
}

#[must_use]
pub(super) fn primary_permission_drift_class(
    classes: &[ExtensionPermissionDriftClass],
) -> ExtensionPermissionDriftClass {
    for candidate in [
        ExtensionPermissionDriftClass::MissingProvenance,
        ExtensionPermissionDriftClass::ProvenanceMismatch,
        ExtensionPermissionDriftClass::StaleManifest,
        ExtensionPermissionDriftClass::PolicyProfileMismatch,
        ExtensionPermissionDriftClass::AddedDangerousCapabilities,
        ExtensionPermissionDriftClass::AddedCapabilities,
        ExtensionPermissionDriftClass::RemovedCapabilities,
        ExtensionPermissionDriftClass::ExplicitlyTrustedChange,
    ] {
        if classes.contains(&candidate) {
            return candidate;
        }
    }
    ExtensionPermissionDriftClass::NoDrift
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy)]
pub(super) struct PermissionDriftFlags {
    pub(super) has_added_dangerous: bool,
    pub(super) has_added: bool,
    pub(super) has_removed: bool,
    pub(super) missing_provenance: bool,
    pub(super) policy_mismatch: bool,
    pub(super) manifest_stale: bool,
    pub(super) provenance_mismatch: bool,
    pub(super) provenance_empty: bool,
    pub(super) trusted_change: bool,
}

#[must_use]
pub(super) fn permission_drift_classes(
    flags: PermissionDriftFlags,
) -> Vec<ExtensionPermissionDriftClass> {
    let mut classes = Vec::new();
    if flags.missing_provenance {
        classes.push(ExtensionPermissionDriftClass::MissingProvenance);
    }
    if flags.provenance_mismatch {
        classes.push(ExtensionPermissionDriftClass::ProvenanceMismatch);
    }
    if flags.manifest_stale {
        classes.push(ExtensionPermissionDriftClass::StaleManifest);
    }
    if flags.policy_mismatch {
        classes.push(ExtensionPermissionDriftClass::PolicyProfileMismatch);
    }
    if flags.has_added_dangerous {
        classes.push(ExtensionPermissionDriftClass::AddedDangerousCapabilities);
    } else if flags.has_added {
        classes.push(ExtensionPermissionDriftClass::AddedCapabilities);
    }
    if flags.has_removed {
        classes.push(ExtensionPermissionDriftClass::RemovedCapabilities);
    }
    if flags.trusted_change && !flags.missing_provenance {
        classes.push(ExtensionPermissionDriftClass::ExplicitlyTrustedChange);
    }
    if classes.is_empty() {
        classes.push(ExtensionPermissionDriftClass::NoDrift);
    }
    classes
}

#[must_use]
pub(super) const fn permission_drift_provenance_status(
    flags: PermissionDriftFlags,
) -> ExtensionPermissionProvenanceStatus {
    if flags.missing_provenance {
        ExtensionPermissionProvenanceStatus::Missing
    } else if flags.provenance_mismatch {
        ExtensionPermissionProvenanceStatus::Mismatch
    } else if flags.manifest_stale {
        ExtensionPermissionProvenanceStatus::Stale
    } else if flags.trusted_change {
        ExtensionPermissionProvenanceStatus::Trusted
    } else if flags.provenance_empty && !flags.has_added {
        ExtensionPermissionProvenanceStatus::NotRequired
    } else {
        ExtensionPermissionProvenanceStatus::Verified
    }
}

#[must_use]
pub(super) const fn permission_drift_verdict(
    flags: PermissionDriftFlags,
) -> ExtensionPermissionDriftVerdict {
    if flags.missing_provenance
        || flags.provenance_mismatch
        || flags.manifest_stale
        || (flags.policy_mismatch && flags.has_added)
    {
        ExtensionPermissionDriftVerdict::FailClosed
    } else if flags.trusted_change {
        ExtensionPermissionDriftVerdict::AllowWithAudit
    } else if flags.has_added_dangerous || flags.has_added || flags.policy_mismatch {
        ExtensionPermissionDriftVerdict::ReviewRequired
    } else if flags.has_removed {
        ExtensionPermissionDriftVerdict::AllowWithAudit
    } else {
        ExtensionPermissionDriftVerdict::Allow
    }
}

#[must_use]
pub(super) const fn permission_drift_risk_level(
    verdict: ExtensionPermissionDriftVerdict,
    flags: PermissionDriftFlags,
) -> ExtensionPermissionRiskLevel {
    match verdict {
        ExtensionPermissionDriftVerdict::FailClosed => {
            if flags.missing_provenance || (flags.policy_mismatch && flags.has_added) {
                ExtensionPermissionRiskLevel::Critical
            } else {
                ExtensionPermissionRiskLevel::High
            }
        }
        ExtensionPermissionDriftVerdict::ReviewRequired => {
            if flags.has_added_dangerous || flags.policy_mismatch {
                ExtensionPermissionRiskLevel::High
            } else {
                ExtensionPermissionRiskLevel::Medium
            }
        }
        ExtensionPermissionDriftVerdict::AllowWithAudit => {
            if flags.trusted_change && flags.has_added_dangerous {
                ExtensionPermissionRiskLevel::Medium
            } else {
                ExtensionPermissionRiskLevel::Low
            }
        }
        ExtensionPermissionDriftVerdict::Allow => ExtensionPermissionRiskLevel::Low,
    }
}
