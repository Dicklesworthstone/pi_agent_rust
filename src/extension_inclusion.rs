//! Final inclusion list generation for Pi extension candidates.
//!
//! Merges scoring tiers, candidate pool provenance, license verdicts,
//! and validation evidence into an authoritative inclusion list with
//! version pins. This output is the contract for acquisition and
//! conformance work.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ────────────────────────────────────────────────────────────────────────────
// Types
// ────────────────────────────────────────────────────────────────────────────

/// Version pin strategy.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum VersionPin {
    /// npm package with exact version.
    Npm {
        package: String,
        version: String,
        registry_url: String,
    },
    /// Git repository with path (commit hash if available).
    Git {
        repo: String,
        path: Option<String>,
        commit: Option<String>,
    },
    /// Direct URL.
    Url { url: String },
    /// Checksum-only pin (no upstream reference available).
    Checksum,
}

/// Extension category based on registration types.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionCategory {
    /// Registers a custom tool.
    Tool,
    /// Registers a slash command.
    Command,
    /// Registers a custom provider.
    Provider,
    /// Registers event hooks.
    #[serde(alias = "event-hook")]
    EventHook,
    /// Registers UI components (message renderer).
    #[serde(alias = "ui")]
    UiComponent,
    /// Registers flags or shortcuts.
    #[serde(alias = "shortcut", alias = "flag")]
    Configuration,
    /// Multiple registration types.
    Multi,
    /// No specific registrations detected.
    #[serde(alias = "basic", alias = "exec", alias = "session", alias = "unknown")]
    General,
}

/// A single entry in the final inclusion list.
///
/// Supports both the v1 format (from `ext_inclusion_list` binary) and the
/// v2 format (from `ext_inclusion_list` test generator).  Non-shared fields
/// are optional with serde defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InclusionEntry {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub score: Option<f64>,
    pub category: ExtensionCategory,
    // v1 fields
    #[serde(default)]
    pub registrations: Vec<String>,
    #[serde(default)]
    pub version_pin: Option<VersionPin>,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub artifact_path: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub source_tier: Option<String>,
    #[serde(default)]
    pub rationale: Option<String>,
    // v2 fields
    #[serde(default)]
    pub directory: Option<String>,
    #[serde(default)]
    pub provenance: Option<serde_json::Value>,
    #[serde(default)]
    pub capabilities: Option<Vec<String>>,
    #[serde(default)]
    pub risk_level: Option<String>,
    #[serde(default)]
    pub inclusion_rationale: Option<String>,
}

/// Exclusion note for high-scoring items not selected.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExclusionNote {
    pub id: String,
    pub score: f64,
    pub reason: String,
}

/// The final inclusion list document.
///
/// Supports both v1 format (binary output: task, stats, tier0, exclusions,
/// `category_coverage`) and v2 format (test output: summary, `tier1_review`,
/// coverage, `exclusion_notes`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InclusionList {
    pub schema: String,
    pub generated_at: String,
    // v1 fields
    #[serde(default)]
    pub task: Option<String>,
    #[serde(default)]
    pub stats: Option<InclusionStats>,
    #[serde(default)]
    pub tier0: Vec<InclusionEntry>,
    #[serde(default)]
    pub tier1: Vec<InclusionEntry>,
    #[serde(default)]
    pub tier2: Vec<InclusionEntry>,
    #[serde(default)]
    pub exclusions: Vec<ExclusionNote>,
    #[serde(default)]
    pub category_coverage: HashMap<String, usize>,
    // v2 fields
    #[serde(default)]
    pub summary: Option<serde_json::Value>,
    #[serde(default)]
    pub tier1_review: Vec<InclusionEntry>,
    #[serde(default)]
    pub coverage: Option<serde_json::Value>,
    #[serde(default)]
    pub exclusion_notes: Vec<ExclusionNote>,
}

/// Aggregate stats.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InclusionStats {
    pub total_included: usize,
    pub tier0_count: usize,
    pub tier1_count: usize,
    pub tier2_count: usize,
    pub excluded_count: usize,
    pub pinned_npm: usize,
    pub pinned_git: usize,
    pub pinned_url: usize,
    pub pinned_checksum_only: usize,
}

// ────────────────────────────────────────────────────────────────────────────
// Classification
// ────────────────────────────────────────────────────────────────────────────

/// Classify an extension by its registration types.
#[must_use]
pub fn classify_registrations(registrations: &[String]) -> ExtensionCategory {
    if registrations.len() > 1 {
        // Check if there are truly distinct categories.
        let has_tool = registrations.iter().any(|r| r == "registerTool");
        let has_cmd = registrations
            .iter()
            .any(|r| r == "registerCommand" || r == "registerSlashCommand");
        let has_provider = registrations.iter().any(|r| r == "registerProvider");
        let has_event = registrations
            .iter()
            .any(|r| r == "registerEvent" || r == "registerEventHook");
        let has_ui = registrations.iter().any(|r| r == "registerMessageRenderer");

        let distinct = [has_tool, has_cmd, has_provider, has_event, has_ui]
            .iter()
            .filter(|&&x| x)
            .count();
        if distinct > 1 {
            return ExtensionCategory::Multi;
        }
    }

    if registrations.is_empty() {
        return ExtensionCategory::General;
    }

    match registrations[0].as_str() {
        "registerTool" => ExtensionCategory::Tool,
        "registerCommand" | "registerSlashCommand" => ExtensionCategory::Command,
        "registerProvider" => ExtensionCategory::Provider,
        "registerEvent" | "registerEventHook" => ExtensionCategory::EventHook,
        "registerMessageRenderer" => ExtensionCategory::UiComponent,
        "registerFlag" | "registerShortcut" => ExtensionCategory::Configuration,
        _ => ExtensionCategory::General,
    }
}

/// Build inclusion rationale from tier, score, and registrations.
#[must_use]
pub fn build_rationale(
    tier: &str,
    score: f64,
    category: &ExtensionCategory,
    source_tier: &str,
) -> String {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let score_u = score as u32;
    let tier_reason = match tier {
        "tier-0" => "Official pi-mono baseline; must-pass conformance target",
        "tier-1" => format!("High score ({score_u}/100); passes all gates").leak(),
        "tier-2" => format!("Moderate score ({score_u}/100); stretch conformance target").leak(),
        _ => "Excluded",
    };

    let cat_reason = match category {
        ExtensionCategory::Tool => "Covers tool registration path",
        ExtensionCategory::Command => "Covers command/slash-command registration",
        ExtensionCategory::Provider => "Covers custom provider registration",
        ExtensionCategory::EventHook => "Covers event hook lifecycle",
        ExtensionCategory::UiComponent => "Covers UI component rendering",
        ExtensionCategory::Configuration => "Covers flag/shortcut configuration",
        ExtensionCategory::Multi => "Multi-type: covers multiple registration paths",
        ExtensionCategory::General => "General extension (export default)",
    };

    let source_reason = match source_tier {
        "official-pi-mono" => "official",
        "community" | "agents-mikeastock" => "community",
        "npm-registry" | "npm-registry-pi" => "npm",
        _ => source_tier,
    };

    format!("{tier_reason}. {cat_reason}. Source: {source_reason}.")
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_single_tool() {
        assert_eq!(
            classify_registrations(&["registerTool".into()]),
            ExtensionCategory::Tool
        );
    }

    #[test]
    fn classify_single_command() {
        assert_eq!(
            classify_registrations(&["registerCommand".into()]),
            ExtensionCategory::Command
        );
    }

    #[test]
    fn classify_provider() {
        assert_eq!(
            classify_registrations(&["registerProvider".into()]),
            ExtensionCategory::Provider
        );
    }

    #[test]
    fn classify_multi() {
        assert_eq!(
            classify_registrations(&["registerTool".into(), "registerCommand".into()]),
            ExtensionCategory::Multi
        );
    }

    #[test]
    fn classify_empty() {
        assert_eq!(classify_registrations(&[]), ExtensionCategory::General);
    }

    #[test]
    fn classify_flag_is_configuration() {
        assert_eq!(
            classify_registrations(&["registerFlag".into()]),
            ExtensionCategory::Configuration
        );
    }

    #[test]
    fn classify_event() {
        assert_eq!(
            classify_registrations(&["registerEventHook".into()]),
            ExtensionCategory::EventHook
        );
    }

    #[test]
    fn classify_renderer() {
        assert_eq!(
            classify_registrations(&["registerMessageRenderer".into()]),
            ExtensionCategory::UiComponent
        );
    }

    #[test]
    fn rationale_tier0() {
        let r = build_rationale("tier-0", 60.0, &ExtensionCategory::Tool, "official-pi-mono");
        assert!(r.contains("Official pi-mono baseline"));
        assert!(r.contains("tool registration"));
        assert!(r.contains("official"));
    }

    #[test]
    fn rationale_tier2() {
        let r = build_rationale("tier-2", 52.0, &ExtensionCategory::Provider, "community");
        assert!(r.contains("52/100"));
        assert!(r.contains("custom provider"));
    }

    #[test]
    fn inclusion_entry_serde_round_trip() {
        let entry = InclusionEntry {
            id: "test/ext".into(),
            name: Some("Test Extension".into()),
            tier: Some("tier-0".into()),
            score: Some(60.0),
            category: ExtensionCategory::Tool,
            registrations: vec!["registerTool".into()],
            version_pin: Some(VersionPin::Git {
                repo: "https://github.com/test/ext".into(),
                path: Some("extensions/test".into()),
                commit: None,
            }),
            sha256: Some("abc123".into()),
            artifact_path: Some("tests/ext_conformance/artifacts/test".into()),
            license: Some("MIT".into()),
            source_tier: Some("official-pi-mono".into()),
            rationale: Some("Official baseline".into()),
            directory: None,
            provenance: None,
            capabilities: None,
            risk_level: None,
            inclusion_rationale: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: InclusionEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "test/ext");
        assert_eq!(back.category, ExtensionCategory::Tool);
    }

    #[test]
    fn npm_version_pin_serde() {
        let pin = VersionPin::Npm {
            package: "@oh-my-pi/test".into(),
            version: "1.0.0".into(),
            registry_url: "https://registry.npmjs.org".into(),
        };
        let json = serde_json::to_string(&pin).unwrap();
        assert!(json.contains("npm"));
        assert!(json.contains("1.0.0"));
    }

    #[test]
    fn inclusion_list_serde() {
        let list = InclusionList {
            schema: "pi.ext.inclusion.v1".into(),
            generated_at: "2026-01-01T00:00:00Z".into(),
            task: Some("test".into()),
            stats: Some(InclusionStats {
                total_included: 0,
                tier0_count: 0,
                tier1_count: 0,
                tier2_count: 0,
                excluded_count: 0,
                pinned_npm: 0,
                pinned_git: 0,
                pinned_url: 0,
                pinned_checksum_only: 0,
            }),
            tier0: vec![],
            tier1: vec![],
            tier2: vec![],
            exclusions: vec![],
            category_coverage: HashMap::new(),
            summary: None,
            tier1_review: vec![],
            coverage: None,
            exclusion_notes: vec![],
        };
        let json = serde_json::to_string(&list).unwrap();
        let back: InclusionList = serde_json::from_str(&json).unwrap();
        assert_eq!(back.schema, "pi.ext.inclusion.v1");
    }
}
