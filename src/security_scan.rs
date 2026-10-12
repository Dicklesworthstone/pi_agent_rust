//! Native source review and lockfile dependency inspection (bd-cv653.2.6).
//!
//! Source rule packs remain local. Dependency inventory is also offline and
//! reports unsupported origins instead of guessing a public package identity.

pub mod dependencies;
mod dependency_review;
mod osv;
mod source;

pub use source::{
    CompareReport, Disposition, Finding, Rule, RulePack, SARIF_SCHEMA_URI, SARIF_VERSION,
    SCAN_SCHEMA, compare, findings_from_sarif, fingerprint, load_dispositions, load_rule_packs,
    partition_by_disposition, run_scan, save_dispositions, to_sarif,
};

use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::model::{ContentBlock, TextContent};
use crate::tools::{Tool, ToolEffects, ToolOutput, ToolUpdate};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// One agent-facing security surface, with independent source and dependency engines.
pub struct SecurityScanTool {
    cwd: PathBuf,
    source: source::SecurityScanTool,
    osv: osv::Client,
}

impl SecurityScanTool {
    #[must_use]
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            source: source::SecurityScanTool::new(cwd),
            osv: osv::Client::default(),
        }
    }

    /// Trusted host override for an OSV-compatible service or loopback fixture.
    /// Tool arguments cannot redirect package metadata to a different service.
    pub fn with_osv_base_url(mut self, base: &str) -> Result<Self> {
        self.osv = self.osv.with_base(base)?;
        Ok(self)
    }

    /// Preserve the host's transport/VCR setup for dependency-service requests.
    #[must_use]
    pub fn with_osv_client(mut self, client: crate::http::client::Client) -> Self {
        self.osv = self.osv.with_http(client);
        self
    }
}

#[async_trait::async_trait]
impl Tool for SecurityScanTool {
    fn name(&self) -> &'static str {
        "security_scan"
    }
    fn label(&self) -> &'static str {
        "security scan"
    }
    fn description(&self) -> &'static str {
        "Review local source with plan/run/disposition/compare. dependency_plan inventories Cargo/npm lockfiles offline. audit_dependencies sends eligible public-registry package names, ecosystems and exact versions to OSV, returning known advisory matches and explicit completeness/exclusions. compare_dependencies compares saved audits offline with paged findings and per-package uncertainty. disposition_dependencies records one hash-bound review decision in a new report without hiding findings or changing future audits. Source code, lockfile paths and private-registry entries are not uploaded. New dependency reports never overwrite existing files. This is not exploitability or installed-code analysis."
    }
    fn parameters(&self) -> Value {
        let mut schema = self.source.parameters();
        schema["properties"]["op"]["enum"] = json!([
            "plan",
            "run",
            "disposition",
            "compare",
            "dependency_plan",
            "audit_dependencies",
            "compare_dependencies",
            "disposition_dependencies"
        ]);
        schema["properties"]["op"]["description"] = json!(
            "Source review: plan/run/disposition/compare. dependency_plan: offline lockfile inventory. audit_dependencies: OSV lookup. compare_dependencies: offline comparison. disposition_dependencies: exact-fingerprint review in a new SARIF artifact; does not alter future audits."
        );
        schema["properties"]["paths"]["description"] = json!(
            "Source ops: relative files/directories. Dependency ops: up to 32 relative Cargo.lock or package-lock.json/npm-shrinkwrap.json files; default root Cargo.lock and package-lock.json only."
        );
        schema["properties"]["timeoutMs"] = json!({"type":"integer","minimum":1,"maximum":120_000,"default":60_000,
            "description":"audit_dependencies only: total inventory/network deadline; blocking filesystem calls are not preemptible"});
        schema["properties"]["baseline"]["description"] = json!(
            "compare: source baseline. compare_dependencies: required root-level baseline .sarif filename from audit_dependencies."
        );
        schema["properties"]["current"] = json!({"type":"string",
            "description":"compare_dependencies/disposition_dependencies: required root-level native dependency .sarif filename; comparison requires the same lockfile paths as baseline"});
        schema["properties"]["offset"] = json!({"type":"integer","minimum":0,"default":0,
            "description":"compare_dependencies: stable page offset; compare returned report hashes across pages"});
        schema["properties"]["limit"] = json!({"type":"integer","minimum":1,"maximum":50,"default":20,
            "description":"compare_dependencies: findings per page; counts always cover the full comparison"});
        schema["properties"]["decision"] = json!({"type":"string",
            "enum":["accepted_risk","false_positive","reopen"],
            "description":"disposition_dependencies: explicit decision for one exact dependency fingerprint; reason and new sarifOut required"});
        schema["properties"]["expectedSha256"] = json!({"type":"string","pattern":"^[a-f0-9]{64}$",
            "description":"disposition_dependencies: required current.sha256 from compare_dependencies; changed source bytes are refused"});
        schema["properties"]["sarifOut"]["description"] = json!(
            "run: source report. audit_dependencies: optional new root-level .sarif. disposition_dependencies: required new root-level .sarif; original audit is never overwritten."
        );
        schema["properties"]["reason"]["description"] = json!(
            "disposition/disposition_dependencies: reason required; dependency decisions allow up to 2048 UTF-8 bytes"
        );
        schema["properties"]["fingerprint"]["description"] = json!(
            "disposition: source finding identity. disposition_dependencies: exact 64-hex pi/dependency/v1 fingerprint from the selected report"
        );
        schema
    }
    fn effects(&self) -> ToolEffects {
        self.source.effects().union(ToolEffects::network())
    }

    async fn execute(
        &self,
        call_id: &str,
        input: Value,
        on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let op = input
            .get("op")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if op == "audit_dependencies" {
            return self.osv.execute(&self.cwd, input).await;
        }
        if op == "compare_dependencies" {
            return dependency_review::compare(&self.cwd, input).await;
        }
        if op == "disposition_dependencies" {
            return dependency_review::apply_disposition(&self.cwd, input).await;
        }
        if op != "dependency_plan" {
            return self.source.execute(call_id, input, on_update).await;
        }
        let owner = AgentCx::for_current_or_request();
        if !owner.capabilities().io {
            return Err(Error::tool(
                "security_scan",
                "dependency inventory requires I/O capability",
            ));
        }
        owner
            .checkpoint()
            .map_err(|_| Error::tool("security_scan", "dependency inventory cancelled"))?;
        let input: dependencies::Input = serde_json::from_value(input)
            .map_err(|_| Error::tool("security_scan", "invalid dependency_plan arguments"))?;
        let cwd = self.cwd.clone();
        let inventory = asupersync::runtime::spawn_blocking(move || {
            dependencies::inventory(&cwd, &input.paths)
        })
        .await?;
        owner
            .checkpoint()
            .map_err(|_| Error::tool("security_scan", "dependency inventory cancelled"))?;
        let text = format!(
            "Dependency inventory: {} exact public-registry package/version pairs in {} lockfile(s); {} excluded entries. No vulnerability service was queried. Scope is selected lockfiles, not every workspace dependency.",
            inventory.packages.len(),
            inventory.lockfiles.len(),
            inventory.excluded.len()
        );
        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(text))],
            details: Some(inventory_preview(&inventory)),
            is_error: false,
        })
    }
}

// Keep model context bounded independently of the full SDK inventory and
// SARIF export. Duplicate installs can have many locations.
fn inventory_preview(inventory: &dependencies::Inventory) -> Value {
    json!({"schema":inventory.schema,"scope":inventory.scope,"lockfiles":inventory.lockfiles,
        "eligiblePackages":inventory.packages.len(),"excludedEntries":inventory.excluded.len(),
        "packages":inventory.packages.iter().take(50).map(|package|json!({
            "ecosystem":package.ecosystem,"name":package.name,"version":package.version,
            "locations":package.locations.iter().take(2).collect::<Vec<_>>(),
            "locationCount":package.locations.len(),"locationsTruncated":package.locations.len()>2
        })).collect::<Vec<_>>(),
        "excluded":inventory.excluded.iter().take(20).collect::<Vec<_>>(),
        "packagesTruncated":inventory.packages.len()>50,"exclusionsTruncated":inventory.excluded.len()>20})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dependency_inventory_preserves_the_source_tool_schema() {
        let tool = SecurityScanTool::new(Path::new("."));
        let schema = tool.parameters();
        for op in [
            "plan",
            "run",
            "disposition",
            "compare",
            "dependency_plan",
            "audit_dependencies",
            "compare_dependencies",
            "disposition_dependencies",
        ] {
            assert!(
                schema["properties"]["op"]["enum"]
                    .as_array()
                    .unwrap()
                    .contains(&json!(op))
            );
        }
        assert!(schema["properties"]["baseline"].is_object());
        assert!(schema["properties"]["fingerprint"].is_object());
        assert!(tool.effects().writes());
        assert!(tool.effects().networks());
    }

    #[test]
    fn offline_preview_limits_packages_and_locations_without_hiding_total_counts() {
        let package = dependencies::Package {
            ecosystem: "npm".into(),
            name: "example".into(),
            version: "1.0.0".into(),
            locations: (0..100)
                .map(|index| dependencies::Location {
                    lockfile: "package-lock.json".into(),
                    package_path: Some(format!("node_modules/parent-{index}/node_modules/example")),
                })
                .collect(),
        };
        let inventory = dependencies::Inventory {
            schema: dependencies::INVENTORY_SCHEMA.into(),
            scope: "fixture".into(),
            lockfiles: vec![],
            packages: vec![package; 100],
            excluded: vec![],
        };
        let preview = inventory_preview(&inventory);
        assert_eq!(preview["eligiblePackages"], 100);
        assert_eq!(preview["packages"].as_array().unwrap().len(), 50);
        assert_eq!(preview["packagesTruncated"], true);
        assert_eq!(
            preview["packages"][0]["locations"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(preview["packages"][0]["locationCount"], 100);
        assert_eq!(preview["packages"][0]["locationsTruncated"], true);
    }
}
