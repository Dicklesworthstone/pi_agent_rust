//! Explicit shared-key delegation between native hosts.
//!
//! The parent captures its bank and live owner once, then passes a bounded grant
//! in the child's environment. The child installs only the selected key tools.
//! Transcript/job ownership stays independent. These grants prevent accidental
//! cross-session reuse; they are not an OS sandbox against arbitrary shell I/O.
//!
//! This module provides the SDK boundary. The ordinary CLI registry does not
//! install or propagate grants automatically.

use super::{MemoryStore, SharedMemoryStore, SharedMemoryTool, failure, validate_session};
use crate::agent_cx::AgentCx;
use crate::error::Result;
use crate::jobs::JobSessionScope;
use crate::tools::{Tool, ToolEffects, ToolOutput, ToolRegistry, ToolUpdate};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

const GRANT_ENV: &str = "PI_SUBAGENT_SHARED_MEMORY";
const PARENT_ENV: &str = "PI_SUBAGENT_PARENT_PID";
const RUN_ENV: &str = "PI_SUBAGENT_RUN_ID";
const MAX_GRANT_BYTES: usize = 16 * 1024;
const GRANT_VERSION: u32 = 1;
const TOOL_NAMES: [&str; 3] = ["read_memory", "write_memory", "list_memory"];
const ALL_TOOLS: u8 = 0b111;

fn tool_bit(name: &str) -> u8 {
    match name {
        "read_memory" => 1,
        "write_memory" => 2,
        "list_memory" => 4,
        _ => 0,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum Access {
    ReadOnly,
    ReadWrite,
}

/// Host authority with a live owner resolver. Cloning a binding does not freeze
/// the owner: call `resolve` once for an entire queue, chain and retry sequence.
#[derive(Clone)]
pub struct SharedMemoryBinding {
    bank: Arc<MemoryStore>,
    scope: JobSessionScope,
    access: Access,
    source_root: Option<PathBuf>,
    allowed_tools: u8,
}

impl SharedMemoryBinding {
    #[must_use]
    pub const fn new(bank: Arc<MemoryStore>, scope: JobSessionScope) -> Self {
        Self {
            bank,
            scope,
            access: Access::ReadWrite,
            source_root: None,
            allowed_tools: ALL_TOOLS,
        }
    }

    /// Restrict this binding. No API upgrades an inherited read-only grant.
    #[must_use]
    pub const fn read_only(mut self) -> Self {
        self.access = Access::ReadOnly;
        self
    }

    /// Freeze the owner, with an explicit bound on a suspended host resolver.
    /// The caller should pass its remaining request budget, not restart a budget
    /// per child. A failed resolution must not fall back to a different session.
    pub async fn resolve(&self, timeout: Duration) -> Result<SharedMemoryGrant> {
        if timeout < Duration::from_millis(1) || timeout > Duration::from_hours(24) {
            return Err(failure(
                "PI_SHARED_MEMORY_TIMEOUT",
                "Resolution requires a budget from 1 ms to 24 hours",
            ));
        }
        let owner = AgentCx::for_current_or_request();
        checkpoint(&owner)?;
        if !owner.capabilities().io || !owner.capabilities().time {
            return Err(failure(
                "PI_SHARED_MEMORY_PERMISSION",
                "Delegation requires I/O and timer capabilities",
            ));
        }
        let now = owner
            .cx()
            .timer_driver()
            .map_or_else(asupersync::time::wall_now, |timer| timer.now());
        let session_id = asupersync::time::timeout(now, timeout, self.scope.session_id())
            .await
            .map_err(|_| {
                failure(
                    "PI_SHARED_MEMORY_TIMEOUT",
                    "Shared-memory owner resolution timed out",
                )
            })?
            .map_err(|_| invalid_grant())?;
        checkpoint(&owner)?;
        Ok(SharedMemoryGrant {
            store: SharedMemoryStore::new(Arc::clone(&self.bank), session_id)?,
            access: self.access,
            source_root: self
                .source_root
                .clone()
                .unwrap_or_else(|| self.bank.project_root.clone()),
            allowed_tools: self.allowed_tools,
        })
    }
}

fn checkpoint(owner: &AgentCx) -> Result<()> {
    owner.checkpoint().map_err(|_| {
        failure(
            "PI_SHARED_MEMORY_CANCELLED",
            "Shared-memory delegation cancelled before dispatch",
        )
    })
}

fn invalid_grant() -> crate::error::Error {
    failure(
        "PI_SHARED_MEMORY_DELEGATION",
        "Invalid or unavailable shared-memory grant; no fallback namespace is used",
    )
}

fn read_only_error() -> crate::error::Error {
    failure(
        "PI_SHARED_MEMORY_READ_ONLY",
        "This child may read shared keys but cannot modify them",
    )
}

/// Captured parent namespace: safe to clone across concurrent children and
/// retries; the live parent resolver is no longer consulted.
///
/// Deliberately no Debug/Serialize implementation: storage and scope
/// identifiers are private.
#[derive(Clone)]
pub struct SharedMemoryGrant {
    store: SharedMemoryStore,
    access: Access,
    source_root: PathBuf,
    allowed_tools: u8,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WireGrant {
    version: u32,
    parent_pid: u32,
    run_id: String,
    working_directory: PathBuf,
    database: PathBuf,
    project_root: PathBuf,
    project_key: String,
    session_id: String,
    access: Access,
    allowed_tools: u8,
}

fn valid_run_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

impl SharedMemoryGrant {
    /// Authorize a nested launcher without changing the bank, frozen owner, or
    /// inherited permission ceiling. The child's source root may be a worktree.
    #[must_use]
    pub fn binding(&self) -> SharedMemoryBinding {
        SharedMemoryBinding {
            bank: Arc::clone(&self.store.bank),
            scope: JobSessionScope::fixed(self.store.session_id.clone()),
            access: self.access,
            source_root: Some(self.source_root.clone()),
            allowed_tools: self.allowed_tools,
        }
    }

    /// Intersect an agent's explicit tool pin with this grant. No shared names
    /// means no delegation. A read-only ceiling still denies a selected writer.
    /// An omitted pin retains the inherited selection, never all possible tools.
    pub fn for_tool_selection(&self, tools: Option<&[String]>) -> Option<Self> {
        let mut grant = self.clone();
        if let Some(tools) = tools {
            let selected = tools.iter().fold(0, |bits, name| bits | tool_bit(name));
            grant.allowed_tools &= selected;
            if grant.allowed_tools == 0 {
                return None;
            }
            if !tools.iter().any(|name| name == "write_memory") {
                grant.access = Access::ReadOnly;
            }
        }
        Some(grant)
    }

    /// Check the requested source before creating an isolated worktree. The
    /// caller must not substitute an arbitrary external directory as a worktree.
    pub fn check_source_directory(&self, cwd: &Path) -> Result<()> {
        let root = self
            .source_root
            .canonicalize()
            .map_err(|_| invalid_grant())?;
        let cwd = cwd.canonicalize().map_err(|_| invalid_grant())?;
        if !cwd.starts_with(root) {
            return Err(failure(
                "PI_SHARED_MEMORY_DELEGATION_SCOPE",
                "The source directory is outside the authorized project",
            ));
        }
        Ok(())
    }

    /// Attach the captured grant to an explicitly chosen child. `cwd` is the
    /// final working directory, which may be a worktree of a checked source.
    /// This validates filesystem identities but does not spawn a process or
    /// enable tools. The host retains responsibility for process ownership.
    pub fn configure_command(&self, command: &mut Command, cwd: &Path, run_id: &str) -> Result<()> {
        // A caller that mistakenly continues after an error must not inherit a
        // broader ambient grant. Parent/run markers are set only on success.
        command.env_remove(GRANT_ENV);
        if !valid_run_id(run_id) {
            return Err(invalid_grant());
        }
        validate_session(&self.store.session_id).map_err(|_| invalid_grant())?;
        let grant = WireGrant {
            version: GRANT_VERSION,
            parent_pid: std::process::id(),
            run_id: run_id.to_string(),
            working_directory: cwd.canonicalize().map_err(|_| invalid_grant())?,
            database: self.store.bank.db_path.clone(),
            project_root: self
                .store
                .bank
                .project_root
                .canonicalize()
                .map_err(|_| invalid_grant())?,
            project_key: self.store.bank.project_key.clone(),
            session_id: self.store.session_id.clone(),
            access: self.access,
            allowed_tools: self.allowed_tools,
        };
        if !grant.database.is_absolute()
            || grant.project_key.is_empty()
            || grant.project_key.len() > 128
        {
            return Err(invalid_grant());
        }
        let encoded = serde_json::to_string(&grant).map_err(|_| invalid_grant())?;
        if encoded.len() > MAX_GRANT_BYTES {
            return Err(invalid_grant());
        }
        command
            .current_dir(&grant.working_directory)
            .env(GRANT_ENV, encoded)
            .env(PARENT_ENV, grant.parent_pid.to_string())
            .env(RUN_ENV, run_id);
        Ok(())
    }

    /// Clear ambient shared authority on an unrelated child launch.
    pub fn clear_command(command: &mut Command) {
        command.env_remove(GRANT_ENV);
    }

    /// Decode an explicitly inherited grant. Missing means no delegation;
    /// malformed means an error, never a new local/global namespace. The host
    /// should call this once during startup, before accepting tool requests.
    pub fn from_environment(cwd: &Path) -> Result<Option<Self>> {
        let Some(raw) = std::env::var_os(GRANT_ENV) else {
            return Ok(None);
        };
        let parent_pid = std::env::var_os(PARENT_ENV);
        let run_id = std::env::var_os(RUN_ENV);
        Self::decode(&raw, cwd, parent_pid.as_deref(), run_id.as_deref()).map(Some)
    }

    fn decode(
        raw: &OsStr,
        cwd: &Path,
        parent_pid: Option<&OsStr>,
        run_id: Option<&OsStr>,
    ) -> Result<Self> {
        let raw = raw
            .to_str()
            .filter(|value| value.len() <= MAX_GRANT_BYTES)
            .ok_or_else(invalid_grant)?;
        let grant: WireGrant = serde_json::from_str(raw).map_err(|_| invalid_grant())?;
        let parent_pid = parent_pid
            .and_then(OsStr::to_str)
            .and_then(|value| value.parse::<u32>().ok())
            .ok_or_else(invalid_grant)?;
        let run_id = run_id.and_then(OsStr::to_str).ok_or_else(invalid_grant)?;
        if grant.version != GRANT_VERSION
            || parent_pid == 0
            || grant.parent_pid != parent_pid
            || !valid_run_id(run_id)
            || grant.run_id != run_id
            || !grant.database.is_absolute()
            || !grant.project_root.is_absolute()
            || !grant.working_directory.is_absolute()
            || grant.project_key.is_empty()
            || grant.project_key.len() > 128
            || grant.allowed_tools == 0
            || grant.allowed_tools & !ALL_TOOLS != 0
            || cwd.canonicalize().map_err(|_| invalid_grant())? != grant.working_directory
        {
            return Err(invalid_grant());
        }
        validate_session(&grant.session_id).map_err(|_| invalid_grant())?;
        let bank = Arc::new(MemoryStore {
            db_path: grant.database,
            project_root: grant.project_root,
            project_key: grant.project_key,
            secret_patterns: Vec::new(),
        });
        Ok(Self {
            store: SharedMemoryStore::new(bank, grant.session_id).map_err(|_| invalid_grant())?,
            access: grant.access,
            source_root: grant.working_directory,
            allowed_tools: grant.allowed_tools,
        })
    }

    /// Install the requested dedicated tools atomically with respect to name
    /// collisions. Their frozen parent scope ignores later job-session rebinding.
    /// Existing tools, tier choices and host settings are not reconstructed.
    /// A selected writer on a read-only grant is installed but rejects execution.
    pub fn install_tools(&self, registry: &mut ToolRegistry, enabled: &[&str]) -> Result<()> {
        let selected: Vec<_> = TOOL_NAMES
            .into_iter()
            .filter(|name| enabled.contains(name))
            .collect();
        // The exact role pin travels with the grant. A nested host cannot gain
        // list/read access merely by presenting a wider local enabled set.
        if selected
            .iter()
            .any(|name| self.allowed_tools & tool_bit(name) == 0)
        {
            return Err(failure(
                "PI_SHARED_MEMORY_TOOL_SCOPE",
                "A requested shared-memory tool is outside the inherited role selection",
            ));
        }
        if selected.is_empty() {
            return Ok(());
        }
        for name in TOOL_NAMES {
            if registry.get(name).is_some()
                || registry
                    .inactive_tools()
                    .iter()
                    .any(|tool| tool.name() == name)
            {
                return Err(failure(
                    "PI_SHARED_MEMORY_TOOL_COLLISION",
                    "A delegated shared-memory tool name is already registered",
                ));
            }
        }
        let tools: Vec<Box<dyn Tool>> = selected
            .into_iter()
            .map(|name| {
                let bank = Arc::clone(&self.store.bank);
                let mut inner = match name {
                    "read_memory" => SharedMemoryTool::read(bank),
                    "write_memory" => SharedMemoryTool::write(bank),
                    _ => SharedMemoryTool::list(bank),
                };
                inner.bind_job_session_scope(JobSessionScope::fixed(self.store.session_id.clone()));
                Box::new(DelegatedTool {
                    inner,
                    writable: self.access == Access::ReadWrite,
                }) as Box<dyn Tool>
            })
            .collect();
        registry.extend(tools);
        Ok(())
    }
}

struct DelegatedTool {
    inner: SharedMemoryTool,
    writable: bool,
}

#[async_trait::async_trait]
impl Tool for DelegatedTool {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn label(&self) -> &str {
        self.inner.label()
    }
    fn description(&self) -> &'static str {
        match self.name() {
            "read_memory" => {
                "Read a key and revision from this task's parent-shared namespace, separate from its transcript and jobs."
            }
            "write_memory" => {
                "Write exact text to the parent-shared namespace. Use expectedRevision (or absent for create-only). Read-only grants reject writes."
            }
            _ => {
                "List parent-shared keys, revisions and bounded previews with literal prefix filtering and key cursors."
            }
        }
    }
    fn parameters(&self) -> Value {
        self.inner.parameters()
    }
    fn effects(&self) -> ToolEffects {
        self.inner.effects()
    }
    // Do not forward bind_job_session_scope: the child's jobs do not own these keys.
    async fn execute(
        &self,
        id: &str,
        input: Value,
        update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        if self.name() == "write_memory" && !self.writable {
            return Err(read_only_error());
        }
        self.inner.execute(id, input, update).await
    }
}

#[cfg(test)]
#[path = "delegation_tests.rs"]
mod tests;
