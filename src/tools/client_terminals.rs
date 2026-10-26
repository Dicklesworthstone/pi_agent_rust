//! Session-owned terminal execution supplied by an editor. Native Bash owns
//! admission, mediation, shell shaping and selection; the host owns transport.

use super::*;
use std::sync::RwLock;

/// An explicit shell invocation; never interpret the whole command as argv[0].
#[derive(Debug, Clone)]
pub struct TerminalCommand {
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    pub timeout: Option<Duration>,
}

/// Foreground commands admitted by the native tool policy. Errors must never
/// cause a local retry: a remote allocation may already have executed the line.
#[async_trait]
pub trait TerminalClient: Send + Sync {
    async fn execute(
        &self,
        command: TerminalCommand,
        on_update: Option<&(dyn Fn(ToolUpdate) + Send + Sync)>,
    ) -> Result<ToolOutput>;
}

/// Shared by a registry, its shallow snapshots and tools mounted later.
#[derive(Clone, Default)]
pub struct TerminalClientScope(Arc<RwLock<Option<Arc<dyn TerminalClient>>>>);

impl TerminalClientScope {
    pub(crate) fn set(&self, client: Arc<dyn TerminalClient>) -> Result<()> {
        *self
            .0
            .write()
            .map_err(|_| Error::session("Terminal client scope unavailable"))? = Some(client);
        Ok(())
    }

    pub(super) fn get(&self) -> Result<Option<Arc<dyn TerminalClient>>> {
        self.0
            .read()
            .map(|client| client.clone())
            .map_err(|_| Error::session("Terminal client scope unavailable"))
    }
}

impl BashTool {
    pub(super) fn client_terminal_command(&self, input: &BashInput) -> Result<TerminalCommand> {
        let command = self
            .shell_path
            .clone()
            .map_or_else(default_bash_shell, Ok)?;
        let line = self
            .command_prefix
            .as_deref()
            .filter(|prefix| !prefix.trim().is_empty())
            .map_or_else(
                || input.command.clone(),
                |prefix| format!("{prefix}\n{}", input.command),
            );
        // Match the direct runner's foreground child-wait semantics. The
        // client's terminal owns process cleanup; no local process is spawned.
        let line = format!("trap 'code=$?; wait; exit $code' EXIT\n{line}");
        let mut env = crate::http::proxy::child_process_env();
        env.extend(
            crate::bash_mediation::NON_INTERACTIVE_ENV
                .iter()
                .map(|(name, value)| ((*name).to_string(), (*value).to_string())),
        );
        Ok(TerminalCommand {
            command,
            args: vec!["-c".to_string(), line],
            env,
            cwd: self.cwd.clone(),
            timeout: match input.timeout {
                Some(0) => None,
                Some(seconds) => Some(Duration::from_secs(seconds)),
                None => Some(Duration::from_secs(DEFAULT_BASH_TIMEOUT_SECS)),
            },
        })
    }
}
