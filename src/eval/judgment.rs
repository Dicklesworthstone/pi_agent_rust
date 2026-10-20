//! Invocation-owned typed judgments over the ordinary eval bridge.
//!
//! A model must be selected explicitly by the host or by this cell's
//! judge_model field. Neither interpreter holds credentials, a privacy vault,
//! a provider handle or a judgment task that can outlive the cell.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Value, json};

use crate::agent_cx::AgentCx;
use crate::auth::AuthStorage;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::judgment::{Judge, JudgmentRequest, Protect};
use crate::models::ModelRegistry;
use crate::provider::StreamOptions;
use crate::tools::ToolOutput;

mod batch;

pub(super) const BRIDGE_NAME: &str = "__pi_judge";
const MAX_CALLS: usize = 16;

#[derive(Clone, Debug, Serialize)]
struct Selection {
    provider: String,
    model: String,
}

impl Selection {
    fn parse(value: &Value) -> Result<Self> {
        let fields = value.as_object().filter(|fields| fields.len() == 2)
            .ok_or_else(input_error)?;
        let provider = fields.get("provider").and_then(Value::as_str).ok_or_else(input_error)?;
        let model = fields.get("model").and_then(Value::as_str).ok_or_else(input_error)?;
        if [provider, model].into_iter().any(|part| {
            part.is_empty() || part.len() > 256 || part.trim() != part
                || part.chars().any(char::is_control)
        }) {
            return Err(input_error());
        }
        // Bound borrowed names before making any copy of guest-controlled data.
        Ok(Self { provider: provider.to_owned(), model: model.to_owned() })
    }
}

fn input_error() -> Error {
    Error::tool("eval", "EVAL_INPUT: judge_model requires exactly nonempty provider and model strings (at most 256 bytes each)")
}

fn binding_error() -> Error {
    Error::tool("judge", "PI_JUDGE_BINDING: selected model, trusted configuration or credentials unavailable; no fallback")
}

/// Only non-secret model names may come from the cell. Model endpoints,
/// credentials and request headers come from the host's existing registry.
fn resolve(cwd: &Path, selection: &Selection) -> Result<Judge> {
    let global_dir = Config::global_dir();
    let override_path = std::env::var_os("PI_CONFIG_PATH").map(PathBuf::from);
    let global = Config::load_with_roots_and_project_trust(
        override_path.as_deref(), &global_dir, cwd, false,
    ).map_err(|_| binding_error())?;
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
    ).map_err(|_| binding_error())?;
    let config = Config::load_with_roots_and_project_trust(
        override_path.as_deref(), &global_dir, cwd, trust.trusted,
    ).map_err(|_| binding_error())?;
    if config.disabled_providers.as_ref().is_some_and(|providers| providers.iter().any(|provider| {
        crate::provider_metadata::provider_ids_match(provider, &selection.provider)
    })) {
        return Err(binding_error());
    }
    let mut auth = AuthStorage::load(Config::auth_path()).map_err(|_| binding_error())?;
    auth.set_command_working_directory(cwd).map_err(|_| binding_error())?;
    let registry = ModelRegistry::load(&auth, None);
    let entry = registry.find(&selection.provider, &selection.model)
        .ok_or_else(binding_error)?;
    if config.disabled_providers.as_ref().is_some_and(|providers| providers.iter().any(|provider| {
        crate::provider_metadata::provider_ids_match(provider, &entry.model.provider)
    })) {
        return Err(binding_error());
    }
    let api_key = crate::models::normalize_api_key_opt(
        auth.resolve_api_key(&entry.model.provider, None),
    ).or_else(|| crate::models::normalize_api_key_opt(entry.api_key.clone()));
    if crate::models::model_requires_configured_credential(&entry) && api_key.is_none() {
        return Err(binding_error());
    }
    let options = StreamOptions {
        api_key,
        headers: entry.headers.clone(),
        max_tokens: Some(entry.model.max_tokens.min(4096)),
        ..StreamOptions::default()
    };
    let provider = crate::providers::create_provider_with_auth(&entry, None, Some(&auth))
        .map_err(|_| binding_error())?;
    Ok(Judge::new(provider, options).with_secrets_settings(config.secrets.as_ref()))
}

/// Deliberately borrowed through the native runner. Never retain the Agent's
/// live privacy callback in a shared tool, interpreter realm or background task.
pub(super) struct Invocation<'p> {
    judge: Option<Judge>,
    selection: Option<Selection>,
    protect: Option<&'p Protect<'p>>,
    receipts: Vec<Value>,
}

impl<'p> Invocation<'p> {
    pub(super) fn new(
        judge: Option<Judge>,
        input: &Value,
        protect: Option<&'p Protect<'p>>,
    ) -> Result<Self> {
        let selection = input.get("judge_model").map(Selection::parse).transpose()?;
        if judge.is_some() && selection.is_some() {
            return Err(Error::tool("eval", "EVAL_INPUT: judge_model cannot replace a host-bound Judge"));
        }
        Ok(Self { judge, selection, protect, receipts: Vec::new() })
    }

    pub(super) async fn dispatch(
        &mut self,
        tool: &super::EvalTool,
        name: &str,
        input: Value,
    ) -> std::result::Result<String, String> {
        if name == BRIDGE_NAME {
            self.evaluate(&tool.cwd, input).await
        } else if name == batch::BRIDGE_NAME {
            self.evaluate_batch(&tool.cwd, input).await
        } else {
            // Check the raw operation before structured-result unwrapping.
            // Nested tool.result calls cannot expand the normal tool whitelist.
            tool.run_bridge_call_parts(name, input).await
        }
    }

    pub(super) async fn evaluate(&mut self, cwd: &Path, input: Value) -> std::result::Result<String, String> {
        let request = JudgmentRequest::parse(input).map_err(public_error)?;
        if self.judge.is_none() && self.selection.is_none() {
            return Err("PI_JUDGE_UNBOUND: supply this cell's judge_model or bind a Judge in the host".into());
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
            "selectedModel": self.selection,
            "status": "incomplete",
            "usage": null,
            "usageKnown": false,
        }));
        let outcome = self.evaluate_one(cwd, request).await;
        match outcome {
            Ok(verdict) => {
                self.receipts[index] = json!({
                    "attempt": index + 1,
                    "status": "completed",
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

    async fn evaluate_one(&mut self, cwd: &Path, request: JudgmentRequest) -> Result<Value> {
        self.bound_judge(cwd).await?
            .evaluate_protected(request, self.protect).await
    }

    // Resolve once per invocation, not once per batch item. Cloning a Judge
    // shares the existing provider, never credentials or privacy with a guest.
    async fn bound_judge(&mut self, cwd: &Path) -> Result<Judge> {
        let owner = AgentCx::for_current_or_request();
        owner.checkpoint().map_err(|_| Error::tool("judge", "PI_JUDGE_CANCELLED"))?;
        if !owner.capabilities().io || !owner.capabilities().time {
            return Err(Error::tool("judge", "PI_JUDGE_PERMISSION"));
        }
        if self.judge.is_none() {
            if !owner.capabilities().spawn {
                return Err(Error::tool("judge", "PI_JUDGE_PERMISSION"));
            }
            // Credential/configuration resolution may use disk or a configured
            // key command. Do not occupy the executor or pass it the payload.
            // Dropping the wait may finish local resolution, never generation.
            let cwd = cwd.to_path_buf();
            let selection = self.selection.clone().ok_or_else(binding_error)?;
            let resolver_owner = owner.clone();
            let judge = asupersync::runtime::spawn_blocking_io(move || {
                let _current = resolver_owner.cx().clone().set_current_restricted();
                resolver_owner.checkpoint().map_err(|_| binding_error())?;
                resolve(&cwd, &selection)
            }).await?;
            owner.checkpoint().map_err(|_| Error::tool("judge", "PI_JUDGE_CANCELLED"))?;
            self.judge = Some(judge);
        }
        self.judge.clone().ok_or_else(binding_error)
    }

    /// Keep receipts outside guest stdout. A cell may discard a verdict or
    /// catch its error; neither should conceal the auxiliary request. Failed
    /// and cancelled calls have unknown usage, never an invented zero bill.
    pub(super) fn finish(&self, outcome: Result<ToolOutput>) -> Result<ToolOutput> {
        if self.receipts.is_empty() {
            return outcome;
        }
        let receipts = json!({"schema":"pi.eval.judgments.v1", "attempts":self.receipts});
        match outcome {
            Ok(mut output) => {
                let details = output.details.get_or_insert_with(|| json!({}));
                let details = details.as_object_mut().ok_or_else(|| {
                    Error::tool("eval", "EVAL_JUDGE_RECEIPT: result details are not an object")
                })?;
                details.insert("judgments".into(), receipts);
                Ok(output)
            }
            Err(error) => Err(Error::tool("eval", format!("{error}; EVAL_JUDGMENTS: {receipts}"))),
        }
    }
}

// Foreign callback/provider/configuration errors are not authority to echo
// request state, prompt fragments, authentication headers or command output.
fn public_error(error: Error) -> String {
    let text = error.to_string();
    for code in [
        "PI_SECRET_BLOCK", "PI_AUXILIARY_INPUT_LIMIT", "PI_AUXILIARY_KEY_COLLISION",
        "PI_JUDGE_INPUT", "PI_JUDGE_BINDING", "PI_JUDGE_PRIVACY", "PI_JUDGE_PERMISSION",
        "PI_JUDGE_CANCELLED", "PI_JUDGE_TIMEOUT", "PI_JUDGE_RESPONSE", "PI_JUDGE_PROVIDER",
    ] {
        if text.contains(code) {
            return format!("{code}: judgment failed; no verdict");
        }
    }
    "PI_JUDGE_FAILED: judgment failed; no verdict".into()
}

#[cfg(test)]
mod tests;
