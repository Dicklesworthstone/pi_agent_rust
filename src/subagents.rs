//! Native child-agent orchestration.
//!
//! This module deliberately uses the Pi executable that is already running
//! (or the explicit `PI_SUBAGENT_PI_BINARY` override) instead of resolving a
//! `pi` binary through `PATH`.  That makes a Rust Pi parent reliably launch
//! Rust Pi children even on hosts that also have the TypeScript implementation
//! installed.

use crate::agent_cx::AgentCx;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::model::{ContentBlock, TextContent};
use crate::tools::{Tool, ToolEffects, ToolOutput, ToolUpdate};
use async_trait::async_trait;
use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

const MAX_PARALLEL_TASKS: usize = 8;
const DEFAULT_CONCURRENCY: usize = 4;
const MAX_SUBAGENT_DEPTH: usize = 3;
const MAX_CHILD_OUTPUT_BYTES: usize = 256 * 1024;
const SUBAGENT_RESULT_SCHEMA: &str = "pi.subagent.result.v1";
const SUBAGENT_PROGRESS_SCHEMA: &str = "pi.subagent.progress.v1";
const DEFAULT_CHILD_TOOLS: &str = "read,bash,edit,write,grep,find,ls,hashline_edit";

type UpdateCallback = Arc<dyn Fn(ToolUpdate) + Send + Sync>;

/// A native tool that delegates bounded work to isolated Pi child processes.
pub struct SubagentTool {
    cwd: PathBuf,
    global_dir: PathBuf,
    child_binary: PathBuf,
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
            child_binary,
        }
    }

    #[cfg(test)]
    fn with_paths(cwd: PathBuf, global_dir: PathBuf, child_binary: PathBuf) -> Self {
        Self {
            cwd,
            global_dir,
            child_binary,
        }
    }

    fn discover(&self, scope: AgentScope) -> Result<BTreeMap<String, AgentDefinition>> {
        discover_agents_with_roots(&self.cwd, &self.global_dir, scope)
    }

    async fn run_request(
        &self,
        request: SubagentRequest,
        on_update: Option<UpdateCallback>,
    ) -> Result<Vec<SubagentResult>> {
        let agents = self.discover(request.scope)?;
        let concurrency = request
            .concurrency
            .unwrap_or(DEFAULT_CONCURRENCY)
            .clamp(1, MAX_PARALLEL_TASKS);

        match request.mode()? {
            RequestMode::Single(task) => {
                Ok(vec![self.run_one(&agents, task, None, on_update).await])
            }
            RequestMode::Parallel(tasks) => {
                let cwd = self.cwd.clone();
                let global_dir = self.global_dir.clone();
                let binary = self.child_binary.clone();
                let update = on_update.clone();
                let results = stream::iter(tasks.into_iter().enumerate())
                    .map(move |(index, task)| {
                        let agents = agents.clone();
                        let cwd = cwd.clone();
                        let global_dir = global_dir.clone();
                        let binary = binary.clone();
                        let update = update.clone();
                        async move {
                            let runner = ChildRunner::new(cwd, global_dir, binary);
                            (index, runner.run_one(&agents, task, None, update).await)
                        }
                    })
                    .buffer_unordered(concurrency)
                    .collect::<Vec<_>>()
                    .await;
                let mut ordered = results;
                ordered.sort_by_key(|(index, _)| *index);
                Ok(ordered.into_iter().map(|(_, result)| result).collect())
            }
            RequestMode::Chain(tasks) => {
                let mut previous = String::new();
                let mut results = Vec::with_capacity(tasks.len());
                for (step, task) in tasks.into_iter().enumerate() {
                    let task = task.with_rendered_previous(&previous);
                    let result = self
                        .run_one(&agents, task, Some(step + 1), on_update.clone())
                        .await;
                    previous.clone_from(&result.output);
                    let failed = result.is_error;
                    results.push(result);
                    if failed {
                        break;
                    }
                }
                Ok(results)
            }
        }
    }

    async fn run_one(
        &self,
        agents: &BTreeMap<String, AgentDefinition>,
        task: SubagentTask,
        step: Option<usize>,
        on_update: Option<UpdateCallback>,
    ) -> SubagentResult {
        ChildRunner::new(
            self.cwd.clone(),
            self.global_dir.clone(),
            self.child_binary.clone(),
        )
        .run_one(agents, task, step, on_update)
        .await
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
        "Delegate an isolated task to a named Pi child agent. Supports one task, bounded parallel tasks, or a sequential chain whose tasks may reference {previous}. Agent definitions live in $PI_CODING_AGENT_DIR/agents/*.md or .pi/agents/*.md."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "agent": {"type": "string", "description": "Named agent for a single delegation."},
                "task": {"type": "string", "description": "Task for a single delegation."},
                "tasks": {"type": "array", "maxItems": MAX_PARALLEL_TASKS, "items": {"$ref": "#/definitions/task"}, "description": "Independent tasks to run in parallel."},
                "chain": {"type": "array", "maxItems": MAX_PARALLEL_TASKS, "items": {"$ref": "#/definitions/task"}, "description": "Sequential tasks; {previous} is replaced with the prior child output."},
                "concurrency": {"type": "integer", "minimum": 1, "maximum": MAX_PARALLEL_TASKS},
                "scope": {"type": "string", "enum": ["both", "user", "project"], "default": "both"}
            },
            "definitions": {
                "task": {
                    "type": "object",
                    "required": ["agent", "task"],
                    "properties": {
                        "agent": {"type": "string"},
                        "task": {"type": "string"},
                        "cwd": {"type": "string"}
                    }
                }
            },
            "additionalProperties": false
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: Value,
        on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        if current_subagent_depth() >= MAX_SUBAGENT_DEPTH {
            return Err(Error::tool(
                "subagent",
                format!(
                    "Refusing nested subagent depth above {MAX_SUBAGENT_DEPTH}; child agents are isolated by default and do not receive the subagent tool."
                ),
            ));
        }
        let request: SubagentRequest = serde_json::from_value(input)
            .map_err(|error| Error::tool("subagent", format!("Invalid input: {error}")))?;
        let update = on_update.map(Arc::from);
        let mode = request.mode_name()?;
        let results = self.run_request(request, update).await?;
        let is_error = results.iter().any(|result| result.is_error);
        let content = render_results(&results);
        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(content))],
            details: Some(json!({
                "schema": SUBAGENT_RESULT_SCHEMA,
                "mode": mode,
                "sessionIsolation": "ephemeral_no_session",
                "results": results,
            })),
            is_error,
        })
    }

    fn effects(&self) -> ToolEffects {
        ToolEffects::process()
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
    tasks: Option<Vec<SubagentTask>>,
    #[serde(default)]
    chain: Option<Vec<SubagentTask>>,
    #[serde(default)]
    concurrency: Option<usize>,
    #[serde(default)]
    scope: AgentScope,
}

impl SubagentRequest {
    fn mode(&self) -> Result<RequestMode> {
        let single = self
            .agent
            .as_ref()
            .zip(self.task.as_ref())
            .map(|(agent, task)| SubagentTask {
                agent: agent.clone(),
                task: task.clone(),
                cwd: None,
            });
        let selected = usize::from(single.is_some())
            + usize::from(self.tasks.is_some())
            + usize::from(self.chain.is_some());
        if selected.ne(&1) {
            return Err(Error::tool(
                "subagent",
                "Provide exactly one of agent+task, tasks, or chain.",
            ));
        }
        if self.agent.is_some() != self.task.is_some() {
            return Err(Error::tool(
                "subagent",
                "Single delegation requires both agent and task.",
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
    task: String,
    #[serde(default)]
    cwd: Option<PathBuf>,
}

impl SubagentTask {
    fn with_rendered_previous(mut self, previous: &str) -> Self {
        self.task = self.task.replace(concat!("{", "previous", "}"), previous);
        self
    }
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
    source: AgentSource,
    file_path: PathBuf,
}

fn discover_agents_with_roots(
    cwd: &Path,
    global_dir: &Path,
    scope: AgentScope,
) -> Result<BTreeMap<String, AgentDefinition>> {
    let mut agents = BTreeMap::new();
    if !matches!(scope, AgentScope::Project) {
        load_agent_dir(&global_dir.join("agents"), AgentSource::User, &mut agents)?;
    }
    if !matches!(scope, AgentScope::User)
        && let Some(project_dir) = nearest_project_agents_dir(cwd)
    {
        // Project definitions intentionally replace user definitions of the same name.
        load_agent_dir(&project_dir, AgentSource::Project, &mut agents)?;
    }
    Ok(agents)
}

fn nearest_project_agents_dir(cwd: &Path) -> Option<PathBuf> {
    let mut current = cwd.to_path_buf();
    loop {
        let candidate = current.join(".pi").join("agents");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if !current.pop() {
            return None;
        }
    }
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
    let mut paths = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
        })
        .collect::<Vec<_>>();
    paths.sort();
    for path in paths {
        let raw = std::fs::read_to_string(&path).map_err(|error| {
            Error::tool(
                "subagent",
                format!("Cannot read agent definition {}: {error}", path.display()),
            )
        })?;
        let (frontmatter, body) = parse_frontmatter(&raw);
        let name = required_agent_field(&frontmatter, "name", &path)?;
        let description = required_agent_field(&frontmatter, "description", &path)?;
        let tools = frontmatter.get("tools").map(|value| split_csv(value));
        let definition_dir = path.parent().unwrap_or(directory);
        let skills = frontmatter
            .get("skills")
            .map(|value| {
                split_csv(value)
                    .into_iter()
                    .map(PathBuf::from)
                    .map(|skill| {
                        if skill.is_absolute() {
                            skill
                        } else {
                            definition_dir.join(skill)
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        agents.insert(
            name.clone(),
            AgentDefinition {
                name,
                description,
                model: frontmatter.get("model").cloned(),
                reasoning: frontmatter
                    .get("reasoning")
                    .or_else(|| frontmatter.get("thinking"))
                    .cloned(),
                tools,
                skills,
                system_prompt: body,
                source,
                file_path: path,
            },
        );
    }
    Ok(())
}

fn required_agent_field(
    fields: &BTreeMap<String, String>,
    field: &str,
    path: &Path,
) -> Result<String> {
    fields
        .get(field)
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            Error::tool(
                "subagent",
                format!(
                    "Agent definition {} requires frontmatter field {field:?}",
                    path.display()
                ),
            )
        })
}

fn split_csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn parse_frontmatter(raw: &str) -> (BTreeMap<String, String>, String) {
    let mut lines = raw.lines();
    if !matches!(lines.next(), Some(first) if first.trim().eq("---")) {
        return (BTreeMap::new(), raw.to_string());
    }
    let mut fields = BTreeMap::new();
    let mut body = Vec::new();
    let mut closed = false;
    for line in lines.by_ref() {
        if line.trim().eq("---") {
            closed = true;
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = trimmed.split_once(':') {
            let key = key.trim();
            if !key.is_empty() {
                fields.insert(
                    key.to_string(),
                    value
                        .trim()
                        .trim_matches('"')
                        .trim_matches('\'')
                        .to_string(),
                );
            }
        }
    }
    if !closed {
        return (BTreeMap::new(), raw.to_string());
    }
    body.extend(lines);
    (fields, body.join("\n"))
}

struct ChildRunner {
    cwd: PathBuf,
    global_dir: PathBuf,
    child_binary: PathBuf,
}

impl ChildRunner {
    const fn new(cwd: PathBuf, global_dir: PathBuf, child_binary: PathBuf) -> Self {
        Self {
            cwd,
            global_dir,
            child_binary,
        }
    }

    async fn run_one(
        &self,
        agents: &BTreeMap<String, AgentDefinition>,
        task: SubagentTask,
        step: Option<usize>,
        on_update: Option<UpdateCallback>,
    ) -> SubagentResult {
        let Some(agent) = agents.get(&task.agent) else {
            return SubagentResult::unknown(task, step);
        };
        let cwd = task.cwd.clone().unwrap_or_else(|| self.cwd.clone());
        if !cwd.is_dir() {
            return SubagentResult::failed(
                agent,
                task,
                step,
                format!("Working directory does not exist: {}", cwd.display()),
            );
        }
        let args = child_args(agent, &task.task);
        let mut result =
            SubagentResult::starting(agent, task, step, &self.child_binary, &cwd, &args);
        let update = on_update.as_ref();
        emit_progress(update, &result);

        let mut command = Command::new(&self.child_binary);
        command
            .args(&args)
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // `Command` inherits the rest of the parent environment, including API/router/auth
            // variables and `PI_CODING_AGENT_DIR`; set this explicitly for auditability.
            .env("PI_CODING_AGENT_DIR", &self.global_dir)
            .env("PI_SUBAGENT_PARENT_PID", std::process::id().to_string())
            .env("PI_SUBAGENT_DEPTH", child_depth().to_string());

        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                result.fail(format!(
                    "Failed to launch {}: {error}",
                    self.child_binary.display()
                ));
                emit_progress(update, &result);
                return result;
            }
        };
        let mut child = ChildProcessGuard::new(child);
        result.pid = Some(child.id());
        result.status = SubagentStatus::Running;
        emit_progress(update, &result);

        if !child.has_stdout() {
            result.fail("Child stdout was not piped.".to_string());
            return result;
        }
        let stdout = child.take_stdout().expect("stdout checked above");
        let stderr = child.take_stderr().expect("stderr is piped");
        let (tx, rx) = mpsc::sync_channel(256);
        let stdout_thread = spawn_pipe_reader(stdout, PipeKind::Stdout, tx.clone());
        let stderr_thread = spawn_pipe_reader(stderr, PipeKind::Stderr, tx);
        let mut saw_cancellation = false;
        let cx = AgentCx::for_current_or_request();

        loop {
            drain_child_frames(&rx, &mut result, update);
            match child.try_wait() {
                Ok(Some(status)) => {
                    result.exit_code = status.code();
                    break;
                }
                Ok(None) => {}
                Err(error) => {
                    result.fail(format!("Failed while waiting for child: {error}"));
                    child.terminate();
                    break;
                }
            }
            if cx.checkpoint().is_err() {
                saw_cancellation = true;
                result.status = SubagentStatus::Cancelled;
                result.error = Some("Parent cancellation propagated to child process.".to_string());
                result.is_error = true;
                child.terminate();
                break;
            }
            let now = cx
                .cx()
                .timer_driver()
                .map_or_else(asupersync::time::wall_now, |timer| timer.now());
            asupersync::time::sleep(now, Duration::from_millis(10)).await;
        }

        drain_until_reader_exit(rx, &mut result, update, stdout_thread, stderr_thread).await;
        if !saw_cancellation && !matches!(result.status, SubagentStatus::Failed) {
            if result.exit_code == Some(0) {
                result.status = SubagentStatus::Completed;
            } else {
                result.status = SubagentStatus::Failed;
                result.is_error = true;
                result.error.get_or_insert_with(|| {
                    format!("Child exited with code {}.", result.exit_code.unwrap_or(-1))
                });
            }
        }
        child.disarm();
        emit_progress(update, &result);
        result
    }
}

/// Owns a spawned child until it has been reaped.  The parent agent's abort
/// path drops tool futures, so this guard is the final cancellation boundary:
/// a dropped subagent future cannot leave a Rust Pi child running.
struct ChildProcessGuard {
    child: Option<std::process::Child>,
}

impl ChildProcessGuard {
    const fn new(child: std::process::Child) -> Self {
        Self { child: Some(child) }
    }

    fn id(&self) -> u32 {
        self.child.as_ref().map_or(0, std::process::Child::id)
    }

    fn has_stdout(&self) -> bool {
        self.child
            .as_ref()
            .is_some_and(|child| child.stdout.is_some())
    }

    fn take_stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.child.as_mut().and_then(|child| child.stdout.take())
    }

    fn take_stderr(&mut self) -> Option<std::process::ChildStderr> {
        self.child.as_mut().and_then(|child| child.stderr.take())
    }

    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.child
            .as_mut()
            .map_or(Ok(None), std::process::Child::try_wait)
    }

    fn terminate(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn disarm(&mut self) {
        let _ = self.child.take();
    }
}

impl Drop for ChildProcessGuard {
    fn drop(&mut self) {
        self.terminate();
    }
}

fn child_args(agent: &AgentDefinition, task: &str) -> Vec<OsString> {
    let mut args = vec![
        "--mode".into(),
        "json".into(),
        "--print".into(),
        "--no-session".into(),
        "--tools".into(),
        agent
            .tools
            .as_ref()
            .map_or_else(|| DEFAULT_CHILD_TOOLS.to_string(), |tools| tools.join(","))
            .into(),
    ];
    if let Some(model) = &agent.model {
        args.extend(["--model".into(), model.clone().into()]);
    }
    if let Some(reasoning) = &agent.reasoning {
        args.extend(["--thinking".into(), reasoning.clone().into()]);
    }
    for skill in &agent.skills {
        args.extend(["--skill".into(), skill.clone().into_os_string()]);
    }
    if !agent.system_prompt.trim().is_empty() {
        args.extend([
            "--append-system-prompt".into(),
            agent.system_prompt.clone().into(),
        ]);
    }
    args.push(format!("Task: {task}").into());
    args
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

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SubagentResult {
    agent: String,
    description: Option<String>,
    task: String,
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
    session_isolation: &'static str,
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
            task: task.task,
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
            session_isolation: "ephemeral_no_session",
            is_error: false,
        }
    }

    fn unknown(task: SubagentTask, step: Option<usize>) -> Self {
        Self {
            agent: task.agent.clone(),
            description: None,
            task: task.task,
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
            session_isolation: "ephemeral_no_session",
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
            format!("## {heading}\n{body}")
        })
        .collect::<Vec<_>>()
        .join("\n\n")
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

#[derive(Debug, Clone, Copy)]
enum PipeKind {
    Stdout,
    Stderr,
}

struct PipeFrame {
    kind: PipeKind,
    line: String,
}

fn spawn_pipe_reader<R: Read + Send + 'static>(
    pipe: R,
    kind: PipeKind,
    tx: mpsc::SyncSender<PipeFrame>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let reader = BufReader::new(pipe);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if tx.send(PipeFrame { kind, line }).is_err() {
                break;
            }
        }
    })
}

fn drain_child_frames(
    rx: &Receiver<PipeFrame>,
    result: &mut SubagentResult,
    update: Option<&UpdateCallback>,
) {
    while let Ok(frame) = rx.try_recv() {
        match frame.kind {
            PipeKind::Stderr => append_bounded_line(&mut result.stderr, &frame.line),
            PipeKind::Stdout => ingest_child_event(&frame.line, result, update),
        }
    }
}

async fn drain_until_reader_exit(
    rx: Receiver<PipeFrame>,
    result: &mut SubagentResult,
    update: Option<&UpdateCallback>,
    stdout: thread::JoinHandle<()>,
    stderr: thread::JoinHandle<()>,
) {
    for _ in 0..500 {
        drain_child_frames(&rx, result, update);
        if stdout.is_finished() && stderr.is_finished() {
            break;
        }
        let now = asupersync::time::wall_now();
        asupersync::time::sleep(now, Duration::from_millis(10)).await;
    }
    drain_child_frames(&rx, result, update);
}

fn ingest_child_event(line: &str, result: &mut SubagentResult, update: Option<&UpdateCallback>) {
    let Ok(event) = serde_json::from_str::<Value>(line) else {
        append_bounded_line(&mut result.stderr, line);
        return;
    };
    match event.get("type").and_then(Value::as_str) {
        Some("message_update") => {
            if let Some(delta) = event
                .pointer("/assistantMessageEvent/delta")
                .and_then(Value::as_str)
            {
                append_bounded(&mut result.output, delta);
                emit_progress(update, result);
            }
        }
        Some("message_end") => {
            if let Some(text) = assistant_text(event.get("message")) {
                if result.output.is_empty() {
                    append_bounded_line(&mut result.output, &text);
                }
                emit_progress(update, result);
            }
        }
        Some("agent_end") => {
            if result.output.is_empty()
                && let Some(messages) = event.get("messages").and_then(Value::as_array)
            {
                for message in messages.iter().rev() {
                    if let Some(text) = assistant_text(Some(message)) {
                        append_bounded_line(&mut result.output, &text);
                        break;
                    }
                }
            }
        }
        _ => {}
    }
}

fn assistant_text(message: Option<&Value>) -> Option<String> {
    let message = message?;
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let content = message.get("content")?.as_array()?;
    content.iter().find_map(|block| {
        (block.get("type").and_then(Value::as_str) == Some("text"))
            .then(|| {
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .flatten()
    })
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
        let agents = discover_agents_with_roots(&cwd, &global, AgentScope::Both).expect("discover");
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
            source: AgentSource::User,
            file_path: PathBuf::from("/tmp/scout.md"),
        };
        let args = child_args(&agent, "inspect provider")
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
    }

    #[test]
    fn request_requires_exactly_one_mode_and_renders_chain_context() {
        let invalid: SubagentRequest = serde_json::from_value(json!({
            "agent": "scout", "task": "x", "tasks": [{"agent": "review", "task": "y"}]
        }))
        .expect("parse");
        assert!(invalid.mode().is_err());
        let task = SubagentTask {
            agent: "review".to_string(),
            task: "review {previous}".to_string(),
            cwd: None,
        };
        assert_eq!(
            task.with_rendered_previous("evidence").task,
            "review evidence"
        );
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
printf '{"type":"message_update","assistantMessageEvent":{"delta":"streamed:"}}\n'
printf '{"type":"message_update","assistantMessageEvent":{"delta":"%s"}}\n' "$PI_CODING_AGENT_DIR"
printf '{"type":"agent_end","messages":[{"role":"assistant","content":[{"type":"text","text":"final child result"}]}]}\n'
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
        let output = runtime
            .block_on(tool.execute(
                "subagent-fixture",
                json!({"agent": "scout", "task": "verify child protocol"}),
                None,
            ))
            .expect("child execution succeeds");

        let ContentBlock::Text(text) = &output.content[0] else {
            panic!("expected text output");
        };
        assert!(text.text.contains("streamed:"));
        assert!(text.text.contains(global_dir.to_string_lossy().as_ref()));
        assert!(
            output.details.as_ref().is_some_and(|details| {
                details["results"][0]["binary"] == Value::String(child.display().to_string())
                    && details["results"][0]["status"] == "completed"
                    && details["sessionIsolation"] == "ephemeral_no_session"
            }),
            "missing child-process evidence: {output:?}"
        );
    }
}
