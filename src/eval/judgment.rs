//! Invocation-owned host calls and typed judgments over the eval bridge.
//!
//! A model must be selected explicitly by the host or by this cell's
//! judge_model field. Neither interpreter holds credentials, a privacy vault,
//! a provider handle. Explicit background starts transfer only prepared,
//! tool-free requests into the native session-owned jobs registry.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Value, json};

use crate::agent_cx::AgentCx;
use crate::auth::AuthStorage;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::judgment::{Judge, JudgmentRequest, Protect};
use crate::models::{ModelEntry, ModelRegistry, ModelRole};
use crate::provider::StreamOptions;
use crate::tools::{ToolInvocationHost, ToolOutput};

mod background;
mod batch;
mod completion;
mod drain;
mod images;

pub(super) const BRIDGE_NAME: &str = "__pi_judge";
const MAX_CALLS: usize = 16;
const MAX_TOOL_CALLS: usize = 64;

pub(super) type SnapshotPrivacy<'a> =
    dyn Fn() -> crate::text_completion::AuxiliaryPrivacySnapshot + Send + Sync + 'a;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
enum Selection {
    Named { provider: String, model: String },
    Role { role: String },
}

impl Selection {
    fn parse(value: &Value) -> Result<Self> {
        let fields = value.as_object().ok_or_else(input_error)?;
        if fields.len() == 1 && fields.contains_key("role") {
            return Self::role(&fields["role"]).ok_or_else(input_error);
        }
        if fields.len() != 2 {
            return Err(input_error());
        }
        let provider = fields
            .get("provider")
            .and_then(Value::as_str)
            .ok_or_else(input_error)?;
        let model = fields
            .get("model")
            .and_then(Value::as_str)
            .ok_or_else(input_error)?;
        if [provider, model].into_iter().any(|part| {
            part.is_empty()
                || part.len() > 256
                || part.trim() != part
                || part.chars().any(char::is_control)
        }) {
            return Err(input_error());
        }
        // Bound borrowed names before making any copy of guest-controlled data.
        Ok(Self::Named {
            provider: provider.to_owned(),
            model: model.to_owned(),
        })
    }

    fn role(value: &Value) -> Option<Self> {
        // Canonical names only: do not trim, normalize an arbitrary string or
        // interpret a provider/model selector supplied by interpreter code.
        let name = value.as_str()?;
        let role = ModelRole::ALL
            .into_iter()
            .find(|role| role.as_str() == name)?;
        Some(Self::Role {
            role: role.as_str().to_owned(),
        })
    }
}

fn input_error() -> Error {
    Error::tool(
        "eval",
        "EVAL_INPUT: judge_model requires either provider and model strings (at most 256 bytes each) or one canonical native role",
    )
}

fn binding_error() -> Error {
    Error::tool(
        "judge",
        "PI_JUDGE_BINDING: selected model, trusted configuration or credentials unavailable; no fallback",
    )
}

/// Strict settings lookup shared by named and role selections. Auxiliary
/// requests must never auto-select a paid model when a requested role is
/// missing or misspelled. Role effort suffixes use the registered-model
/// resolver already used by explicit native model handoffs.
fn resolve_entry(
    cwd: &Path,
    selection: &Selection,
    config: &Config,
    registry: &ModelRegistry,
) -> Result<(ModelEntry, Option<crate::model::ThinkingLevel>)> {
    let scope = config
        .model_scope_overrides
        .as_deref()
        .and_then(|overrides| crate::failover::best_scope_override(overrides, cwd));
    let disabled = |provider: &str| {
        crate::failover::provider_is_disabled(
            config.disabled_providers.as_deref().unwrap_or_default(),
            scope,
            provider,
        )
    };
    let (entry, thinking) = match selection {
        Selection::Named { provider, model } => {
            if disabled(provider) {
                return Err(binding_error());
            }
            (
                registry.find(provider, model).ok_or_else(binding_error)?,
                None,
            )
        }
        Selection::Role { role } => {
            let role = ModelRole::from_name(role).ok_or_else(binding_error)?;
            let spec = config
                .model_roles
                .as_ref()
                .and_then(|roles| crate::app::role_spec_from_settings(roles, role))
                .ok_or_else(binding_error)?;
            let target = crate::app::resolve_prewalk_model_spec(spec, config, registry, cwd)
                .map_err(|_| binding_error())?;
            let entry = registry
                .find(&target.provider, &target.model)
                .ok_or_else(binding_error)?;
            let thinking = target
                .thinking
                .map(|level| entry.clamp_thinking_level(level));
            (entry, thinking)
        }
    };
    if disabled(&entry.model.provider) {
        return Err(binding_error());
    }
    Ok((entry, thinking))
}

/// Only non-secret model names may come from the cell. Model endpoints,
/// credentials and request headers come from the host's existing registry.
fn resolve(cwd: &Path, selection: &Selection) -> Result<Judge> {
    let global_dir = Config::global_dir();
    let override_path = std::env::var_os("PI_CONFIG_PATH").map(PathBuf::from);
    let global = Config::load_with_roots_and_project_trust(
        override_path.as_deref(),
        &global_dir,
        cwd,
        false,
    )
    .map_err(|_| binding_error())?;
    let trust = crate::workspace_trust::establish(
        cwd,
        &global_dir.join(crate::workspace_trust::TRUST_STORE_FILE),
        &crate::workspace_trust::TrustInputs {
            cli_trust: false,
            trust_all_workspaces: global.trust_all_workspaces.unwrap_or(false),
            env_override: std::env::var(crate::workspace_trust::TRUST_ENV_VAR).ok(),
            interactive: false,
        },
        |_| Err(binding_error()),
    )
    .map_err(|_| binding_error())?;
    let config = Config::load_with_roots_and_project_trust(
        override_path.as_deref(),
        &global_dir,
        cwd,
        trust.trusted,
    )
    .map_err(|_| binding_error())?;
    let mut auth = AuthStorage::load(Config::auth_path()).map_err(|_| binding_error())?;
    auth.set_command_working_directory(cwd)
        .map_err(|_| binding_error())?;
    let registry = ModelRegistry::load(&auth, None);
    let (entry, thinking_level) = resolve_entry(cwd, selection, &config, &registry)?;
    let api_key =
        crate::models::normalize_api_key_opt(auth.resolve_api_key(&entry.model.provider, None))
            .or_else(|| crate::models::normalize_api_key_opt(entry.api_key.clone()));
    if crate::models::model_requires_configured_credential(&entry) && api_key.is_none() {
        return Err(binding_error());
    }
    let options = StreamOptions {
        api_key,
        headers: entry.headers.clone(),
        max_tokens: Some(entry.model.max_tokens.min(4096)),
        thinking_level,
        ..StreamOptions::default()
    };
    let provider = crate::providers::create_provider_with_auth(&entry, None, Some(&auth))
        .map_err(|_| binding_error())?;
    Ok(Judge::new(provider, options)
        .with_image_support(
            entry
                .model
                .input
                .contains(&crate::provider::InputType::Image),
        )
        .with_secrets_settings(config.secrets.as_ref()))
}

/// Deliberately borrowed through the native runner. Never retain the Agent's
/// live privacy callback in a shared tool, interpreter realm or background task.
pub(super) struct Invocation<'p> {
    /// An SDK-provided Judge is a fixed authority ceiling, never a cache slot.
    judge: Option<Judge>,
    cost_budget: Option<crate::cost::CostBudget>,
    selection: Option<Selection>,
    /// Per-cell bindings only. Retained jobs clone their selected native Judge;
    /// interpreter handles never carry provider or credential objects.
    resolved: Vec<(Selection, Judge)>,
    protect: Option<&'p Protect<'p>>,
    receipts: Vec<Value>,
    host: Option<&'p dyn ToolInvocationHost>,
    host_session: Option<String>,
    tool_receipts: Vec<Value>,
    snapshot_privacy: Option<&'p SnapshotPrivacy<'p>>,
}

impl<'p> Invocation<'p> {
    pub(super) fn new(
        judge: Option<Judge>,
        input: &Value,
        protect: Option<&'p Protect<'p>>,
    ) -> Result<Self> {
        let selection = input.get("judge_model").map(Selection::parse).transpose()?;
        if judge.is_some() && selection.is_some() {
            return Err(Error::tool(
                "eval",
                "EVAL_INPUT: judge_model cannot replace a host-bound Judge",
            ));
        }
        Ok(Self {
            judge,
            cost_budget: None,
            selection,
            resolved: Vec::new(),
            protect,
            receipts: Vec::new(),
            host: None,
            host_session: None,
            tool_receipts: Vec::new(),
            snapshot_privacy: None,
        })
    }

    pub(super) fn with_host(mut self, host: Option<&'p dyn ToolInvocationHost>) -> Self {
        self.host = host;
        self
    }

    pub(super) fn with_cost_budget(mut self, budget: Option<crate::cost::CostBudget>) -> Self {
        self.cost_budget = budget;
        self
    }

    pub(super) fn with_snapshot_privacy(
        mut self,
        snapshot: Option<&'p SnapshotPrivacy<'p>>,
    ) -> Self {
        self.snapshot_privacy = snapshot;
        self
    }

    pub(super) fn bind_host_session(&mut self, session: &str) {
        self.host_session = Some(session.to_owned());
    }

    /// Remove the optional helper model-role selector before the ordinary
    /// request validators run. It cannot replace an SDK binding. A cell still
    /// needs judge_model admission even when each request selects its own role;
    /// this preserves EvalTool's advertised network effects before execution.
    fn request_selection(&self, input: &mut Value) -> Result<Option<Selection>> {
        let role = input
            .as_object_mut()
            .and_then(|fields| fields.remove("model"));
        match role {
            None => Ok(self.selection.clone()),
            Some(role) => {
                let selection = Selection::role(&role).ok_or_else(|| {
                    Error::tool(
                        "judge",
                        "PI_JUDGE_INPUT: model must name a canonical native role",
                    )
                })?;
                if self.judge.is_some() {
                    return Err(binding_error());
                }
                Ok(Some(selection))
            }
        }
    }

    pub(super) async fn dispatch(
        &mut self,
        tool: &super::EvalTool,
        name: &str,
        input: Value,
    ) -> std::result::Result<String, String> {
        if name == drain::BRIDGE_NAME {
            self.drain_batch(tool, input).await
        } else if let Some(operation) = background::Operation::for_bridge(name) {
            self.start_background(tool, operation, input).await
        } else if name == BRIDGE_NAME {
            self.evaluate(&tool.cwd, input).await
        } else if name == batch::BRIDGE_NAME {
            self.evaluate_batch(&tool.cwd, input).await
        } else if name == completion::BRIDGE_NAME {
            self.complete(&tool.cwd, input).await
        } else if let Some(host) = self.host {
            self.call_host(host, name, input).await
        } else {
            // Check the raw operation before structured-result unwrapping.
            // Nested tool.result calls cannot expand the normal tool whitelist.
            tool.run_bridge_call_parts(name, input).await
        }
    }

    async fn call_host(
        &mut self,
        host: &dyn ToolInvocationHost,
        name: &str,
        input: Value,
    ) -> std::result::Result<String, String> {
        let request = super::bridge_result::Request::parse(name, input)?;
        let output = self
            .call_host_output(host, request.name, request.args)
            .await?;
        if request.structured {
            super::bridge_result::structured(&output)
        } else {
            super::bridge_result::text(&output)
        }
    }

    /// The same dispatch and receipt path serves guest calls and image reads.
    /// Image preparation consumes native content directly, so encoded image
    /// bytes need not make a redundant round trip through guest JSON framing.
    async fn call_host_output(
        &mut self,
        host: &dyn ToolInvocationHost,
        name: String,
        args: Value,
    ) -> std::result::Result<ToolOutput, String> {
        if name.is_empty()
            || name.len() > 128
            || name.chars().any(char::is_control)
            || !args.is_object()
        {
            return Err(
                "EVAL_BRIDGE_INPUT: expected a bounded tool name and object arguments".into(),
            );
        }
        if self.tool_receipts.len() >= MAX_TOOL_CALLS {
            return Err("EVAL_BRIDGE_LIMIT: at most 64 host tool calls per cell".into());
        }
        let call = crate::model::ToolCall {
            id: format!("eval-{}", uuid::Uuid::new_v4().simple()),
            name,
            arguments: args,
            thought_signature: None,
        };
        let index = self.tool_receipts.len();
        // Native receipts survive a caught guest error and partial evaluation.
        // No arguments, output bodies, credentials or host handles are retained.
        self.tool_receipts.push(json!({
            "toolCallId": call.id,
            "tool": call.name,
            "status": "incomplete",
        }));
        let session = self.host_session.as_deref().ok_or_else(|| {
            "EVAL_SESSION_UNAVAILABLE: native cell owner was not captured".to_string()
        })?;
        let outcome = host.call_tool(call, session).await;
        let receipt = self
            .tool_receipts
            .get_mut(index)
            .and_then(Value::as_object_mut)
            .ok_or_else(|| "EVAL_TOOL_RECEIPT: native call receipt was unavailable".to_string())?;
        match outcome {
            Ok(output) => {
                receipt.insert(
                    "status".into(),
                    json!(if output.is_error {
                        "failed"
                    } else {
                        "completed"
                    }),
                );
                receipt.insert("isError".into(), json!(output.is_error));
                Ok(output)
            }
            Err(error) => {
                receipt.insert("status".into(), json!("failed"));
                receipt.insert("isError".into(), json!(true));
                Err(error.to_string())
            }
        }
    }

    pub(super) async fn evaluate(
        &mut self,
        cwd: &Path,
        mut input: Value,
    ) -> std::result::Result<String, String> {
        let selection = self.request_selection(&mut input).map_err(public_error)?;
        let (request, images) = images::parse_request(input).map_err(public_error)?;
        if self.judge.is_none() && self.selection.is_none() {
            return Err(
                "PI_JUDGE_UNBOUND: supply this cell's judge_model or bind a Judge in the host"
                    .into(),
            );
        }
        if self.receipts.len() >= MAX_CALLS {
            return Err("PI_JUDGE_LIMIT: at most 16 judgment attempts per eval cell".into());
        }
        let index = self.receipts.len();
        // Install before the first await so a cell watchdog can report an
        // incomplete attempt. A caller dropping the entire eval future still
        // cannot receive a result; this is not a durable billing journal.
        self.receipts.push(json!({
            "attempt": index + 1,
            "selectedModel": selection,
            "status": "incomplete",
            "usage": null,
            "usageKnown": false,
        }));
        let outcome = self
            .evaluate_one(cwd, request, images, selection.as_ref())
            .await;
        match outcome {
            Ok(verdict) => {
                self.receipts[index] = json!({
                    "attempt": index + 1,
                    "status": "completed",
                    "selectedModel": selection,
                    "provider": verdict["provider"],
                    "modelId": verdict["modelId"],
                    "api": verdict["api"],
                    "usage": verdict["usage"],
                    "usageKnown": !verdict["usage"].is_null(),
                });
                serde_json::to_string(&verdict)
                    .map_err(|_| "PI_JUDGE_RESPONSE: could not encode verdict".to_string())
            }
            Err(error) => {
                let error = public_error(error);
                self.receipts[index]["status"] = json!("failed");
                self.receipts[index]["error"] = json!(error);
                Err(error)
            }
        }
    }

    async fn evaluate_one(
        &mut self,
        cwd: &Path,
        request: JudgmentRequest,
        images: images::Sources,
        selection: Option<&Selection>,
    ) -> Result<Value> {
        let judge = self.bound_judge(cwd, selection).await?;
        judge.check_image_support(!images.is_empty())?;
        self.check_image_reads(images.path_count())?;
        let request = self.attach_images(request, images).await?;
        judge.evaluate_protected(request, self.protect).await
    }

    // Resolve once per selected binding per invocation, not per batch item. Cloning a Judge
    // shares the existing provider, never credentials or privacy with a guest.
    async fn bound_judge(&mut self, cwd: &Path, selection: Option<&Selection>) -> Result<Judge> {
        let owner = AgentCx::for_current_or_request();
        owner
            .checkpoint()
            .map_err(|_| Error::tool("judge", "PI_JUDGE_CANCELLED"))?;
        if !owner.capabilities().io || !owner.capabilities().time {
            return Err(Error::tool("judge", "PI_JUDGE_PERMISSION"));
        }
        if let Some(judge) = &self.judge {
            return Ok(judge.clone().with_cost_budget(self.cost_budget.clone()));
        }
        let selection = selection.ok_or_else(binding_error)?;
        if let Some((_, judge)) = self.resolved.iter().find(|(key, _)| key == selection) {
            return Ok(judge.clone());
        }
        if !owner.capabilities().spawn {
            return Err(Error::tool("judge", "PI_JUDGE_PERMISSION"));
        }
        // Credential/configuration resolution may use disk or a configured
        // key command. Do not occupy the executor or pass it the payload.
        // Dropping the wait may finish local resolution, never generation.
        let cwd = cwd.to_path_buf();
        let selected = selection.clone();
        let resolver_owner = owner.clone();
        let judge = asupersync::runtime::spawn_blocking(move || {
            let _current = resolver_owner.cx().clone().set_current_restricted();
            resolver_owner.checkpoint().map_err(|_| binding_error())?;
            resolve(&cwd, &selected)
        })
        .await?
        .with_cost_budget(self.cost_budget.clone());
        owner
            .checkpoint()
            .map_err(|_| Error::tool("judge", "PI_JUDGE_CANCELLED"))?;
        self.resolved.push((selection.clone(), judge.clone()));
        Ok(judge)
    }

    /// Keep receipts outside guest stdout. A cell may discard a verdict or
    /// catch its error; neither should conceal the auxiliary request. Failed
    /// and cancelled calls have unknown usage, never an invented zero bill.
    pub(super) fn finish(&self, outcome: Result<ToolOutput>) -> Result<ToolOutput> {
        if self.receipts.is_empty() && self.tool_receipts.is_empty() {
            return outcome;
        }
        let receipts = json!({"schema":"pi.eval.judgments.v1", "attempts":self.receipts});
        let tool_receipts = json!({"schema":"pi.eval.tool_calls.v1", "calls":self.tool_receipts});
        match outcome {
            Ok(mut output) => {
                let details = output.details.get_or_insert_with(|| json!({}));
                let details = details.as_object_mut().ok_or_else(|| {
                    Error::tool(
                        "eval",
                        "EVAL_JUDGE_RECEIPT: result details are not an object",
                    )
                })?;
                if !self.receipts.is_empty() {
                    details.insert("judgments".into(), receipts);
                }
                if !self.tool_receipts.is_empty() {
                    details.insert("toolCalls".into(), tool_receipts);
                }
                Ok(output)
            }
            Err(error) => {
                let mut message = error.to_string();
                if !self.receipts.is_empty() {
                    let _ = write!(message, "; EVAL_JUDGMENTS: {receipts}");
                }
                if !self.tool_receipts.is_empty() {
                    let _ = write!(message, "; EVAL_TOOL_CALLS: {tool_receipts}");
                }
                Err(Error::tool("eval", message))
            }
        }
    }
}

// Foreign callback/provider/configuration errors are not authority to echo
// request state, prompt fragments, authentication headers or command output.
#[allow(clippy::needless_pass_by_value)] // `map_err` adapter at every call site
fn public_error(error: Error) -> String {
    if matches!(&error, Error::CostCap { .. }) {
        return format!("cost_cap: {error}");
    }
    let text = error.to_string();
    for code in [
        "PI_SECRET_BLOCK",
        "PI_AUXILIARY_INPUT_LIMIT",
        "PI_AUXILIARY_KEY_COLLISION",
        "PI_JUDGE_INPUT",
        "PI_JUDGE_BINDING",
        "PI_JUDGE_PRIVACY",
        "PI_JUDGE_PERMISSION",
        "PI_JUDGE_CANCELLED",
        "PI_JUDGE_TIMEOUT",
        "PI_JUDGE_RESPONSE",
        "PI_JUDGE_PROVIDER",
        "PI_JUDGE_IMAGE_UNSUPPORTED",
        "PI_JUDGE_IMAGE_INPUT",
        "PI_JUDGE_IMAGE_LIMIT",
        "PI_JUDGE_IMAGE_READ",
        "PI_JUDGE_IMAGE_HOST",
        "PI_JUDGE_IMAGE_CALL_LIMIT",
    ] {
        if text.contains(code) {
            return format!("{code}: judgment failed; no verdict");
        }
    }
    "PI_JUDGE_FAILED: judgment failed; no verdict".into()
}

#[cfg(test)]
mod tests;
