//! Native child-agent orchestration.
//!
//! This module deliberately uses the Pi executable that is already running
//! (or the explicit `PI_SUBAGENT_PI_BINARY` override) instead of resolving a
//! `pi` binary through `PATH`.  That makes a Rust Pi parent reliably launch
//! Rust Pi children even on hosts that also have the TypeScript implementation
//! installed.

use crate::config::Config;
use crate::error::{Error, Result};
use crate::model::{ContentBlock, TextContent};
use crate::tools::{
    Tool, ToolEffects, ToolInvocationContext, ToolModelScope, ToolModelSelection, ToolOutput,
    ToolUpdate,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
#[cfg(all(test, unix))]
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

mod background;
mod bundled;
mod deadline;
#[cfg(test)]
mod deadline_tests;
mod definition;
mod execution;
#[cfg(test)]
mod execution_tests;
mod model_selection;
mod protocol;
mod seance;
mod task_file;
mod worker;
mod workflow;
mod workspace;

use deadline::Deadline;
use execution::ChildRunner;

const MAX_PARALLEL_TASKS: usize = 8;
const DEFAULT_CONCURRENCY: usize = 4;
const MAX_SUBAGENT_DEPTH: usize = 3;
const MAX_CHILD_OUTPUT_BYTES: usize = 256 * 1024;
/// v2 (bd-cv653.5.1) extends v1 additively with `data`, `schemaValid`,
/// `validationErrors`, and `schemaRetries` on schema-bearing results; every
/// v1 field is unchanged, so v1 consumers keep working.
const SUBAGENT_RESULT_SCHEMA: &str = "pi.subagent.result.v2";
const SUBAGENT_PROGRESS_SCHEMA: &str = "pi.subagent.progress.v1";
/// Per-field byte budget for `output`/`error` in the opt-in structured block.
const STRUCTURED_FIELD_LIMIT_BYTES: usize = 2 * 1024;
/// Byte budget for the JSON payload of the opt-in structured block.
const STRUCTURED_BLOCK_LIMIT_BYTES: usize = 16 * 1024;
const STRUCTURED_BLOCK_OPEN: &str = "<subagent-structured-result>";
const STRUCTURED_BLOCK_CLOSE: &str = "</subagent-structured-result>";
const STRUCTURED_TRUNCATION_MARKER: &str = "…[truncated]";
const DEFAULT_CHILD_TOOLS: &str = "read,bash,edit,write,grep,find,ls,hashline_edit,hub";
const TAN_RESULT_SCHEMA: &str = "pi.background-tan.result.v1";
const TAN_AGENT_NAME: &str = "tan";
const TAN_SYSTEM_PROMPT: &str = "You are a background tangential coding agent. Complete the assigned work autonomously in the current working directory. Keep your final response concise and lead with the concrete outcome, changed files, and verification performed. Do not ask follow-up questions.";

type UpdateCallback = Arc<dyn Fn(ToolUpdate) + Send + Sync>;
/// Optional live admission policy retained by retries and explicit revival.
type LaunchAdmission = Arc<dyn Fn() -> Result<()> + Send + Sync>;

/// Settled result from an interactive `/tan` background child.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TanCompletion {
    pub schema: &'static str,
    pub hub_id: Option<String>,
    pub task: String,
    pub status: String,
    pub output: String,
    pub error: Option<String>,
    pub is_error: bool,
}

impl TanCompletion {
    fn from_result(result: SubagentResult) -> Self {
        Self {
            schema: TAN_RESULT_SCHEMA,
            hub_id: result.hub_id,
            task: result.task,
            status: result.status.as_str().to_string(),
            output: result.output,
            error: result.error,
            is_error: result.is_error,
        }
    }

    /// Follow-up content delivered to the parent agent at its next idle turn
    /// boundary. The bounded excerpt prevents a verbose child from consuming
    /// the entire next-turn context.
    #[must_use]
    pub fn follow_up_text(&self) -> String {
        let summary_source = if self.output.trim().is_empty() {
            self.error.as_deref().unwrap_or("(no output)")
        } else {
            self.output.trim()
        };
        let summary = truncated_field(summary_source, 16 * 1024);
        format!(
            "[background tan {} settled: {}]\nwork: {}\nsummary:\n{}",
            self.hub_id.as_deref().unwrap_or("unregistered"),
            self.status,
            self.task,
            summary
        )
    }

    /// Display-only completion card for the interactive transcript. The
    /// authoritative model delivery is [`Self::follow_up_text`].
    #[must_use]
    pub fn card_text(&self) -> String {
        let marker = if self.is_error { "failed" } else { "completed" };
        format!("(/tan {marker})\n{}", self.follow_up_text())
    }
}

/// A native tool that delegates bounded work to isolated Pi child processes.
pub struct SubagentTool {
    cwd: PathBuf,
    global_dir: PathBuf,
    /// Explicit host admission for project-authored agent definitions.
    project_trusted: bool,
    /// A captured launch decision is authoritative over the standalone flag.
    workspace_trust_guard: Option<crate::workspace_trust::WorkspaceTrustGuard>,
    /// Shared admission policy for saved sessions and task files, including live roots.
    workspace: crate::workspace::WorkspaceHandle,
    child_binary: PathBuf,
    structured_results: bool,
    /// Model spec children run with when their agent definition does not pin
    /// `model:` — the `task` role spec, else `smol` (bd-cv653.3.1).
    role_model_spec: Option<String>,
    /// The role that actually supplied the spec, distinct from model identity.
    role_model_role: Option<crate::models::ModelRole>,
    /// Effective host policy used when admitting explicit task model choices.
    model_config: Option<Config>,
    /// Live host identity for tools mounted in an agent's registry.
    model_scope: Option<ToolModelScope>,
    /// Live session budget; each admitted run retains its original gate.
    cost_scope: crate::cost::CostScope,
    /// An explicit launch-time snapshot for background host commands.
    parent_model: Option<ToolModelSelection>,
    /// Host ceiling for the entire request, not a fresh allowance per child.
    timeout: Option<Duration>,
    /// Fallback policy for direct SDK calls; live Agent execution supplies its
    /// own projection so discoveries stay scoped to the invoking session.
    privacy: Option<crate::text_completion::AuxiliaryPrivacy>,
    /// The same native owner identity used by jobs list/wait/cancel.
    job_session_scope: crate::jobs::JobSessionScope,
}

/// Private execution policy retained for an explicit hub revival. In
/// particular, the enabled-by-default hub cannot turn a synthetic roster
/// entry into a new delegation or rediscover a more permissive definition.
#[derive(Clone)]
pub(crate) struct RevivalSpec {
    agent: AgentDefinition,
    task: SubagentTask,
    step: Option<usize>,
    cwd: PathBuf,
    global_dir: PathBuf,
    child_binary: PathBuf,
    role_model_spec: Option<String>,
    parent_model: Option<ToolModelSelection>,
    timeout: Duration,
    depth: usize,
    public_task: Option<String>,
    launch_admission: Option<LaunchAdmission>,
    cost_budget: Option<crate::cost::CostBudget>,
}

impl RevivalSpec {
    /// Public assignment label; the private launch keeps its complete prompt.
    pub(crate) fn public_task(&self) -> Option<&str> {
        self.public_task
            .as_deref()
            .or(self.task.task_file.as_deref())
    }

    fn display_task(&self) -> &str {
        self.public_task()
            .unwrap_or_else(|| self.task.display_task())
    }

    fn check_launch_admission(&self) -> Result<()> {
        self.launch_admission
            .as_ref()
            .map_or(Ok(()), |check| check())
    }
}

/// Execute one explicitly requested replacement through the ordinary native
/// child runner. A fresh request gets a fresh clock bounded by its original
/// launch ceiling and the current caller's inherited deadline/capabilities.
pub(crate) async fn revive_child(
    id: &str,
    on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
) -> Result<ToolOutput> {
    let owner = crate::agent_cx::AgentCx::for_current_or_request();
    if owner.checkpoint().is_err() {
        return Err(Error::tool(
            "hub",
            "PI_HUB_REVIVAL_CANCELLED: caller is cancelled",
        ));
    }
    let capabilities = owner.capabilities();
    if !capabilities.io || !capabilities.spawn || !capabilities.time {
        return Err(Error::tool(
            "hub",
            "PI_SUBAGENT_PERMISSION: child execution requires I/O, spawn and timer capabilities",
        ));
    }
    if current_subagent_depth() >= MAX_SUBAGENT_DEPTH {
        return Err(Error::tool(
            "subagent",
            format!("Refusing nested subagent depth above {MAX_SUBAGENT_DEPTH}"),
        ));
    }
    let source = crate::agent_hub::registry()
        .lock()
        .map_err(|_| Error::tool("hub", "agent registry lock poisoned"))?
        .native_revival_source(id)?;
    let prompt = source.continuation_prompt()?;
    let launch = source.launch;
    let deadline = Deadline::for_request(Some(launch.timeout), None)?;
    let mut task = launch.task.clone();
    task.task = prompt;
    let agents = BTreeMap::from([(launch.agent.name.clone(), launch.agent.clone())]);
    let runner = ChildRunner::new(
        launch.cwd.clone(),
        launch.global_dir.clone(),
        launch.child_binary.clone(),
        launch.role_model_spec.clone(),
        source.entry.kind,
        deadline,
    )
    .with_parent_model(launch.parent_model.clone())
    .with_cost_budget(launch.cost_budget.clone())
    .with_revival(id.to_string(), launch.clone());
    let mut result = owner
        .with_current(runner.run_one(&agents, task, launch.step, on_update.map(Arc::from)))
        .await;
    // Continuation and schema-correction prompts are transport details. The
    // returned assignment and any later revival keep the authored source.
    result.task = launch.display_task().to_string();
    let text = render_results(std::slice::from_ref(&result));
    Ok(ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(text))],
        details: Some(json!({
            "schema": "pi.agent-hub.revive/v1",
            "id": result.hub_id,
            "revivedFrom": id,
            "result": result,
        })),
        is_error: result.is_error,
    })
}

impl SubagentTool {
    #[must_use]
    pub fn new(cwd: &Path) -> Self {
        let child_binary = std::env::var_os("PI_SUBAGENT_PI_BINARY")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::current_exe().ok())
            .unwrap_or_else(|| PathBuf::from("<current executable unavailable>"));
        Self {
            cwd: cwd.to_path_buf(),
            global_dir: Config::global_dir(),
            project_trusted: false,
            workspace_trust_guard: None,
            workspace: crate::workspace::WorkspaceHandle::default(),
            child_binary,
            structured_results: false,
            role_model_spec: None,
            role_model_role: None,
            model_config: None,
            model_scope: None,
            cost_scope: crate::cost::CostScope::default(),
            parent_model: None,
            timeout: None,
            privacy: None,
            job_session_scope: crate::jobs::JobSessionScope::default(),
        }
    }

    /// Share the owning agent's live additional roots with saved-session
    /// consultation. Changes to this handle apply to subsequent admission checks.
    #[must_use]
    pub fn with_workspace(mut self, workspace: crate::workspace::WorkspaceHandle) -> Self {
        self.workspace = workspace;
        self
    }

    /// Admit project-authored agent definitions for a standalone host. The
    /// default is false; model-selected scope cannot grant workspace trust.
    /// A supplied launch guard remains authoritative over this flag.
    #[must_use]
    pub const fn with_project_trust(mut self, trusted: bool) -> Self {
        self.project_trusted = trusted;
        self
    }

    /// Retain and revalidate the host's original workspace admission before
    /// loading project profiles. User profiles use the same captured global
    /// directory as the host, even if the process environment changes later.
    #[must_use]
    pub fn with_workspace_trust_guard(
        mut self,
        guard: Option<crate::workspace_trust::WorkspaceTrustGuard>,
    ) -> Self {
        if let Some(guard) = &guard {
            self.global_dir = guard.global_dir().to_path_buf();
        }
        self.workspace_trust_guard = guard;
        self
    }

    /// Configure privacy for saved-session consultation through direct SDK
    /// calls. Live Agent calls use the invoking session's current vault.
    #[must_use]
    pub fn with_secrets_settings(
        mut self,
        settings: Option<&crate::secrets::SecretsSettings>,
    ) -> Self {
        self.privacy = Some(crate::text_completion::AuxiliaryPrivacy::from_settings(
            settings,
        ));
        self
    }

    /// Set the role model spec children fall back to when their agent
    /// definition has no `model:` pin (task role, else smol).
    #[must_use]
    pub fn with_role_model_spec(mut self, spec: Option<String>) -> Self {
        self.role_model_spec = spec.filter(|s| !s.trim().is_empty());
        self.role_model_role = None;
        self
    }

    /// Preserve the selected task/smol role's recovery policy. An unlabeled
    /// explicit model spec never acquires role ownership by matching its ID.
    #[must_use]
    pub fn with_role_model_selection(
        mut self,
        selection: Option<(crate::models::ModelRole, String)>,
    ) -> Self {
        let selection = selection.filter(|(_, spec)| !spec.trim().is_empty());
        self.role_model_role = selection.as_ref().map(|(role, _)| *role);
        self.role_model_spec = selection.map(|(_, spec)| spec);
        self
    }

    /// Retain the host's already trust-filtered provider policy. Standalone
    /// callers without a snapshot use global settings, never untrusted project
    /// settings, when admitting an explicit model override.
    #[must_use]
    pub fn with_model_config(mut self, config: Option<Config>) -> Self {
        self.model_config = config;
        self
    }

    /// Bind the owning agent's live provider/model. Each delegation captures
    /// it once; definition pins and task/smol roles retain precedence.
    #[must_use]
    pub fn with_model_scope(mut self, scope: ToolModelScope) -> Self {
        self.bind_model_scope(scope);
        self
    }

    /// Bind the owning session's live cost gate. Background runs capture it
    /// before launch, so switching the parent's session cannot reset them.
    #[must_use]
    pub fn with_cost_scope(mut self, scope: crate::cost::CostScope) -> Self {
        self.cost_scope = scope;
        self
    }

    /// Explicit shared budget for standalone SDK delegation.
    #[must_use]
    pub fn with_cost_budget(self, budget: Option<crate::cost::CostBudget>) -> Self {
        let scope = crate::cost::CostScope::default();
        scope.set(budget);
        self.with_cost_scope(scope)
    }

    /// Freeze the parent's identity before dispatching background work such
    /// as `/tan`. This carries no credentials; children use their ordinary
    /// inherited authentication environment and configured credential stores.
    #[must_use]
    pub fn with_parent_model(mut self, model: Option<ToolModelSelection>) -> Self {
        self.model_scope = None;
        self.parent_model = model;
        self
    }

    fn capture_parent_model(&self) -> Option<ToolModelSelection> {
        self.model_scope
            .as_ref()
            .map_or_else(|| self.parent_model.clone(), ToolModelScope::current)
    }

    /// Set the host's request-wide execution ceiling (1 ms through 24 hours).
    /// Without this override, `PI_SUBAGENT_TIMEOUT_SECS` supplies the ceiling,
    /// defaulting to 900 seconds. A model's `timeoutSeconds` can only shorten it.
    /// Invalid limits are rejected before launch. Inherited parent deadlines
    /// remain an upper bound even when this explicit SDK policy is supplied.
    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Opt in to appending the machine-readable
    /// `<subagent-structured-result>` JSON block to the tool result text.
    ///
    /// Off by default; when disabled the tool output is byte-identical to
    /// previous releases. See pi_agent_rust#163.
    #[must_use]
    pub const fn with_structured_results(mut self, enabled: bool) -> Self {
        self.structured_results = enabled;
        self
    }

    /// Run one built-in tangential child in the current working directory.
    ///
    /// This uses the same process, task-role, cancellation, transcript, and
    /// hub machinery as the opt-in `subagent` tool while avoiding a required
    /// user-authored agent definition for the `/tan` host command.
    pub async fn run_background_tan(&self, task: &str) -> Result<TanCompletion> {
        if current_subagent_depth() >= MAX_SUBAGENT_DEPTH {
            return Err(Error::tool(
                "subagent",
                format!(
                    "Refusing nested subagent depth above {MAX_SUBAGENT_DEPTH}; child agents are isolated by default and do not receive the subagent tool."
                ),
            ));
        }
        let task = task.trim();
        if task.is_empty() {
            return Err(Error::validation("/tan requires non-empty work"));
        }

        let deadline = Deadline::for_request(self.timeout, None)?;
        let definition = tan_agent_definition();
        let agents = BTreeMap::from([(TAN_AGENT_NAME.to_string(), definition)]);
        let mut request = SubagentTask {
            agent: TAN_AGENT_NAME.to_string(),
            task: task.to_string(),
            task_file: None,
            task_file_sha256: None,
            model: None,
            model_selection: None,
            fallback_policy: None,
            cwd: Some(self.cwd.clone()),
            isolation: None,
            iso_apply: None,
            output_schema: None,
            schema_mode: SchemaMode::Permissive,
        };
        model_selection::prepare(self, &agents, std::iter::once(&mut request), deadline)?;
        let result = ChildRunner::new(
            self.cwd.clone(),
            self.global_dir.clone(),
            self.child_binary.clone(),
            self.role_model_spec.clone(),
            crate::agent_hub::ChildKind::Tan,
            deadline,
        )
        .into_background()
        .with_parent_model(self.capture_parent_model())
        .with_cost_budget(self.cost_scope.current())
        .run_one(&agents, request, None, None)
        .await;
        Ok(TanCompletion::from_result(result))
    }

    /// Construct with explicit discovery and child-runtime paths.
    ///
    /// This seam is intentionally narrow: embedders and hermetic conformance
    /// tests can exercise the real child protocol without consulting the
    /// process-global agent directory or current executable.
    #[doc(hidden)]
    #[must_use]
    pub fn with_paths(cwd: PathBuf, global_dir: PathBuf, child_binary: PathBuf) -> Self {
        Self {
            cwd,
            global_dir,
            project_trusted: false,
            workspace_trust_guard: None,
            workspace: crate::workspace::WorkspaceHandle::default(),
            child_binary,
            structured_results: false,
            role_model_spec: None,
            role_model_role: None,
            model_config: None,
            model_scope: None,
            cost_scope: crate::cost::CostScope::default(),
            parent_model: None,
            timeout: None,
            privacy: None,
            job_session_scope: crate::jobs::JobSessionScope::default(),
        }
    }

    fn discover(&self, scope: AgentScope) -> Result<BTreeMap<String, AgentDefinition>> {
        let project_trusted = if matches!(scope, AgentScope::User) {
            // User-only delegation never consumes project profiles and stays
            // usable when an unrelated project trust surface has changed.
            false
        } else if let Some(guard) = &self.workspace_trust_guard {
            guard.validate_workspace(&self.cwd)?;
            guard.trusted()
        } else {
            self.project_trusted
        };
        discover_agents_with_roots(&self.cwd, &self.global_dir, scope, project_trusted)
    }

    async fn run_request(
        &self,
        request: SubagentRequest,
        on_update: Option<UpdateCallback>,
    ) -> Result<Vec<SubagentResult>> {
        Ok(self.prepare_request(&request)?.run(on_update).await)
    }

    /// Freeze discovery, model affinity and the original deadline before a
    /// caller transfers execution into a managed background job.
    fn prepare_request(&self, request: &SubagentRequest) -> Result<PreparedRequest> {
        // Capture once, before discovery and queueing. Every parallel task,
        // sequential step and schema-correction retry consumes this budget.
        let deadline = Deadline::for_request(self.timeout, request.timeout_seconds)?;
        self.prepare_request_with_deadline(request, deadline)
    }

    fn prepare_request_with_deadline(
        &self,
        request: &SubagentRequest,
        deadline: Deadline,
    ) -> Result<PreparedRequest> {
        let parent_model = self.capture_parent_model();
        let mut mode = request.mode()?;
        let agents = self.discover(request.scope)?;
        match &mut mode {
            RequestMode::Single(task) => {
                task_file::prepare(self, std::iter::once(&mut *task), deadline)?;
                model_selection::prepare(self, &agents, std::iter::once(task), deadline)?;
            }
            RequestMode::Parallel(tasks) | RequestMode::Chain(tasks) => {
                task_file::prepare(self, tasks.iter_mut(), deadline)?;
                model_selection::prepare(self, &agents, tasks.iter_mut(), deadline)?;
            }
        }
        let concurrency = request
            .concurrency
            .unwrap_or(DEFAULT_CONCURRENCY)
            .clamp(1, MAX_PARALLEL_TASKS);
        let tasks = match &mode {
            RequestMode::Single(task) => std::slice::from_ref(task),
            RequestMode::Parallel(tasks) | RequestMode::Chain(tasks) => tasks.as_slice(),
        };
        let launch_admission = task_file::admission(self, tasks);
        Ok(PreparedRequest {
            runner: ChildRunner::new(
                self.cwd.clone(),
                self.global_dir.clone(),
                self.child_binary.clone(),
                self.role_model_spec.clone(),
                crate::agent_hub::ChildKind::Subagent,
                deadline,
            )
            .with_parent_model(parent_model)
            .with_cost_budget(self.cost_scope.current())
            .with_launch_admission(launch_admission),
            agents,
            mode,
            concurrency,
            deadline,
        })
    }
}

/// An admitted delegation contains no borrowed Agent, mutable tool registry or
/// caller callback. Queued work cannot rediscover a changed agent definition or
/// silently adopt a new model after the caller receives its job handle.
struct PreparedRequest {
    runner: ChildRunner,
    agents: BTreeMap<String, AgentDefinition>,
    mode: RequestMode,
    concurrency: usize,
    deadline: Deadline,
}

impl PreparedRequest {
    async fn run(self, on_update: Option<UpdateCallback>) -> Vec<SubagentResult> {
        let Self {
            runner,
            agents,
            mode,
            concurrency,
            ..
        } = self;
        match mode {
            RequestMode::Single(task) => vec![runner.run_one(&agents, task, None, on_update).await],
            RequestMode::Parallel(tasks) => {
                runner
                    .run_parallel(&agents, tasks, concurrency, on_update)
                    .await
            }
            RequestMode::Chain(tasks) => {
                let mut previous: Option<SubagentResult> = None;
                let mut results = Vec::with_capacity(tasks.len());
                for (step, mut task) in tasks.into_iter().enumerate() {
                    if let Err(error) = task.render_previous_result(previous.as_ref()) {
                        let result = if let Some(agent) = agents.get(&task.agent) {
                            SubagentResult::failed(agent, task, Some(step + 1), error.to_string()) // ubs:ignore[rust.perf.string-alloc-in-loop] -- This terminal error is allocated once before the chain immediately breaks.
                        } else {
                            SubagentResult::unknown(task, Some(step + 1))
                        };
                        emit_progress(on_update.as_ref(), &result);
                        results.push(result);
                        break;
                    }
                    let result = runner
                        .run_one(&agents, task, Some(step + 1), on_update.clone())
                        .await;
                    let failed = result.is_error;
                    previous = Some(result.clone());
                    results.push(result);
                    if failed {
                        break;
                    }
                }
                results
            }
        }
    }
}

#[async_trait]
impl Tool for SubagentTool {
    fn name(&self) -> &'static str {
        "subagent"
    }

    fn label(&self) -> &'static str {
        "Subagent"
    }

    fn description(&self) -> &'static str {
        concat!(
            "Delegate an isolated task to a named Pi child agent. Supports one task, bounded parallel tasks, or a sequential chain whose tasks may reference {previous}. timeoutSeconds bounds the entire request, including queued tasks and retries, and cannot extend the host limit (900 seconds by default). Built-in agents: task (implementation), scout (read-only exploration), reviewer (read-only correctness review), security-reviewer (read-only security review), sonic (mechanical updates). User definitions in $PI_CODING_AGENT_DIR/agents/*.md override built-ins; the nearest .pi/agents/*.md definitions override both only when the host has admitted workspace trust. Scope never grants trust. All built-ins use the ordinary task-role/parent model selection and do not enable recursive delegation. Workspace isolation: single requests and each tasks/chain entry accept cwd, isolation and isoApply. `isolation: \"worktree\"` runs the child in a git worktree carrying the parent's uncommitted state, returning {worktree_path, diff_stat, patch} and applying per `isoApply` (keep|apply|drop; parallel tasks and their retries share a captured baseline, then accepted patches apply in task order after execution; conflicts reported never forced). Coordination: isolated worktree children need no file reservations by construction; NON-isolated children share the parent checkout, so concurrent edits to the same files should be coordinated (e.g. Agent Mail file reservations with reason=<task id>).",
            " Model selection: an optional model on one task or workflow call accepts an exact provider/model[:thinking], unambiguous catalog ID, or {role: canonicalName} explicitly assigned in trusted modelRoles settings, overriding definition/task-role/parent defaults. An unconfigured role never falls back to another role or default model. Every explicit selector is checked before any child or worktree is created; unknown, unavailable, disabled or unsupported destinations fail without fallback. Put model on each tasks/chain entry, not the batch. worker_start and pool_start freeze the resolved destination and role-specific fallback policy for their entire lifetime; send/push cannot switch models. Seance accepts only exact-model strings.",
            " File instructions: ordinary single/tasks/chain requests and workflow calls accept taskFile instead of task. Supply an absolute read-admitted workspace/agent file or local:// scratch document, containing nonempty UTF-8 instructions of at most 4 MiB (workflow tasks retain their 64 KiB expanded-task limit). Every source is captured before any child starts or a background job is acknowledged; results retain taskFile and taskFileSha256 for the original bytes. Queued tasks, schema retries and revival use the captured instructions. File content never changes the selected agent, model, tools, workspace or approval policy. Seance and retained workers/pools do not accept taskFile.",
            " Historical consultation: use a single request with agent: \"seance\", sourceSession: an explicit saved v3 JSONL path, and task: your question. The fixed consultant receives the active branch's complete bounded, privacy-protected text, no tools/extensions/MCP/current-workspace context, and the saved branch model/effort unless model explicitly overrides it. It never resumes or writes the source. Large snapshots refuse instead of truncating. seance cannot be overridden by agent files or used with cwd, isolation, scope, batches or workflows.",
            " workflow is an alternative dependency graph of up to 32 named nodes with native call objects and needs IDs. Ready nodes execute in parallel waves, with accepted worktree writeback settled before dependent waves start. Failed or schema-invalid predecessors skip descendants; independent branches continue. One original deadline covers the graph. Inspect each ordered node's state, native result and hubId; this is not graph-wide rollback.",
            " Set async: true on a single, parallel tasks, chain, or workflow request to return a session-owned background job immediately. Use jobs list/wait/cancel with its jobId; a settled wait includes the native subagent or workflow result. Completion also arrives through the normal background follow-up queue. Definitions, model and request deadline freeze before acknowledgment; queued work does not get a fresh timeout. Background delegation does not retain an eval cell or an Agent callback. Seance remains foreground.",
            " Reusable workers: operation worker_start launches a persistent RPC child with agent/task; worker_send queues another task on its retained conversation. worker_status/worker_wait select workerId and optional turnId; worker_close cancels the entire worker and its queued turns. Each worker has one fixed lifetime deadline and at most 32 total turns; wait timeout does not cancel work. Workers and pools support Unix and Windows with isolation none. outputSchema on worker_start or pool_start (or the agent's output_schema) freezes one contract for every turn: each final answer must be bare JSON that validates, returned as data, with no corrective retry; schemaMode there may only be strict. Their jobId owns the whole process lifetime; use worker_wait for individual turn results.",
            " Native pools: pool_start freezes one agent and up to 8 reusable children under one session-owned job, with optional tasks as a string array. pool_push atomically appends a nonempty task batch, up to 32 items total. Idle workers with lower observed context pressure receive new work first; busy workers retain balanced FIFO queues. pool_finish seals admission and drains all accepted work before reaping children and delivering one ordered aggregate completion. pool_status returns bounded item summaries and result references; pool_result and pool_wait select poolId/itemId for full results. pool_wait without itemId observes aggregate completion without sealing. pool_drain returns up to 4 settled full results with an explicit replayable cursor, without waiting for earlier items, sealing admission or consuming results. pool_close cancels all unfinished work. Pool identities and results survive eval kernel resets; original deadlines and session ownership remain fixed. Failed assignments are never replayed or transferred to replacement children."
        )
    }

    fn parameters(&self) -> Value {
        workflow::schema(json!({
            "type": "object",
            "properties": {
                "operation": {"type": "string", "enum": ["worker_start", "worker_send", "worker_status", "worker_wait", "worker_close", "pool_start", "pool_push", "pool_finish", "pool_status", "pool_result", "pool_wait", "pool_drain", "pool_close"], "description": "Persistent RPC worker or aggregate pool control. Omit for ordinary one-shot delegation. Each operation rejects unrelated fields."},
                "workerId": {"type": "string", "minLength": 1, "description": "Stable session-owned worker identity returned by worker_start."},
                "poolId": {"type": "string", "minLength": 1, "description": "Stable session-owned pool identity returned by pool_start."},
                "itemId": {"type": "integer", "minimum": 1, "maximum": 32, "description": "pool_result requires an admitted item ID; pool_wait optionally selects one item instead of the whole pool."},
                "turnId": {"type": "integer", "minimum": 1, "maximum": 32, "description": "worker_status/worker_wait: select a specific admitted turn; defaults to the latest."},
                "timeoutMs": {"type": "integer", "minimum": 0, "maximum": 600_000, "description": "worker_wait/pool_wait: bounded observation time, default 30000. pool_drain defaults to zero. Zero polls; expiration leaves accepted work running."},
                "cursor": {"type":"object","additionalProperties":false,"required":["schema","poolId","itemIds"],"properties":{"schema":{"const":"pi.subagent.pool.cursor.v1"},"poolId":{"type":"string","minLength":1},"itemIds":{"type":"array","maxItems":32,"uniqueItems":true,"items":{"type":"integer","minimum":1,"maximum":32}}},"description":"pool_drain only: caller-owned acknowledgment of already processed terminal items, sorted by item ID. Omit for an empty cursor; replaying a cursor does not consume results."},
                "maxItems": {"type":"integer","minimum":1,"maximum":4,"description":"pool_drain only: maximum full results in one page, default 4."},
                "agent": {"type": "string", "description": "Named agent for a single delegation."},
                "task": {"type": "string", "description": "Inline instructions for a single delegation; mutually exclusive with taskFile."},
                "taskFile": {"type": "string", "minLength": 1, "maxLength": 4096, "description": "Alternative to task: an absolute UTF-8 file path inside the read-admitted workspace/agent roots, or a local:// scratch document. Snapshot once before launch, at most 4 MiB; the submitted reference and SHA-256 of its original bytes are returned. Single, tasks, chain and workflow calls only; workflows retain a 64 KiB expanded-task limit."},
                "async": {"type": "boolean", "default": false, "description": "Return a managed background job for a single, tasks, chain, or workflow request. jobs wait returns its native result; cancellation and session shutdown terminate unfinished children. Not supported with seance."},
                "sourceSession": {"type": "string", "minLength": 1, "description": "Required only for agent: seance. Explicit v3 JSONL file path, absolute or relative to the host workspace, admitted only within the workspace, live added roots, or configured agent data directory. The source is inspected read only; IDs, discovery and SQLite sessions are not supported."},
                "model": model_selection::schema(),
                "cwd": {"type": "string", "minLength": 1, "description": "Single delegation working directory, absolute or relative to the host workspace. For tasks/chain, put this on each entry."},
                "isolation": {"type": "string", "enum": ["none", "worktree"], "default": "none", "description": "Single delegation isolation. worktree uses the existing captured-baseline and acceptance gates; non-Git directories refuse rather than falling back."},
                "isoApply": {"type": "string", "enum": ["keep", "apply", "drop"], "description": "Single delegation worktree disposition, requiring isolation: worktree. Defaults to apply; keep preserves it for inspection, drop reports the patch without writeback."},
                "outputSchema": {"type": "object", "description": "JSON Schema the single delegation's final output must match; the parent validates and returns parsed data."},
                "schemaMode": {"type": "string", "enum": ["permissive", "strict"], "default": "permissive", "description": "permissive keeps an invalid result with a warning; strict fails the task. worker_start and pool_start are always strict and accept only strict."},
                "tasks": {"anyOf": [{"type": "array", "maxItems": MAX_PARALLEL_TASKS, "items": {"$ref": "#/definitions/task"}}, {"type": "array", "maxItems": 32, "items": {"type": "string", "minLength": 1}}], "description": "Ordinary delegation: up to 8 task objects. pool_start/pool_push: string assignments for the pool's frozen agent, up to 32 items over the entire pool lifetime."},
                "chain": {"type": "array", "maxItems": MAX_PARALLEL_TASKS, "items": {"$ref": "#/definitions/task"}, "description": "Sequential tasks; {previous} is replaced with the prior child output, and {{previous.data.<field.path>}} addresses the prior task's schema-validated data."},
                "concurrency": {"type": "integer", "minimum": 1, "maximum": MAX_PARALLEL_TASKS},
                "timeoutSeconds": {"type": "integer", "minimum": 1, "maximum": deadline::MAX_TIMEOUT_SECS, "description": "Budget for this whole delegation request, including queueing, chained steps and corrective retries. May shorten, never extend, the host's limit."},
                "scope": {"type": "string", "enum": ["both", "user", "project"], "default": "both", "description": "Select authored profile sources. Project profiles require host-established workspace trust; scope cannot grant it. Built-ins are always available. User scope never reads project profiles."}
            },
            "definitions": {
                "task": {
                    "type": "object",
                    "required": ["agent"],
                    "oneOf": [
                        {"required": ["task"], "not": {"required": ["taskFile"]}},
                        {"required": ["taskFile"], "not": {"required": ["task"]}}
                    ],
                    "properties": {
                        "agent": {"type": "string"},
                        "task": {"type": "string"},
                        "taskFile": {"type": "string", "minLength": 1, "maxLength": 4096, "description": "Absolute read-admitted file or local:// scratch document, mutually exclusive with task. Captured before any child starts; chain placeholders apply to the captured instructions."},
                        "model": model_selection::schema(),
                        "cwd": {"type": "string", "minLength": 1},
                        "isolation": {"type": "string", "enum": ["none", "worktree"], "default": "none", "description": "worktree runs the child in a git worktree with the parent's uncommitted state; non-git dirs refuse with PI_ISO_NOT_GIT."},
                        "isoApply": {"type": "string", "enum": ["keep", "apply", "drop"], "default": "apply", "description": "What to do with the isolated worktree after completion; requires isolation: worktree."},
                        "outputSchema": {"type": "object", "description": "JSON Schema this task's final output must match."},
                        "schemaMode": {"type": "string", "enum": ["permissive", "strict"], "default": "permissive"}
                    }
                }
            },
            "additionalProperties": false
        }))
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        input: Value,
        on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let privacy = self
            .privacy
            .clone()
            .unwrap_or_else(|| crate::text_completion::AuxiliaryPrivacy::from_settings(None));
        if worker::requested(&input)? {
            return worker::execute(
                self,
                input,
                crate::text_completion::AuxiliaryPrivacySnapshot::from_privacy(privacy),
            )
            .await;
        }
        if background::requested(&input)? {
            return background::start(
                self,
                input,
                crate::text_completion::AuxiliaryPrivacySnapshot::from_privacy(privacy),
            )
            .await;
        }
        let protect = |parts: &[&str]| crate::text_completion::redact_inputs(parts, &privacy);
        self.execute_with_auxiliary_privacy(tool_call_id, input, on_update, &protect)
            .await
    }

    async fn execute_with_invocation(
        &self,
        tool_call_id: &str,
        input: Value,
        on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
        context: ToolInvocationContext<'_>,
    ) -> Result<ToolOutput> {
        if worker::requested(&input)? {
            return worker::execute(self, input, (context.snapshot_privacy)()).await;
        }
        if background::requested(&input)? {
            return background::start(self, input, (context.snapshot_privacy)()).await;
        }
        self.execute_with_auxiliary_privacy(tool_call_id, input, on_update, context.protect)
            .await
    }

    async fn execute_with_auxiliary_privacy(
        &self,
        _tool_call_id: &str,
        mut input: Value,
        on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
        protect: &(dyn for<'a, 'b> Fn(&'a [&'b str]) -> Result<Vec<String>> + Send + Sync),
    ) -> Result<ToolOutput> {
        if worker::requested(&input)? {
            return Err(Error::tool(
                "subagent",
                "PI_SUBAGENT_WORKER_PRIVACY: worker operations require an owned invocation privacy snapshot; use execute_with_invocation or standalone execute",
            ));
        }
        if background::requested(&input)? {
            return Err(Error::tool(
                "subagent",
                "PI_SUBAGENT_BACKGROUND_PRIVACY: background execution requires an owned invocation privacy snapshot; use execute_with_invocation or the standalone execute entry point",
            ));
        }
        if let Some(fields) = input.as_object_mut() {
            fields.remove("async");
        }
        if current_subagent_depth() >= MAX_SUBAGENT_DEPTH {
            return Err(Error::tool(
                "subagent",
                format!(
                    "Refusing nested subagent depth above {MAX_SUBAGENT_DEPTH}; child agents are isolated by default and do not receive the subagent tool."
                ),
            ));
        }
        // An explicit null/string must not silently remove a requested limit
        // when an SDK caller bypasses schema validation.
        if input
            .get("timeoutSeconds")
            .is_some_and(|value| value.as_u64().is_none())
        {
            return Err(Error::tool(
                "subagent",
                "PI_SUBAGENT_INVALID_TIMEOUT: timeoutSeconds must be an integer from 1 to 86400",
            ));
        }
        if seance::is_request(&input)? {
            if input.get("model").is_some_and(|model| !model.is_string()) {
                return Err(Error::tool(
                    "subagent",
                    "PI_SEANCE_INVALID: seance model must be an exact-model string; role selections are not supported",
                ));
            }
            return seance::execute(self, input, on_update.map(Arc::from), protect).await;
        }
        if let Some(plan) = workflow::Plan::parse(&input)? {
            return plan.execute(self, on_update.map(Arc::from)).await;
        }
        workspace::validate_request(&input)?;
        model_selection::validate_request(&input)?;
        task_file::validate_request(&input)?;
        let request: SubagentRequest = serde_json::from_value(input)
            .map_err(|error| Error::tool("subagent", format!("Invalid input: {error}")))?;
        let update = on_update.map(Arc::from);
        let mode = request.mode_name()?;
        let results = self.run_request(request, update).await?;
        Ok(result_output(&results, mode, self.structured_results))
    }

    fn effects(&self) -> ToolEffects {
        ToolEffects::process()
    }

    fn bind_model_scope(&mut self, scope: ToolModelScope) {
        self.model_scope = Some(scope);
        self.parent_model = None;
    }

    fn bind_cost_scope(&mut self, scope: crate::cost::CostScope) {
        self.cost_scope = scope;
    }

    fn bind_job_session_scope(&mut self, scope: crate::jobs::JobSessionScope) {
        self.job_session_scope = scope;
    }
}

fn result_output(results: &[SubagentResult], mode: &str, structured_results: bool) -> ToolOutput {
    let is_error = results.iter().any(|result| result.is_error);
    let mut content = render_results(results);
    if structured_results {
        content.push_str("\n\n");
        content.push_str(&structured_result_block(results));
    }
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(content))],
        details: Some(json!({
            "schema": SUBAGENT_RESULT_SCHEMA,
            "mode": mode,
            "sessionIsolation": "ephemeral_no_session",
            "results": results,
        })),
        is_error,
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubagentRequest {
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    task: Option<String>,
    #[serde(default)]
    task_file: Option<String>,
    #[serde(default)]
    model: Option<model_selection::Selector>,
    /// Single-delegation workspace controls use the same runner as array entries.
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    isolation: Option<String>,
    #[serde(default)]
    iso_apply: Option<String>,
    /// Single-delegation form of the per-task `outputSchema` (bd-cv653.5.1).
    #[serde(default)]
    output_schema: Option<Value>,
    #[serde(default)]
    schema_mode: SchemaMode,
    #[serde(default)]
    tasks: Option<Vec<SubagentTask>>,
    #[serde(default)]
    chain: Option<Vec<SubagentTask>>,
    #[serde(default)]
    concurrency: Option<usize>,
    #[serde(default)]
    timeout_seconds: Option<u64>,
    #[serde(default)]
    scope: AgentScope,
}

impl SubagentRequest {
    fn mode(&self) -> Result<RequestMode> {
        if self.task.is_some() && self.task_file.is_some() {
            return Err(Error::tool(
                "subagent",
                "PI_SUBAGENT_TASK_FILE: provide task or taskFile, never both",
            ));
        }
        if (self.tasks.is_some() || self.chain.is_some()) && self.task_file.is_some() {
            return Err(Error::tool(
                "subagent",
                "PI_SUBAGENT_TASK_FILE: taskFile belongs on each tasks/chain entry, not the batch",
            ));
        }
        if (self.tasks.is_some() || self.chain.is_some()) && self.model.is_some() {
            return Err(Error::tool(
                "subagent",
                "PI_SUBAGENT_MODEL: model belongs on each tasks/chain entry, not the batch",
            ));
        }
        if (self.tasks.is_some() || self.chain.is_some())
            && (self.cwd.is_some() || self.isolation.is_some() || self.iso_apply.is_some())
        {
            return Err(Error::tool(
                "subagent",
                "PI_SUBAGENT_WORKSPACE_SCOPE: cwd, isolation and isoApply belong on each tasks/chain entry, not the batch",
            ));
        }
        let single = self
            .agent
            .as_ref()
            .filter(|_| self.task.is_some() || self.task_file.is_some())
            .map(|agent| SubagentTask {
                agent: agent.clone(),
                task: self.task.clone().unwrap_or_default(),
                task_file: self.task_file.clone(),
                task_file_sha256: None,
                model: self.model.clone(),
                model_selection: None,
                fallback_policy: None,
                cwd: self.cwd.clone(),
                isolation: self.isolation.clone(),
                iso_apply: self.iso_apply.clone(),
                output_schema: self.output_schema.clone(),
                schema_mode: self.schema_mode,
            });
        let selected = usize::from(single.is_some())
            + usize::from(self.tasks.is_some())
            + usize::from(self.chain.is_some());
        if selected.ne(&1) {
            return Err(Error::tool(
                "subagent",
                "Provide exactly one of agent+(task or taskFile), tasks, or chain.",
            ));
        }
        if self.agent.is_some() != (self.task.is_some() || self.task_file.is_some()) {
            return Err(Error::tool(
                "subagent",
                "Single delegation requires agent and either task or taskFile.",
            ));
        }
        if let Some(tasks) = &self.tasks
            && (tasks.is_empty() || tasks.len() > MAX_PARALLEL_TASKS)
        {
            return Err(Error::tool(
                "subagent",
                format!("tasks must contain 1-{MAX_PARALLEL_TASKS} entries."),
            ));
        }
        if let Some(chain) = &self.chain
            && (chain.is_empty() || chain.len() > MAX_PARALLEL_TASKS)
        {
            return Err(Error::tool(
                "subagent",
                format!("chain must contain 1-{MAX_PARALLEL_TASKS} entries."),
            ));
        }
        Ok(single.map_or_else(
            || {
                self.tasks.as_ref().map_or_else(
                    || RequestMode::Chain(self.chain.clone().unwrap_or_default()),
                    |tasks| RequestMode::Parallel(tasks.clone()),
                )
            },
            RequestMode::Single,
        ))
    }

    fn mode_name(&self) -> Result<&'static str> {
        match self.mode()? {
            RequestMode::Single(_) => Ok("single"),
            RequestMode::Parallel(_) => Ok("parallel"),
            RequestMode::Chain(_) => Ok("chain"),
        }
    }
}

enum RequestMode {
    Single(SubagentTask),
    Parallel(Vec<SubagentTask>),
    Chain(Vec<SubagentTask>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubagentTask {
    agent: String,
    #[serde(default)]
    task: String,
    #[serde(default)]
    task_file: Option<String>,
    /// Computed from captured source bytes; never accepted from caller JSON.
    #[serde(skip)]
    task_file_sha256: Option<String>,
    #[serde(default)]
    model: Option<model_selection::Selector>,
    /// Admission resolves the public selector once. Retries, queues and hub
    /// revival retain this identity; callers cannot inject a resolved target.
    #[serde(skip)]
    model_selection: Option<model_selection::Selection>,
    /// Bounded, credential-free launch snapshot; never accepted from callers.
    #[serde(skip)]
    fallback_policy: Option<String>,
    #[serde(default)]
    cwd: Option<PathBuf>,
    /// Workspace isolation: `none` (default) or `worktree` (bd-cv653.5.2).
    #[serde(default)]
    isolation: Option<String>,
    /// What to do with an isolated worktree after completion: `keep`,
    /// `apply` (default), or `drop`.
    #[serde(default)]
    iso_apply: Option<String>,
    /// JSON Schema the child's final output must match (bd-cv653.5.1).
    /// Overrides the agent definition's `output_schema` when both are set.
    #[serde(default)]
    output_schema: Option<Value>,
    #[serde(default)]
    schema_mode: SchemaMode,
}

/// How a schema-validation failure that survives the corrective retry is
/// treated (bd-cv653.5.1).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum SchemaMode {
    /// Accept the invalid result, exposing `schema_valid: false` plus
    /// `validation_errors` as a warning.
    #[default]
    Permissive,
    /// Fail the task with the validation errors.
    Strict,
}

impl SubagentTask {
    fn display_task(&self) -> &str {
        self.task_file.as_deref().unwrap_or(&self.task)
    }

    /// Render `{previous}` (raw prior output — historical contract) and, when
    /// the prior result is schema-valid, `{{previous.data.<dotted.path>}}`
    /// field references (bd-cv653.5.1). Unresolvable field references are
    /// left verbatim so mistakes stay visible instead of silently vanishing.
    fn render_previous_result(
        &mut self,
        previous: Option<&SubagentResult>,
    ) -> std::result::Result<(), &'static str> {
        let raw = previous.map_or("", |result| result.output.as_str());
        let marker = concat!("{", "previous", "}");
        let limit = if self.task_file.is_some() {
            task_file::MAX_BYTES
        } else {
            usize::MAX
        };
        // Count before replace allocates: a small repeated token can otherwise
        // amplify a bounded file and bounded predecessor into gigabytes.
        let count = self.task.matches(marker).count();
        let unchanged = self.task.len() - count * marker.len();
        if count
            .checked_mul(raw.len())
            .and_then(|bytes| bytes.checked_add(unchanged))
            .is_none_or(|bytes| bytes > limit)
        {
            return Err(CHAIN_TASK_LIMIT);
        }
        let mut rendered = self.task.replace(marker, raw);
        if let Some(data) = previous
            .filter(|result| result.schema_valid == Some(true))
            .and_then(|result| result.data.as_ref())
        {
            rendered = render_previous_data_fields(&rendered, data, limit)?;
        }
        self.task = rendered;
        Ok(())
    }
}

const CHAIN_TASK_LIMIT: &str =
    "PI_SUBAGENT_TASK_FILE_LIMIT: expanded taskFile chain instructions exceed 4 MiB";

fn push_chain_part(
    target: &mut String,
    part: &str,
    limit: usize,
) -> std::result::Result<(), &'static str> {
    if part.len() > limit.saturating_sub(target.len()) {
        return Err(CHAIN_TASK_LIMIT);
    }
    target.push_str(part);
    Ok(())
}

/// Replace `{{previous.data.<path>}}` tokens with values addressed out of the
/// prior task's parsed `data`. Scalars render bare; objects/arrays render as
/// compact JSON. Missing paths leave the token untouched.
fn render_previous_data_fields(
    task: &str,
    data: &Value,
    limit: usize,
) -> std::result::Result<String, &'static str> {
    const OPEN: &str = "{{previous.data.";
    const CLOSE: &str = "}}";
    let mut rendered = String::with_capacity(task.len());
    let mut rest = task;
    while let Some(start) = rest.find(OPEN) {
        push_chain_part(&mut rendered, &rest[..start], limit)?;
        let after_open = &rest[start + OPEN.len()..];
        let Some(end) = after_open.find(CLOSE) else {
            push_chain_part(&mut rendered, &rest[start..], limit)?;
            return Ok(rendered);
        };
        let path = &after_open[..end];
        let pointer = format!("/{}", path.replace('.', "/"));
        match data.pointer(&pointer) {
            Some(Value::String(text)) => push_chain_part(&mut rendered, text, limit)?,
            Some(Value::Null) | None => {
                // Leave the token verbatim so the miss is visible.
                push_chain_part(
                    &mut rendered,
                    &rest[start..start + OPEN.len() + end + CLOSE.len()],
                    limit,
                )?;
            }
            Some(value) => push_chain_part(&mut rendered, &value.to_string(), limit)?, // ubs:ignore[rust.perf.string-alloc-in-loop] -- Each requested typed substitution serializes a predecessor-bounded value before enforcing the total prompt byte limit.
        }
        rest = &after_open[end + CLOSE.len()..];
    }
    push_chain_part(&mut rendered, rest, limit)?;
    Ok(rendered)
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum AgentScope {
    User,
    Project,
    #[default]
    Both,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum AgentSource {
    BuiltIn,
    User,
    Project,
}

#[derive(Debug, Clone)]
struct AgentDefinition {
    name: String,
    description: String,
    model: Option<String>,
    reasoning: Option<String>,
    tools: Option<Vec<String>>,
    skills: Vec<PathBuf>,
    system_prompt: String,
    /// Default JSON Schema for the child's final output, from the definition's
    /// `output_schema:` (or `output:`) frontmatter field. A task's
    /// `outputSchema` overrides it.
    output_schema: Option<Value>,
    source: AgentSource,
    file_path: PathBuf,
}

fn tan_agent_definition() -> AgentDefinition {
    AgentDefinition {
        name: TAN_AGENT_NAME.to_string(),
        description: "Background tangential coding agent".to_string(),
        model: None,
        reasoning: None,
        tools: None,
        skills: Vec::new(),
        system_prompt: TAN_SYSTEM_PROMPT.to_string(),
        output_schema: None,
        source: AgentSource::BuiltIn,
        file_path: PathBuf::from("<built-in:tan>"),
    }
}

fn discover_agents_with_roots(
    cwd: &Path,
    global_dir: &Path,
    scope: AgentScope,
    project_trusted: bool,
) -> Result<BTreeMap<String, AgentDefinition>> {
    // Scope selects authored overrides; bundled definitions are always available.
    let mut agents = bundled::definitions();
    if !matches!(scope, AgentScope::Project) {
        load_agent_dir(&global_dir.join("agents"), AgentSource::User, &mut agents)?;
    }
    if project_trusted
        && !matches!(scope, AgentScope::User)
        && let Some(project_dir) = crate::workspace_trust::nearest_project_agents_dir(cwd)
    {
        // Project definitions intentionally replace user definitions of the same name.
        load_agent_dir(&project_dir, AgentSource::Project, &mut agents)?;
    }
    Ok(agents)
}

fn load_agent_dir(
    directory: &Path,
    source: AgentSource,
    agents: &mut BTreeMap<String, AgentDefinition>,
) -> Result<()> {
    if !directory.exists() {
        return Ok(());
    }
    let entries = std::fs::read_dir(directory).map_err(|error| {
        Error::tool(
            "subagent",
            format!(
                "Cannot read agent directory {}: {error}",
                directory.display()
            ),
        )
    })?;
    let mut paths = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|error| {
                Error::tool(
                    "subagent",
                    format!("Cannot enumerate agent definitions: {error}"),
                )
            })?
            .path();
        if path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
        {
            if paths.len() >= definition::MAX_DEFINITIONS {
                return Err(Error::tool(
                    "subagent",
                    "PI_AGENT_DEFINITION_LIMIT: too many agent definitions in one directory",
                ));
            }
            paths.push(path);
        }
    }
    paths.sort();
    let mut names = std::collections::BTreeSet::new();
    for path in paths {
        let agent = definition::read(&path, source)?;
        if !names.insert(agent.name.clone()) {
            return Err(Error::tool(
                "subagent",
                "PI_AGENT_DEFINITION_DUPLICATE: two files declare the same agent name in one directory",
            ));
        }
        agents.insert(agent.name.clone(), agent);
    }
    Ok(())
}

fn split_csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn with_fallback_policy_args(args: Vec<OsString>, policy: Option<&str>) -> Vec<OsString> {
    let Some(policy) = policy else {
        return args;
    };
    let mut launch = vec!["--subagent-fallback-policy".into(), policy.into()];
    launch.extend(args);
    launch
}

fn child_args(
    agent: &AgentDefinition,
    task: &str,
    role_model_spec: Option<&str>,
    parent_model: Option<&ToolModelSelection>,
    selection: Option<&model_selection::Selection>,
    output_schema: Option<&Value>,
) -> Vec<OsString> {
    let mut args = vec!["--mode".into(), "json".into(), "--print".into()];
    args.extend(child_options(
        agent,
        role_model_spec,
        parent_model,
        selection,
        output_schema,
    ));
    args.push(format!("Task: {task}").into());
    args
}

fn worker_args(
    agent: &AgentDefinition,
    role_model_spec: Option<&str>,
    parent_model: Option<&ToolModelSelection>,
    selection: Option<&model_selection::Selection>,
) -> Vec<OsString> {
    let mut args = vec!["--mode".into(), "rpc".into()];
    args.extend(child_options(
        agent,
        role_model_spec,
        parent_model,
        selection,
        None,
    ));
    args
}

fn child_options(
    agent: &AgentDefinition,
    role_model_spec: Option<&str>,
    parent_model: Option<&ToolModelSelection>,
    selection: Option<&model_selection::Selection>,
    output_schema: Option<&Value>,
) -> Vec<OsString> {
    let mut args = vec![
        "--no-session".into(),
        "--no-implicit-tools".into(),
        "--tools".into(),
        agent
            .tools
            .as_ref()
            .map_or_else(|| DEFAULT_CHILD_TOOLS.to_string(), |tools| tools.join(","))
            .into(),
    ];
    if agent.tools.as_ref().is_some_and(Vec::is_empty) {
        args.push("--no-tools".into());
    }
    let consultant = seance::is_consultant(agent);
    if consultant {
        args.extend(
            [
                "--no-mcp",
                "--no-extensions",
                "--no-skills",
                "--no-context-files",
                "--no-prompt-templates",
                "--no-themes",
                "--no-migrations",
                "--hide-cwd-in-prompt",
            ]
            .into_iter()
            .map(OsString::from),
        );
    }
    // An admitted per-call identity wins over the definition, task/smol role,
    // captured parent and ambient configuration. Never reparse its model ID.
    if let Some(selection) = selection {
        args.extend([
            "--provider".into(),
            selection.provider.clone().into(),
            "--model".into(),
            selection.model.clone().into(),
        ]);
    } else if let Some(model) = &agent.model {
        args.extend(["--model".into(), model.clone().into()]);
    } else if let Some(spec) = role_model_spec {
        args.extend(["--model".into(), spec.into()]);
    } else if let Some(parent) = parent_model {
        // A parent's model ID is already resolved. Separate arguments preserve
        // slashes and suffixes instead of parsing it as a fresh model spec.
        args.extend([
            "--provider".into(),
            parent.provider.clone().into(),
            "--model".into(),
            parent.model.clone().into(),
        ]);
    }
    if let Some(thinking) = selection.and_then(|selection| selection.thinking) {
        args.extend(["--thinking".into(), thinking.to_string().into()]);
    } else if let Some(reasoning) = &agent.reasoning {
        args.extend(["--thinking".into(), reasoning.clone().into()]);
    }
    for skill in &agent.skills {
        args.extend(["--skill".into(), skill.clone().into_os_string()]);
    }
    // The schema directive rides the same appended system prompt as the
    // definition body (bd-cv653.5.1): one --append-system-prompt carrying
    // both keeps the child argv shape identical for schema-free tasks.
    let schema_directive = output_schema.map(|schema| {
        format!(
            "Your final answer MUST be a single JSON value matching this JSON Schema (no prose, no code fences):\n{schema}"
        )
    });
    let appended_prompt = match (agent.system_prompt.trim(), &schema_directive) {
        ("", None) => None,
        ("", Some(directive)) => Some(directive.clone()),
        (prompt, None) => Some(prompt.to_string()),
        (prompt, Some(directive)) => Some(format!("{prompt}\n\n{directive}")),
    };
    if let Some(prompt) = appended_prompt {
        // The historical consultant owns the entire prompt. Ordinary agents
        // continue appending their definitions to the host's context.
        let flag = if consultant {
            "--system-prompt"
        } else {
            "--append-system-prompt"
        };
        args.extend([flag.into(), prompt.into()]);
    }
    args
}

/// Compile an `outputSchema`, surfacing draft/keyword errors as strings.
fn compile_output_schema(schema: &Value) -> std::result::Result<jsonschema::Validator, String> {
    jsonschema::validator_for(schema).map_err(|error| error.to_string())
}

/// Validate a child's final output against `schema` (bd-cv653.5.1).
///
/// The output is located tolerantly before validation: the whole trimmed
/// text, else the payload of a ```json fence, else the first balanced
/// `{...}`/`[...]` region — models frequently wrap yields in prose despite
/// the directive. Returns the parsed value on success, or the collected
/// validation (or parse) errors.
fn validate_child_output(output: &str, schema: &Value) -> std::result::Result<Value, Vec<String>> {
    let validator = compile_output_schema(schema).map_err(|error| vec![error])?;
    let candidate = extract_json_candidate(output)
        .ok_or_else(|| vec!["child output contains no parseable JSON value".to_string()])?;
    let errors: Vec<String> = validator
        .iter_errors(&candidate)
        .map(|error| format!("{}: {error}", error.instance_path()))
        .collect();
    if errors.is_empty() {
        Ok(candidate)
    } else {
        Err(errors)
    }
}

/// Locate the JSON value in a child's final text output.
fn extract_json_candidate(output: &str) -> Option<Value> {
    let trimmed = output.trim();
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        return Some(value);
    }
    // ```json ... ``` (or bare ```) fenced payloads.
    for fence in ["```json", "```"] {
        if let Some(start) = trimmed.find(fence) {
            let after = &trimmed[start + fence.len()..];
            if let Some(end) = after.find("```")
                && let Ok(value) = serde_json::from_str::<Value>(after[..end].trim())
            {
                return Some(value);
            }
        }
    }
    // First balanced object/array region.
    for open in ['{', '['] {
        if let Some(start) = trimmed.find(open)
            && let Some(candidate) = balanced_json_region(&trimmed[start..])
            && let Ok(value) = serde_json::from_str::<Value>(candidate)
        {
            return Some(value);
        }
    }
    None
}

/// The shortest prefix of `text` (which starts at `{` or `[`) that closes the
/// opening bracket, honoring strings and escapes. `None` if never balanced.
fn balanced_json_region(text: &str) -> Option<&str> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (index, byte) in text.bytes().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        match byte {
            b'\\' if in_string => escaped = true,
            b'"' => in_string = !in_string,
            b'{' | b'[' if !in_string => depth += 1,
            b'}' | b']' if !in_string => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(&text[..=index]);
                }
            }
            _ => {}
        }
    }
    None
}

/// The corrective follow-up task for the single bounded retry. (`child_args`
/// adds the `Task:` prefix, so this must not.)
fn corrective_retry_task(original_task: &str, errors: &[String]) -> String {
    format!(
        "{original_task}\n\nYour previous output failed schema validation:\n{}\n\nReturn ONLY the corrected JSON value matching the required schema — no prose, no code fences.",
        errors.join("\n")
    )
}

fn child_depth() -> usize {
    current_subagent_depth().saturating_add(1)
}

fn current_subagent_depth() -> usize {
    std::env::var("PI_SUBAGENT_DEPTH")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or_default()
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
enum SubagentStatus {
    Starting,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl SubagentStatus {
    const fn as_str(&self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SubagentResult {
    agent: String,
    description: Option<String>,
    task: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    task_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    task_file_sha256: Option<String>,
    step: Option<usize>,
    source: Option<AgentSource>,
    definition_path: Option<PathBuf>,
    model: Option<String>,
    reasoning: Option<String>,
    tools: Vec<String>,
    cwd: PathBuf,
    binary: PathBuf,
    pid: Option<u32>,
    status: SubagentStatus,
    exit_code: Option<i32>,
    output: String,
    stderr: String,
    error: Option<String>,
    /// Parsed final output when an `outputSchema` validated it (bd-cv653.5.1).
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
    /// `Some(true)`/`Some(false)` when a schema applied; `None` otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    schema_valid: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    validation_errors: Option<Vec<String>>,
    /// Corrective retries consumed (bounded to one).
    #[serde(skip_serializing_if = "Option::is_none")]
    schema_retries: Option<usize>,
    /// Worktree-isolation outcome (bd-cv653.5.2) when the task ran isolated.
    #[serde(skip_serializing_if = "Option::is_none")]
    iso: Option<crate::worktree_iso::IsoOutcome>,
    /// Rejected attempts retained before a corrective retry. Their locations
    /// and patches remain inspectable even when the final attempt succeeds.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    preserved_worktrees: Vec<crate::worktree_iso::IsoOutcome>,
    session_isolation: &'static str,
    /// Agent-hub run id (bd-cv653.5.3) when the registry tracked this run.
    /// Serialized as `hubId`: callers revive, steer and correlate runs from
    /// `details.results[]`, so the documented hub reference must reach them.
    #[serde(skip_serializing_if = "Option::is_none")]
    hub_id: Option<String>,
    #[serde(skip)]
    is_error: bool,
}

impl SubagentResult {
    fn starting(
        agent: &AgentDefinition,
        task: SubagentTask,
        step: Option<usize>,
        binary: &Path,
        cwd: &Path,
        _args: &[OsString],
    ) -> Self {
        Self {
            agent: agent.name.clone(),
            description: Some(agent.description.clone()),
            task: task.display_task().to_string(),
            task_file: task.task_file,
            task_file_sha256: task.task_file_sha256,
            step,
            source: Some(agent.source),
            definition_path: Some(agent.file_path.clone()),
            model: agent.model.clone(),
            reasoning: agent.reasoning.clone(),
            tools: agent
                .tools
                .clone()
                .unwrap_or_else(|| split_csv(DEFAULT_CHILD_TOOLS)),
            cwd: cwd.to_path_buf(),
            binary: binary.to_path_buf(),
            pid: None,
            status: SubagentStatus::Starting,
            exit_code: None,
            output: String::new(),
            stderr: String::new(),
            error: None,
            data: None,
            schema_valid: None,
            validation_errors: None,
            schema_retries: None,
            iso: None,
            preserved_worktrees: Vec::new(),
            session_isolation: "ephemeral_no_session",
            hub_id: None,
            is_error: false,
        }
    }

    fn unknown(task: SubagentTask, step: Option<usize>) -> Self {
        Self {
            agent: task.agent.clone(),
            description: None,
            task: task.display_task().to_string(),
            task_file: task.task_file,
            task_file_sha256: task.task_file_sha256,
            step,
            source: None,
            definition_path: None,
            model: None,
            reasoning: None,
            tools: Vec::new(),
            cwd: task.cwd.unwrap_or_default(),
            binary: PathBuf::new(),
            pid: None,
            status: SubagentStatus::Failed,
            exit_code: None,
            output: String::new(),
            stderr: String::new(),
            error: Some(format!("Unknown agent: {}", task.agent)),
            data: None,
            schema_valid: None,
            validation_errors: None,
            schema_retries: None,
            iso: None,
            preserved_worktrees: Vec::new(),
            session_isolation: "ephemeral_no_session",
            hub_id: None,
            is_error: true,
        }
    }

    fn failed(
        agent: &AgentDefinition,
        task: SubagentTask,
        step: Option<usize>,
        error: String,
    ) -> Self {
        let mut result = Self::starting(agent, task, step, Path::new(""), Path::new(""), &[]);
        result.fail(error);
        result
    }

    fn fail(&mut self, error: String) {
        self.status = SubagentStatus::Failed;
        self.error = Some(error);
        self.is_error = true;
    }
}

fn render_results(results: &[SubagentResult]) -> String {
    results
        .iter()
        .map(|result| {
            let heading = result.step.map_or_else(
                || result.agent.clone(),
                |step| format!("step {step}: {}", result.agent),
            );
            let body = if result.output.trim().is_empty() {
                result.error.as_deref().unwrap_or("(no output)")
            } else {
                result.output.trim()
            };
            if result.is_error && !result.output.trim().is_empty() {
                format!(
                    "## {heading}\nFailed: {}\n\nPartial output (not an accepted result):\n{body}",
                    result.error.as_deref().unwrap_or("child execution failed")
                )
            } else if result.schema_valid == Some(false) {
                format!("## {heading}\nWarning: output failed schema validation; isolated edits were not applied.\n\n{body}")
            } else {
                format!("## {heading}\n{body}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Truncate `value` to at most `limit` bytes (on a char boundary), appending
/// an explicit marker when anything was cut.
fn truncated_field(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_string();
    }
    let mut cut = limit;
    while !value.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{STRUCTURED_TRUNCATION_MARKER}", &value[..cut])
}

/// Compact per-child entry for the opt-in structured block.
///
/// Field names deliberately match the `pi.subagent.result.v1` details schema
/// (`agent`, `step`, `status`, `exitCode`, `output`, `error`).
fn structured_result_entry(result: &SubagentResult) -> Value {
    json!({
        "agent": result.agent,
        "step": result.step,
        "status": result.status,
        "exitCode": result.exit_code,
        "output": truncated_field(&result.output, STRUCTURED_FIELD_LIMIT_BYTES),
        "error": result
            .error
            .as_deref()
            .map(|error| truncated_field(error, STRUCTURED_FIELD_LIMIT_BYTES)),
    })
}

/// Render the opt-in `<subagent-structured-result>` block: a JSON array of
/// per-child entries, capped at [`STRUCTURED_BLOCK_LIMIT_BYTES`].  When the
/// cap forces entries to be dropped, the final array element is an explicit
/// `{"truncated": true, "omittedResults": N}` marker.
///
/// Every `<` in the JSON body is escaped as the JSON unicode escape
/// `\\u003c` (identical after JSON parsing; in serialized JSON `<` can only
/// occur inside string literals) so child output containing
/// `</subagent-structured-result>` cannot inject a premature closing tag:
/// the wrapper tags are the only literal `<` bytes in the block.
fn structured_result_block(results: &[SubagentResult]) -> String {
    let mut entries: Vec<Value> = results.iter().map(structured_result_entry).collect();
    let mut omitted = 0usize;
    loop {
        let mut rendered = entries.clone();
        if omitted > 0 {
            rendered.push(json!({"truncated": true, "omittedResults": omitted}));
        }
        let body = serde_json::to_string(&rendered)
            .unwrap_or_else(|_| "[]".to_string())
            .replace('<', "\\u003c");
        if body.len() <= STRUCTURED_BLOCK_LIMIT_BYTES || entries.is_empty() {
            return format!("{STRUCTURED_BLOCK_OPEN}{body}{STRUCTURED_BLOCK_CLOSE}");
        }
        entries.pop();
        omitted += 1;
    }
}

fn emit_progress(update: Option<&UpdateCallback>, result: &SubagentResult) {
    let Some(update) = update else {
        return;
    };
    let preview = if result.output.trim().is_empty() {
        format!("{}: {:?}", result.agent, result.status)
    } else {
        format!("{}: {}", result.agent, result.output.trim())
    };
    update(ToolUpdate {
        content: vec![ContentBlock::Text(TextContent::new(preview))],
        details: Some(json!({
            "schema": SUBAGENT_PROGRESS_SCHEMA,
            "result": result,
        })),
    });
}

fn append_bounded(target: &mut String, value: &str) {
    if target.len() >= MAX_CHILD_OUTPUT_BYTES {
        return;
    }
    let remaining = MAX_CHILD_OUTPUT_BYTES.saturating_sub(target.len());
    if value.len() <= remaining {
        target.push_str(value);
    } else {
        let mut cut = remaining;
        while !value.is_char_boundary(cut) {
            cut -= 1;
        }
        target.push_str(&value[..cut]);
        target.push_str("\n[output truncated]\n");
    }
}

fn append_bounded_line(target: &mut String, value: &str) {
    append_bounded(target, value);
    if !target.ends_with('\n') && target.len() < MAX_CHILD_OUTPUT_BYTES {
        target.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Tool;
    use tempfile::TempDir;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn write_agent(dir: &Path, name: &str, body: &str) {
        std::fs::create_dir_all(dir).expect("create agent dir");
        std::fs::write(dir.join(format!("{name}.md")), body).expect("write agent");
    }

    fn project_guard(
        cwd: &Path,
        global_dir: &Path,
        cli_trust: bool,
    ) -> crate::workspace_trust::WorkspaceTrustGuard {
        use crate::workspace_trust::{
            TRUST_STORE_FILE, TrustInputs, WorkspaceTrustGuard, establish,
        };

        let inputs = TrustInputs {
            cli_trust,
            trust_all_workspaces: false,
            env_override: None,
            interactive: false,
        };
        let state = establish(cwd, &global_dir.join(TRUST_STORE_FILE), &inputs, |_| {
            panic!("noninteractive discovery must not prompt")
        })
        .expect("establish project trust");
        WorkspaceTrustGuard::from_established(cwd, &state, &inputs, global_dir, None)
    }

    #[test]
    fn untrusted_project_profiles_cannot_expand_bundled_or_user_tool_access() {
        for with_user_profile in [false, true] {
            let temp = TempDir::new().expect("tempdir");
            let cwd = temp.path().join("workspace");
            let global = temp.path().join("global");
            let project_agents = cwd.join(".pi/agents");
            write_agent(
                &project_agents,
                "scout",
                "---\nname: scout\ndescription: project\ntools: eval,github,security_scan,subagent\n---\nproject prompt",
            );
            if with_user_profile {
                write_agent(
                    &global.join("agents"),
                    "scout",
                    "---\nname: scout\ndescription: user\ntools: read\n---\nuser prompt",
                );
            }
            let tool = SubagentTool::with_paths(cwd, global, PathBuf::from("unused-child"));
            for scope in [AgentScope::Both, AgentScope::Project, AgentScope::User] {
                let agents = tool.discover(scope).expect("untrusted profiles skipped");
                let scout = &agents["scout"];
                let expected = if with_user_profile && !matches!(scope, AgentScope::Project) {
                    assert!(matches!(scout.source, AgentSource::User));
                    "read"
                } else {
                    assert!(matches!(scout.source, AgentSource::BuiltIn));
                    "read,grep,find,ls"
                };
                let args = child_args(scout, "inspect", None, None, None, None);
                assert!(
                    args.windows(2)
                        .any(|pair| pair[0] == "--tools" && pair[1] == expected)
                );
            }

            let trusted = tool.with_project_trust(true);
            let agents = trusted
                .discover(AgentScope::Both)
                .expect("explicit host trust");
            assert!(matches!(agents["scout"].source, AgentSource::Project));
            let args = child_args(&agents["scout"], "inspect", None, None, None, None);
            assert!(args.windows(2).any(|pair| {
                pair[0] == "--tools" && pair[1] == "eval,github,security_scan,subagent"
            }));

            write_agent(&project_agents, "scout", "malformed project definition");
            assert!(trusted.discover(AgentScope::Both).is_err());
            let denied = trusted.with_project_trust(false);
            for scope in [AgentScope::Both, AgentScope::Project, AgentScope::User] {
                denied.discover(scope).expect("denied bytes are not parsed");
            }
        }
    }

    #[test]
    fn captured_project_denial_overrides_builder_flags_and_anchors_user_profiles() {
        let temp = TempDir::new().expect("tempdir");
        let cwd = temp.path().join("workspace");
        let global = temp.path().join("captured-global");
        let other_global = temp.path().join("other-global");
        write_agent(
            &cwd.join(".pi/agents"),
            "scout",
            "---\nname: scout\ndescription: project\ntools: eval,github,security_scan,subagent\n---\nproject prompt",
        );
        for (root, description) in [(&global, "captured user"), (&other_global, "other user")] {
            write_agent(
                &root.join("agents"),
                "scout",
                &format!(
                    "---\nname: scout\ndescription: {description}\ntools: read\n---\nuser prompt"
                ),
            );
        }
        let denied_guard = project_guard(&cwd, &global, false);
        assert!(!denied_guard.trusted());
        // A later process's approval cannot promote the original launch.
        assert!(project_guard(&cwd, &global, true).trusted());
        for guard_first in [false, true] {
            let tool = SubagentTool::with_paths(
                cwd.clone(),
                other_global.clone(),
                PathBuf::from("unused-child"),
            );
            let tool = if guard_first {
                tool.with_workspace_trust_guard(Some(denied_guard.clone()))
                    .with_project_trust(true)
            } else {
                tool.with_project_trust(true)
                    .with_workspace_trust_guard(Some(denied_guard.clone()))
            };
            for scope in [AgentScope::Both, AgentScope::User] {
                let agents = tool.discover(scope).expect("captured user profiles");
                assert_eq!(agents["scout"].description, "captured user");
            }
            assert!(matches!(
                tool.discover(AgentScope::Project)
                    .expect("bundled fallback")["scout"]
                    .source,
                AgentSource::BuiltIn
            ));
        }
    }

    #[test]
    fn project_profile_changes_require_fresh_admission_before_request_preparation() {
        for inherited in [false, true] {
            let temp = TempDir::new().expect("tempdir");
            let project = temp.path().join("workspace");
            let cwd = if inherited {
                project.join("nested")
            } else {
                project.clone()
            };
            std::fs::create_dir_all(&cwd).expect("workspace");
            let global = temp.path().join("global");
            let profiles = project.join(".pi/agents");
            write_agent(
                &profiles,
                "scout",
                "---\nname: scout\ndescription: admitted\ntools: read\n---\noriginal prompt",
            );
            let guard = project_guard(&cwd, &global, true);
            let tool = SubagentTool::with_paths(
                cwd.clone(),
                global.clone(),
                PathBuf::from("unused-child"),
            )
            .with_model_config(Some(Config::default()))
            .with_workspace_trust_guard(Some(guard));
            let mut request: SubagentRequest =
                serde_json::from_value(json!({"agent":"scout", "task":"inspect"}))
                    .expect("request");
            let admitted = tool
                .prepare_request(&request)
                .expect("freeze admitted profile");
            assert_eq!(
                admitted.agents["scout"].tools,
                Some(vec!["read".to_string()])
            );

            write_agent(
                &profiles,
                "scout",
                "---\nname: scout\ndescription: changed\ntools: eval,github,security_scan,subagent\n---\nchanged prompt",
            );
            for scope in [AgentScope::Both, AgentScope::Project] {
                request.scope = scope;
                let error = tool
                    .prepare_request(&request)
                    .err()
                    .expect("changed profile must fail before child preparation");
                assert!(
                    error
                        .to_string()
                        .contains("PI_WORKSPACE_TRUST_RESTART_REQUIRED")
                );
            }
            assert_eq!(
                admitted.agents["scout"].tools,
                Some(vec!["read".to_string()])
            );
            request.scope = AgentScope::User;
            let user = tool
                .prepare_request(&request)
                .expect("user-only remains usable");
            assert!(matches!(user.agents["scout"].source, AgentSource::BuiltIn));

            let fresh_guard = project_guard(&cwd, &global, true);
            assert!(tool.discover(AgentScope::Both).is_err());
            let fresh = tool.with_workspace_trust_guard(Some(fresh_guard));
            let agents = fresh
                .discover(AgentScope::Both)
                .expect("new launch admission");
            assert_eq!(agents["scout"].description, "changed");
        }
    }

    #[test]
    fn project_agents_override_user_and_parse_runtime_configuration() {
        let temp = TempDir::new().expect("tempdir");
        let global = temp.path().join("global");
        let cwd = temp.path().join("workspace").join("nested");
        write_agent(
            &global.join("agents"),
            "scout",
            "---\nname: scout\ndescription: user\nmodel: provider/user\nreasoning: low\ntools: read,grep\nskills: one.md,two.md\n---\nuser prompt",
        );
        write_agent(
            &cwd.parent().expect("parent").join(".pi/agents"),
            "scout",
            "---\nname: scout\ndescription: project\nmodel: provider/project\nthinking: high\ntools: read,find\n---\nproject prompt",
        );
        let agents =
            discover_agents_with_roots(&cwd, &global, AgentScope::Both, true).expect("discover");
        let scout = agents.get("scout").expect("project scout");
        assert_eq!(scout.description, "project");
        assert_eq!(scout.model.as_deref(), Some("provider/project"));
        assert_eq!(scout.reasoning.as_deref(), Some("high"));
        assert_eq!(
            scout.tools.as_deref(),
            Some(["read".to_string(), "find".to_string()].as_slice())
        );
        assert_eq!(scout.system_prompt, "project prompt");
        assert!(matches!(scout.source, AgentSource::Project));
    }

    /// bd-cv653.3.1: agent-def `model:` pin beats the role spec; the role
    /// spec is used only when the definition has no pin. The resolved parent
    /// supplies the default; standalone unbound tools retain ambient selection.
    #[test]
    fn child_args_role_model_precedence() {
        let base = AgentDefinition {
            name: "scout".to_string(),
            description: "inspect".to_string(),
            model: None,
            reasoning: None,
            tools: None,
            skills: Vec::new(),
            system_prompt: String::new(),
            output_schema: None,
            source: AgentSource::User,
            file_path: PathBuf::from("/tmp/scout.md"),
        };
        let args_of =
            |agent: &AgentDefinition, spec: Option<&str>, parent: Option<&ToolModelSelection>| {
                child_args(agent, "inspect provider", spec, parent, None, None)
                    .iter()
                    .map(|arg| arg.to_string_lossy().to_string())
                    .collect::<Vec<_>>()
            };
        let model_value = |args: &[String]| {
            args.windows(2)
                .find(|pair| pair[0] == "--model")
                .map(|pair| pair[1].clone())
        };

        // No pin + role spec → role spec is used (task/smol resolution
        // happens at the registry; here we only prove the wire shape).
        let parent = ToolModelSelection {
            provider: "router".to_string(),
            model: "vendor/resolved-model:literal".to_string(),
        };
        let args = args_of(&base, Some("openai/gpt-5-mini:low"), Some(&parent));
        assert_eq!(
            model_value(&args).as_deref(),
            Some("openai/gpt-5-mini:low"),
            "role spec must be passed as --model when the agent def has no pin"
        );
        assert!(!args.iter().any(|arg| arg == "--provider"));

        // Pin present → pin wins over the role spec.
        let pinned = AgentDefinition {
            model: Some("ai-router/gpt-5.6-sol".to_string()),
            ..base.clone()
        };
        let args = args_of(&pinned, Some("openai/gpt-5-mini:low"), Some(&parent));
        assert_eq!(
            model_value(&args).as_deref(),
            Some("ai-router/gpt-5.6-sol"),
            "agent-def model pin must beat the role spec"
        );
        assert!(!args.iter().any(|arg| arg == "--provider"));

        let args = args_of(&base, None, Some(&parent));
        assert_eq!(model_value(&args).as_deref(), Some(parent.model.as_str()));
        assert!(args.windows(2).any(|pair| pair == ["--provider", "router"]));

        // Unbound standalone tools still resolve ordinary child configuration.
        let args = args_of(&base, None, None);
        assert!(
            model_value(&args).is_none(),
            "no role spec and no pin must not inject --model"
        );
    }

    #[test]
    fn child_args_keep_model_effort_tools_skills_and_prompt() {
        let agent = AgentDefinition {
            name: "scout".to_string(),
            description: "inspect".to_string(),
            model: Some("ai-router/gpt-5.6-sol".to_string()),
            reasoning: Some("high".to_string()),
            tools: Some(vec!["read".to_string(), "grep".to_string()]),
            skills: vec![PathBuf::from("/tmp/skill.md")],
            system_prompt: "be precise".to_string(),
            output_schema: None,
            source: AgentSource::User,
            file_path: PathBuf::from("/tmp/scout.md"),
        };
        let args = child_args(&agent, "inspect provider", None, None, None, None)
            .iter()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect::<Vec<_>>();
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--model", "ai-router/gpt-5.6-sol"])
        );
        assert!(args.windows(2).any(|pair| pair == ["--thinking", "high"]));
        assert!(args.windows(2).any(|pair| pair == ["--tools", "read,grep"]));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--skill", "/tmp/skill.md"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--append-system-prompt", "be precise"])
        );
        assert_eq!(
            args.last().map(String::as_str),
            Some("Task: inspect provider")
        );
        let selected = model_selection::Selection {
            provider: "local".to_string(),
            model: "vendor/model:literal".to_string(),
            thinking: Some(crate::model::ThinkingLevel::Low),
        };
        let args = child_args(
            &agent,
            "inspect provider",
            Some("role/model"),
            None,
            Some(&selected),
            None,
        )
        .iter()
        .map(|arg| arg.to_string_lossy().to_string())
        .collect::<Vec<_>>();
        assert!(args.windows(2).any(|pair| pair == ["--provider", "local"]));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--model", "vendor/model:literal"])
        );
        assert!(args.windows(2).any(|pair| pair == ["--thinking", "low"]));
        assert_eq!(args.iter().filter(|arg| *arg == "--thinking").count(), 1);
        assert!(args.windows(2).any(|pair| pair == ["--tools", "read,grep"]));
    }

    #[test]
    fn child_tool_ceiling_reaches_registry_for_bundled_and_default_profiles() {
        use clap::Parser as _;

        let cwd = TempDir::new().expect("workspace");
        let config = Config::default();
        let mut agents = bundled::definitions().into_values().collect::<Vec<_>>();
        agents.push(tan_agent_definition());
        let mut implicit = tan_agent_definition();
        implicit.name = "implicit-tools-fixture".to_string();
        implicit.tools = None;
        agents.push(implicit);
        let mut empty = tan_agent_definition();
        empty.name = "empty-tools-fixture".to_string();
        empty.tools = Some(Vec::new());
        agents.push(empty);
        for agent in agents {
            for args in [
                child_args(&agent, "inspect", None, None, None, None),
                worker_args(&agent, None, None, None),
            ] {
                let cli = crate::cli::Cli::try_parse_from(
                    std::iter::once(OsString::from("pi")).chain(args),
                )
                .expect("native child CLI");
                assert!(
                    cli.no_implicit_tools,
                    "{} lost its tool ceiling",
                    agent.name
                );
                let selected = cli.enabled_tools(&config);
                let registry = if cli.builtin_tools_disabled() {
                    crate::tools::ToolRegistry::without_builtins(Some(&config))
                } else {
                    crate::tools::ToolRegistry::with_builtin_tool_policy(
                        &selected,
                        cwd.path(),
                        Some(&config),
                        None,
                        None,
                        false,
                        None,
                        !cli.no_implicit_tools,
                    )
                };
                let mut expected = agent
                    .tools
                    .clone()
                    .unwrap_or_else(|| split_csv(DEFAULT_CHILD_TOOLS));
                expected.sort();
                let mut actual = registry
                    .tools()
                    .iter()
                    .filter(|tool| tool.name() != "xdev")
                    .map(|tool| tool.name().to_string())
                    .collect::<Vec<_>>();
                actual.sort();
                assert_eq!(actual, expected, "profile: {}", agent.name);
                assert!(
                    registry
                        .discoverable_index()
                        .iter()
                        .all(|(name, _)| expected.contains(name))
                );
            }
        }
    }

    #[test]
    fn tan_definition_uses_task_role_and_default_non_recursive_tools() {
        let args = child_args(
            &tan_agent_definition(),
            "update the changelog",
            Some("ai-router/gpt-5.6-terra"),
            None,
            None,
            None,
        )
        .iter()
        .map(|arg| arg.to_string_lossy().to_string())
        .collect::<Vec<_>>();
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--model", "ai-router/gpt-5.6-terra"])
        );
        assert!(args.windows(2).any(|pair| {
            pair == [
                "--tools",
                "read,bash,edit,write,grep,find,ls,hashline_edit,hub",
            ]
        }));
        assert!(!args.iter().any(|arg| arg == "subagent"));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--append-system-prompt", TAN_SYSTEM_PROMPT])
        );
        assert_eq!(
            args.last().map(String::as_str),
            Some("Task: update the changelog")
        );
    }

    #[test]
    fn tan_completion_has_bounded_follow_up_and_distinct_card_shape() {
        let task = SubagentTask {
            agent: TAN_AGENT_NAME.to_string(),
            task: "update the changelog".to_string(),
            task_file: None,
            task_file_sha256: None,
            model: None,
            model_selection: None,
            fallback_policy: None,
            cwd: None,
            isolation: None,
            iso_apply: None,
            output_schema: None,
            schema_mode: SchemaMode::default(),
        };
        let mut result = SubagentResult::unknown(task, None);
        result.hub_id = Some("tan-7".to_string());
        result.status = SubagentStatus::Completed;
        result.output = "changelog updated".to_string();
        result.error = None;
        result.is_error = false;
        let completion = TanCompletion::from_result(result);
        assert_eq!(completion.schema, TAN_RESULT_SCHEMA);
        assert_eq!(completion.hub_id.as_deref(), Some("tan-7"));
        assert_eq!(
            completion.follow_up_text(),
            "[background tan tan-7 settled: completed]\nwork: update the changelog\nsummary:\nchangelog updated"
        );
        assert!(completion.card_text().starts_with("(/tan completed)\n"));
    }

    #[test]
    fn request_requires_exactly_one_mode_and_renders_chain_context() {
        let invalid: SubagentRequest = serde_json::from_value(json!({
            "agent": "scout", "task": "x", "tasks": [{"agent": "review", "task": "y"}]
        }))
        .expect("parse");
        assert!(invalid.mode().is_err());
        let mut task = SubagentTask {
            agent: "review".to_string(),
            task: concat!("review {", "previous}").to_string(),
            task_file: None,
            task_file_sha256: None,
            model: None,
            model_selection: None,
            fallback_policy: None,
            cwd: None,
            isolation: None,
            iso_apply: None,
            output_schema: None,
            schema_mode: SchemaMode::default(),
        };
        let mut previous = SubagentResult::unknown(task.clone(), None);
        previous.output = "evidence".to_string();
        task.render_previous_result(Some(&previous)).unwrap();
        assert_eq!(task.task, "review evidence");
    }

    /// bd-cv653.5.1: `{{previous.data.<path>}}` addresses the prior task's
    /// schema-validated data — scalars render bare, nested paths resolve via
    /// dots, and misses stay verbatim. Without a schema-valid prior result,
    /// tokens are untouched.
    #[test]
    fn chain_previous_data_field_addressing() {
        let mut base = SubagentTask {
            agent: "review".to_string(),
            task: "verdict {{previous.data.verdict}} n {{previous.data.stats.count}} miss {{previous.data.absent}}"
                .to_string(),
            task_file: None,
            task_file_sha256: None,
            model: None,
            model_selection: None,
            fallback_policy: None,
            cwd: None,
            isolation: None,
            iso_apply: None,
            output_schema: None,
            schema_mode: SchemaMode::default(),
        };
        let mut previous = SubagentResult::unknown(base.clone(), None);
        previous.schema_valid = Some(true);
        previous.data = Some(json!({"verdict": "pass", "stats": {"count": 3}}));
        let mut rendered = base.clone();
        rendered.render_previous_result(Some(&previous)).unwrap();
        assert_eq!(
            rendered.task,
            "verdict pass n 3 miss {{previous.data.absent}}"
        );

        // Not schema-valid → tokens untouched.
        let mut invalid = SubagentResult::unknown(base.clone(), None);
        invalid.schema_valid = Some(false);
        invalid.data = Some(json!({"verdict": "pass"}));
        base.render_previous_result(Some(&invalid)).unwrap();
        assert!(base.task.contains("{{previous.data.verdict}}"));
    }

    /// bd-cv653.5.1: JSON extraction tolerates prose/fence wrapping, and
    /// validation reports keyword errors with instance paths.
    #[test]
    fn output_schema_validation_matrix() {
        let schema = json!({
            "type": "object",
            "required": ["verdict"],
            "properties": {"verdict": {"type": "string"}}
        });

        let valid = validate_child_output(r#"{"verdict": "pass"}"#, &schema);
        assert_eq!(valid.expect("valid")["verdict"], "pass");

        let fenced = validate_child_output(
            "Here you go:\n```json\n{\"verdict\": \"pass\"}\n```",
            &schema,
        );
        assert_eq!(fenced.expect("fenced")["verdict"], "pass");

        let embedded = validate_child_output(
            r#"The answer is {"verdict": "pass", "note": "{brace} inside"} — done."#,
            &schema,
        );
        assert_eq!(embedded.expect("embedded")["verdict"], "pass");

        let wrong_shape = validate_child_output(r#"{"verdict": 7}"#, &schema);
        let errors = wrong_shape.expect_err("type mismatch");
        assert!(
            errors.iter().any(|error| error.contains("verdict")),
            "{errors:?}"
        );

        let missing = validate_child_output(r#"{"other": true}"#, &schema);
        assert!(missing.is_err());

        let no_json = validate_child_output("no structured output here", &schema);
        assert_eq!(
            no_json.expect_err("no json"),
            vec!["child output contains no parseable JSON value".to_string()]
        );
    }

    /// bd-cv653.5.1: an agent definition may carry a single-line JSON
    /// `output_schema:`; invalid JSON there is a load-time error.
    #[test]
    fn agent_definition_output_schema_parses_and_rejects_invalid() {
        let temp = TempDir::new().expect("tempdir");
        let global = temp.path().join("global");
        write_agent(
            &global.join("agents"),
            "typed",
            "---\nname: typed\ndescription: typed agent\noutput_schema: {\"type\": \"object\"}\n---\nbody",
        );
        let agents = discover_agents_with_roots(temp.path(), &global, AgentScope::User, false)
            .expect("discover typed agent");
        assert_eq!(
            agents.get("typed").expect("typed").output_schema,
            Some(json!({"type": "object"}))
        );

        write_agent(
            &global.join("agents"),
            "broken",
            "---\nname: broken\ndescription: broken agent\noutput_schema: {not json\n---\nbody",
        );
        let error = discover_agents_with_roots(temp.path(), &global, AgentScope::User, false)
            .expect_err("invalid schema must fail agent loading");
        assert!(error.to_string().contains("output_schema"), "{error}");
    }

    #[test]
    fn tool_schema_advertises_all_three_workflows() {
        let tool =
            SubagentTool::with_paths(PathBuf::from("."), PathBuf::from("."), PathBuf::from("pi"));
        let schema = tool.parameters();
        assert!(schema["properties"].get("agent").is_some());
        assert!(schema["properties"].get("tasks").is_some());
        assert!(schema["properties"].get("chain").is_some());
    }

    fn execute_unknown_agent(structured: bool) -> ToolOutput {
        let temp = TempDir::new().expect("tempdir");
        let tool = SubagentTool::with_paths(
            temp.path().to_path_buf(),
            temp.path().join("global"),
            PathBuf::from("pi"),
        )
        .with_structured_results(structured);
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime
            .block_on(tool.execute(
                "subagent-structured",
                json!({"agent": "missing-agent-fixture", "task": "inspect"}),
                None,
            ))
            .expect("execute returns tool output")
    }

    fn output_text(output: &ToolOutput) -> String {
        let ContentBlock::Text(text) = &output.content[0] else {
            panic!("expected text output");
        };
        text.text.clone()
    }

    #[test]
    fn structured_block_disabled_by_default_keeps_output_byte_identical() {
        let output = execute_unknown_agent(false);
        let text = output_text(&output);
        assert_eq!(
            text,
            "## missing-agent-fixture\nUnknown agent: missing-agent-fixture"
        );
        assert!(!text.contains(STRUCTURED_BLOCK_OPEN));
        assert!(output.is_error);
    }

    #[test]
    fn structured_block_appends_parseable_json_matching_details() {
        let output = execute_unknown_agent(true);
        let text = output_text(&output);
        let prefix = "## missing-agent-fixture\nUnknown agent: missing-agent-fixture\n\n";
        assert!(text.starts_with(prefix), "unexpected text: {text}");
        let block = &text[prefix.len()..];
        let body = block
            .strip_prefix(STRUCTURED_BLOCK_OPEN)
            .and_then(|rest| rest.strip_suffix(STRUCTURED_BLOCK_CLOSE))
            .expect("structured block is fenced");
        let parsed: Value = serde_json::from_str(body).expect("block payload parses as JSON");
        let entries = parsed.as_array().expect("payload is an array");
        assert_eq!(entries.len(), 1);
        let details = output.details.as_ref().expect("details present");
        assert_eq!(entries[0]["agent"], details["results"][0]["agent"]);
        assert_eq!(entries[0]["status"], details["results"][0]["status"]);
        assert_eq!(entries[0]["error"], details["results"][0]["error"]);
        assert_eq!(entries[0]["status"], "failed");
        assert_eq!(entries[0]["exitCode"], Value::Null);
    }

    #[test]
    fn structured_block_truncates_fields_and_caps_block() {
        let task = |name: &str| SubagentTask {
            agent: name.to_string(),
            task: "t".to_string(),
            task_file: None,
            task_file_sha256: None,
            model: None,
            model_selection: None,
            fallback_policy: None,
            cwd: None,
            isolation: None,
            iso_apply: None,
            output_schema: None,
            schema_mode: SchemaMode::default(),
        };
        let mut long = SubagentResult::unknown(task("long"), None);
        long.output = "x".repeat(10 * 1024);
        let entry = structured_result_entry(&long);
        let rendered_output = entry["output"].as_str().expect("output is a string");
        assert!(rendered_output.ends_with(STRUCTURED_TRUNCATION_MARKER));
        assert!(
            rendered_output.len()
                <= STRUCTURED_FIELD_LIMIT_BYTES + STRUCTURED_TRUNCATION_MARKER.len()
        );

        let results: Vec<SubagentResult> = (0..20)
            .map(|index| {
                let mut result = SubagentResult::unknown(task(&format!("agent-{index}")), None);
                result.output = "y".repeat(4 * 1024);
                result
            })
            .collect();
        let block = structured_result_block(&results);
        let body = block
            .strip_prefix(STRUCTURED_BLOCK_OPEN)
            .and_then(|rest| rest.strip_suffix(STRUCTURED_BLOCK_CLOSE))
            .expect("capped block is fenced");
        assert!(body.len() <= STRUCTURED_BLOCK_LIMIT_BYTES);
        let parsed: Value = serde_json::from_str(body).expect("capped payload parses");
        let entries = parsed.as_array().expect("capped payload is an array");
        let marker = entries.last().expect("array is non-empty");
        assert_eq!(marker["truncated"], Value::Bool(true));
        let omitted = marker["omittedResults"]
            .as_u64()
            .expect("omittedResults present");
        assert!(omitted > 0);
        assert_eq!(
            entries.len() - 1 + usize::try_from(omitted).expect("fits"),
            20
        );
    }

    #[test]
    fn structured_block_escapes_close_tag_in_child_output() {
        let task = SubagentTask {
            agent: "inj".to_string(),
            task: "t".to_string(),
            task_file: None,
            task_file_sha256: None,
            model: None,
            model_selection: None,
            fallback_policy: None,
            cwd: None,
            isolation: None,
            iso_apply: None,
            output_schema: None,
            schema_mode: SchemaMode::default(),
        };
        let mut result = SubagentResult::unknown(task, None);
        result.output = format!("before {STRUCTURED_BLOCK_CLOSE} after");
        let block = structured_result_block(&[result]);

        // The wrapper close tag must be the only literal close tag: child
        // output cannot inject a premature terminator.
        assert_eq!(block.matches(STRUCTURED_BLOCK_CLOSE).count(), 1);
        assert!(block.ends_with(STRUCTURED_BLOCK_CLOSE));
        let body = block
            .strip_prefix(STRUCTURED_BLOCK_OPEN)
            .and_then(|rest| rest.strip_suffix(STRUCTURED_BLOCK_CLOSE))
            .expect("block is fenced");
        assert!(
            !body.contains('<'),
            "JSON body must not contain literal '<'"
        );

        // The escaping is lossless: parsing yields the original output.
        let parsed: Value = serde_json::from_str(body).expect("payload parses");
        assert_eq!(
            parsed[0]["output"],
            Value::String(format!("before {STRUCTURED_BLOCK_CLOSE} after"))
        );
    }

    #[test]
    #[cfg(unix)]
    fn child_process_uses_selected_binary_streams_json_and_inherits_global_dir() {
        let temp = TempDir::new().expect("tempdir");
        let global_dir = temp.path().join("global");
        write_agent(
            &global_dir.join("agents"),
            "scout",
            "---\nname: scout\ndescription: child-process fixture\n---\nReturn the child result.",
        );

        let child = temp.path().join("child-fixture.sh");
        std::fs::write(
            &child,
            r#"#!/bin/sh
printf '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"streamed:"}}\n'
printf '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"%s"}}\n' "$PI_CODING_AGENT_DIR"
printf '{"type":"agent_end","messages":[{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"final child result"}]}]}\n'
"#,
        )
        .expect("write child fixture");
        let mut permissions = std::fs::metadata(&child)
            .expect("child metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&child, permissions).expect("make child executable");

        let tool =
            SubagentTool::with_paths(temp.path().to_path_buf(), global_dir.clone(), child.clone());
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let updates = Arc::new(std::sync::Mutex::new(String::new()));
        let captured = Arc::clone(&updates);
        let output = runtime
            .block_on(tool.execute(
                "subagent-fixture",
                json!({"agent": "scout", "task": "verify child protocol"}),
                Some(Box::new(move |update| {
                    captured
                        .lock()
                        .unwrap()
                        .push_str(&serde_json::to_string(&update).unwrap());
                })),
            ))
            .expect("child execution succeeds");

        let ContentBlock::Text(text) = &output.content[0] else {
            panic!("expected text output");
        };
        assert_eq!(text.text, "## scout\nfinal child result");
        let updates = updates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert!(updates.contains("streamed:"));
        assert!(updates.contains(global_dir.to_string_lossy().as_ref()));
        assert!(
            output.details.as_ref().is_some_and(|details| {
                details["results"][0]["binary"] == Value::String(child.display().to_string())
                    && details["results"][0]["status"] == "completed"
                    && details["sessionIsolation"] == "ephemeral_no_session"
            }),
            "missing child-process evidence: {output:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn background_tan_runs_child_and_registers_tan_hub_kind() {
        let temp = TempDir::new().expect("tempdir");
        let global_dir = temp.path().join("global");
        let child = temp.path().join("tan-child-fixture.sh");
        std::fs::write(
            &child,
            r#"#!/bin/sh
sleep 1
printf '{"type":"agent_end","messages":[{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"tan fixture completed"}]}]}\n'
"#,
        )
        .expect("write tan child fixture");
        let mut permissions = std::fs::metadata(&child)
            .expect("tan child metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&child, permissions).expect("make tan child executable");

        let tool = SubagentTool::with_paths(temp.path().to_path_buf(), global_dir, child)
            .with_role_model_spec(Some("test-provider/task-model".to_string()));
        let task = format!("tan-fixture-{}", std::process::id());
        let child_task = task.clone();
        let handle = std::thread::spawn(move || {
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .expect("runtime build");
            runtime
                .block_on(tool.run_background_tan(&child_task))
                .expect("tan child execution succeeds")
        });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let running_entry = loop {
            let candidate = crate::agent_hub::registry()
                .lock()
                .expect("hub registry lock")
                .roster()
                .into_iter()
                .find(|entry| {
                    entry.task == task
                        && matches!(
                            entry.status,
                            crate::agent_hub::ChildStatus::Running
                                | crate::agent_hub::ChildStatus::Done
                        )
                });
            if let Some(entry) = candidate {
                break entry;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "tan child never appeared in the running hub roster"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert_eq!(running_entry.kind, crate::agent_hub::ChildKind::Tan);

        let completion = handle.join().expect("join tan child thread");

        assert!(!completion.is_error, "{completion:?}");
        assert_eq!(completion.status, "completed");
        assert!(completion.output.contains("tan fixture completed"));
        let hub_id = completion.hub_id.expect("tan completion has hub id");
        let entry = crate::agent_hub::registry()
            .lock()
            .expect("hub registry lock")
            .get(&hub_id)
            .expect("tan child remains in roster");
        assert_eq!(entry.kind, crate::agent_hub::ChildKind::Tan);
        assert_eq!(entry.status, crate::agent_hub::ChildStatus::Done);
        assert_eq!(entry.task, task);
    }

    /// Write an executable stub child that emits `first` on its first run and
    /// `second` from then on (state via a marker file next to the script).
    #[cfg(unix)]
    fn write_two_phase_child(temp: &Path, first: &str, second: &str) -> PathBuf {
        let child = temp.join("two-phase-child.sh");
        let marker = temp.join("two-phase-marker");
        // `printf '%s\n' '<line>'` passes the JSON event through untouched:
        // no printf escape processing, and the single-quoted argument may
        // freely contain double quotes and backslashes (`first`/`second` are
        // already JSON-string-escaped payloads for the `text` field).
        std::fs::write(
            &child,
            format!(
                r#"#!/bin/sh
if [ -f "{marker}" ]; then
  printf '%s\n' '{{"type":"agent_end","messages":[{{"role":"assistant","stopReason":"stop","content":[{{"type":"text","text":"{second}"}}]}}]}}'
else
  : > "{marker}"
  printf '%s\n' '{{"type":"agent_end","messages":[{{"role":"assistant","stopReason":"stop","content":[{{"type":"text","text":"{first}"}}]}}]}}'
fi
"#,
                marker = marker.display(),
            ),
        )
        .expect("write two-phase child");
        let mut permissions = std::fs::metadata(&child)
            .expect("child metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&child, permissions).expect("make child executable");
        child
    }

    /// bd-cv653.5.1 acceptance 1+2 (happy half): invalid first output, one
    /// corrective retry, valid second output → parsed `data`, `schemaValid`
    /// true, one recorded retry, task not an error.
    #[test]
    #[cfg(unix)]
    fn output_schema_retry_then_valid_yields_parsed_data() {
        let temp = TempDir::new().expect("tempdir");
        let global_dir = temp.path().join("global");
        write_agent(
            &global_dir.join("agents"),
            "typed",
            "---\nname: typed\ndescription: typed child\n---\nReturn JSON.",
        );
        let child =
            write_two_phase_child(temp.path(), "not json at all", r#"{\"verdict\": \"pass\"}"#);

        let tool = SubagentTool::with_paths(temp.path().to_path_buf(), global_dir, child);
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let output = runtime
            .block_on(tool.execute(
                "typed-retry",
                json!({
                    "agent": "typed",
                    "task": "produce a verdict",
                    "outputSchema": {
                        "type": "object",
                        "required": ["verdict"],
                        "properties": {"verdict": {"type": "string"}}
                    }
                }),
                None,
            ))
            .expect("typed subagent run");

        assert!(!output.is_error, "{output:?}");
        let details = output.details.expect("details");
        let result = &details["results"][0];
        assert_eq!(result["schemaValid"], true, "{result}");
        assert_eq!(result["schemaRetries"], 1, "{result}");
        assert_eq!(result["data"]["verdict"], "pass", "{result}");
        assert_eq!(details["schema"], SUBAGENT_RESULT_SCHEMA);
    }

    /// bd-cv653.5.1 acceptance 2 (exhaustion half): output stays invalid
    /// through the single retry — permissive keeps the raw result with
    /// warning fields; strict fails the task with the validation errors.
    #[test]
    #[cfg(unix)]
    fn output_schema_exhaustion_permissive_warns_strict_fails() {
        for (mode, expect_error) in [("permissive", false), ("strict", true)] {
            let temp = TempDir::new().expect("tempdir");
            let global_dir = temp.path().join("global");
            write_agent(
                &global_dir.join("agents"),
                "typed",
                "---\nname: typed\ndescription: typed child\n---\nReturn JSON.",
            );
            let child = write_two_phase_child(temp.path(), "still not json", "also not json");

            let tool = SubagentTool::with_paths(temp.path().to_path_buf(), global_dir, child);
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .expect("runtime build");
            let output = runtime
                .block_on(tool.execute(
                    "typed-exhausted",
                    json!({
                        "agent": "typed",
                        "task": "produce a verdict",
                        "outputSchema": {"type": "object"},
                        "schemaMode": mode
                    }),
                    None,
                ))
                .expect("typed subagent run");

            assert_eq!(output.is_error, expect_error, "mode={mode}: {output:?}");
            let details = output.details.expect("details");
            let result = &details["results"][0];
            assert_eq!(result["schemaValid"], false, "mode={mode}: {result}");
            assert_eq!(result["schemaRetries"], 1, "mode={mode}: {result}");
            assert!(
                result["validationErrors"]
                    .as_array()
                    .is_some_and(|errors| !errors.is_empty()),
                "mode={mode}: {result}"
            );
            assert_eq!(
                result["status"],
                if expect_error { "failed" } else { "completed" },
                "mode={mode}: {result}"
            );
        }
    }
}
