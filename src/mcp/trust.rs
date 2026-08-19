//! MCP server trust lifecycle (bd-cv653.6.1).
//!
//! Server processes are capability-equivalent to `exec`: a configured server
//! never spawns until an operator explicitly acknowledges it. States:
//! `pending` (never acknowledged) → `acknowledged` (may spawn) and `denied`
//! (never spawn, fail-closed). Every transition is audit-logged with
//! operator provenance, and a trust decision binds to the server's
//! fingerprint — changing the command/args/url re-pends the server.
//!
//! v1 acknowledgement surface: `/mcp trust <name>` (explicit command beats a
//! modal while the TUI stack is mid-migration). Executing a pending server's
//! tool returns a typed `[MCP_TRUST_PENDING]` refusal naming the remedy.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Trust record format version.
const TRUST_SCHEMA_VERSION: u32 = 1;

/// One server's trust state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustState {
    /// Explicitly acknowledged by the operator; may spawn.
    Acknowledged,
    /// Explicitly denied; never spawns (fail-closed).
    Denied,
}

/// An audit entry for one transition.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustAuditEntry {
    /// ISO-8601 timestamp.
    pub at: String,
    /// `acknowledged` | `denied` | `reset`.
    pub action: String,
    /// Who acted (`operator` for the local CLI user).
    pub by: String,
    /// Fingerprint the action applied to.
    pub fingerprint: String,
}

/// One server's persisted record.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustRecord {
    pub state: TrustState,
    /// Fingerprint of the spawn target when the decision was made.
    pub fingerprint: String,
    pub by: String,
    pub at: String,
    #[serde(default)]
    pub audit: Vec<TrustAuditEntry>,
}

/// On-disk store shape.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrustFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    servers: HashMap<String, TrustRecord>,
}

/// The trust store (file-backed, line of truth for spawns).
#[derive(Debug)]
pub struct TrustStore {
    path: PathBuf,
    servers: HashMap<String, TrustRecord>,
}

/// The effective trust decision for a server right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustDecision {
    /// May spawn.
    Acknowledged,
    /// Never recorded, or the fingerprint changed since the decision.
    Pending,
    /// Explicitly denied.
    Denied,
}

impl TrustStore {
    /// Load from `path` (absent file → empty store; malformed → error,
    /// fail-closed).
    ///
    /// # Errors
    ///
    /// Returns an error when the file exists but cannot be parsed.
    pub fn load(path: &Path) -> Result<Self> {
        let servers = match std::fs::read_to_string(path) {
            Ok(content) => {
                let file: TrustFile = serde_json::from_str(&content).map_err(|err| {
                    Error::tool(
                        "mcp",
                        format!(
                            "[MCP_TRUST_CORRUPT] {} is not valid: {err}; \
                             delete it to reset all MCP trust decisions",
                            path.display()
                        ),
                    )
                })?;
                file.servers
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(err) => {
                return Err(Error::tool(
                    "mcp",
                    format!("[MCP_TRUST_IO] cannot read {}: {err}", path.display()),
                ));
            }
        };
        Ok(Self {
            path: path.to_path_buf(),
            servers,
        })
    }

    /// The decision for `name` running `fingerprint` right now.
    #[must_use]
    pub fn decision(&self, name: &str, fingerprint: &str) -> TrustDecision {
        match self.servers.get(name) {
            Some(record) if record.fingerprint == fingerprint => match record.state {
                TrustState::Acknowledged => TrustDecision::Acknowledged,
                TrustState::Denied => TrustDecision::Denied,
            },
            // Missing record or a fingerprint change (config edited) → pending.
            _ => TrustDecision::Pending,
        }
    }

    /// Record an acknowledgement.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot be written.
    pub fn acknowledge(&mut self, name: &str, fingerprint: &str, by: &str) -> Result<()> {
        self.transition(
            name,
            fingerprint,
            TrustState::Acknowledged,
            by,
            "acknowledged",
        )
    }

    /// Record a denial (fail-closed; never spawns until reset).
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot be written.
    pub fn deny(&mut self, name: &str, fingerprint: &str, by: &str) -> Result<()> {
        self.transition(name, fingerprint, TrustState::Denied, by, "denied")
    }

    /// Forget a server (re-pends it on next use).
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot be written.
    pub fn reset(&mut self, name: &str, by: &str) -> Result<()> {
        if let Some(record) = self.servers.get_mut(name) {
            record.audit.push(TrustAuditEntry {
                at: now_iso(),
                action: "reset".to_string(),
                by: by.to_string(),
                fingerprint: record.fingerprint.clone(),
            });
        }
        self.servers.remove(name);
        self.save()
    }

    fn transition(
        &mut self,
        name: &str,
        fingerprint: &str,
        state: TrustState,
        by: &str,
        action: &str,
    ) -> Result<()> {
        let at = now_iso();
        let audit = TrustAuditEntry {
            at: at.clone(),
            action: action.to_string(),
            by: by.to_string(),
            fingerprint: fingerprint.to_string(),
        };
        let record = self
            .servers
            .entry(name.to_string())
            .or_insert_with(|| TrustRecord {
                state,
                fingerprint: fingerprint.to_string(),
                by: by.to_string(),
                at: at.clone(),
                audit: Vec::new(),
            });
        record.state = state;
        record.fingerprint = fingerprint.to_string();
        record.by = by.to_string();
        record.at = at;
        record.audit.push(audit);
        self.save()
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| {
                Error::tool(
                    "mcp",
                    format!("[MCP_TRUST_IO] cannot create {}: {err}", parent.display()),
                )
            })?;
        }
        let file = TrustFile {
            version: TRUST_SCHEMA_VERSION,
            servers: self.servers.clone(),
        };
        let rendered = serde_json::to_string_pretty(&file)
            .map_err(|err| Error::tool("mcp", format!("[MCP_TRUST_IO] serialize failed: {err}")))?;
        // Atomic write: temp + rename in the same directory.
        let mut temp =
            tempfile::NamedTempFile::new_in(self.path.parent().unwrap_or_else(|| Path::new(".")))
                .map_err(|err| Error::tool("mcp", format!("[MCP_TRUST_IO] temp file: {err}")))?;
        std::io::Write::write_all(&mut temp, rendered.as_bytes())
            .map_err(|err| Error::tool("mcp", format!("[MCP_TRUST_IO] write: {err}")))?;
        temp.persist(&self.path)
            .map_err(|err| Error::tool("mcp", format!("[MCP_TRUST_IO] persist: {}", err.error)))?;
        Ok(())
    }

    /// Read-only view of all records (for `/mcp` listing).
    #[must_use]
    pub const fn records(&self) -> &HashMap<String, TrustRecord> {
        &self.servers
    }
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_by_default_and_after_fingerprint_change() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("trust.json");
        let mut store = TrustStore::load(&path).expect("load");
        assert_eq!(store.decision("srv", "fp1"), TrustDecision::Pending);

        store.acknowledge("srv", "fp1", "operator").expect("ack");
        assert_eq!(store.decision("srv", "fp1"), TrustDecision::Acknowledged);
        // Config change re-pends.
        assert_eq!(store.decision("srv", "fp2"), TrustDecision::Pending);
    }

    #[test]
    fn denial_is_fail_closed_and_sticky() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("trust.json");
        let mut store = TrustStore::load(&path).expect("load");
        store.deny("srv", "fp1", "operator").expect("deny");
        assert_eq!(store.decision("srv", "fp1"), TrustDecision::Denied);
        // Acknowledging over a denial is a fresh explicit act.
        store.acknowledge("srv", "fp1", "operator").expect("ack");
        assert_eq!(store.decision("srv", "fp1"), TrustDecision::Acknowledged);
    }

    #[test]
    fn persistence_roundtrip_with_audit() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("trust.json");
        {
            let mut store = TrustStore::load(&path).expect("load");
            store.acknowledge("srv", "fp1", "operator").expect("ack");
            store.deny("srv", "fp2", "operator").expect("deny");
        }
        let store = TrustStore::load(&path).expect("reload");
        assert_eq!(store.decision("srv", "fp2"), TrustDecision::Denied);
        let record = &store.records()["srv"]; // ubs:ignore test index — presence is the assertion
        assert_eq!(record.audit.len(), 2);
        assert_eq!(record.audit[0].action, "acknowledged");
        assert_eq!(record.audit[1].action, "denied");
        assert!(record.audit.iter().all(|a| a.by == "operator"));
    }

    #[test]
    fn reset_re_pends() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("trust.json");
        let mut store = TrustStore::load(&path).expect("load");
        store.acknowledge("srv", "fp1", "operator").expect("ack");
        store.reset("srv", "operator").expect("reset");
        assert_eq!(store.decision("srv", "fp1"), TrustDecision::Pending);
    }

    #[test]
    fn corrupt_store_fails_closed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("trust.json");
        std::fs::write(&path, "{not json").expect("write");
        let err = TrustStore::load(&path).expect_err("corrupt must fail");
        assert!(err.to_string().contains("MCP_TRUST_CORRUPT"), "{err}");
    }
}
