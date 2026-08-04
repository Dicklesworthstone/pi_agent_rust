//! Compatibility-ledger data contracts for extension preflight analysis.
//!
//! The scanner implementation remains in the parent module while the stable
//! public evidence model lives here.  Keeping these wire-format types separate
//! makes downstream consumers independent of scanner implementation details.

use crate::error::Result;
use serde::{Deserialize, Serialize};

use super::COMPAT_LEDGER_SCHEMA_VERSION;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompatEvidence {
    pub file: String,
    pub line: usize,
    pub column: usize,
    pub snippet: String,
}

impl CompatEvidence {
    #[must_use]
    pub const fn new(file: String, line: usize, column: usize, snippet: String) -> Self {
        Self {
            file,
            line,
            column,
            snippet,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompatCapabilityEvidence {
    pub capability: String,
    pub reason: String,
    pub evidence: Vec<CompatEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompatRewriteEvidence {
    pub from: String,
    pub to: String,
    pub evidence: Vec<CompatEvidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompatIssueEvidence {
    pub rule: String,
    pub message: String,
    pub evidence: Vec<CompatEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompatLedger {
    pub schema: String,
    pub capabilities: Vec<CompatCapabilityEvidence>,
    pub rewrites: Vec<CompatRewriteEvidence>,
    pub forbidden: Vec<CompatIssueEvidence>,
    pub flagged: Vec<CompatIssueEvidence>,
}

impl CompatLedger {
    #[must_use]
    pub fn empty() -> Self {
        Self {
            schema: COMPAT_LEDGER_SCHEMA_VERSION.to_string(),
            capabilities: Vec::new(),
            rewrites: Vec::new(),
            forbidden: Vec::new(),
            flagged: Vec::new(),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.capabilities.is_empty()
            && self.rewrites.is_empty()
            && self.forbidden.is_empty()
            && self.flagged.is_empty()
    }

    pub fn to_json_pretty(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }
}
