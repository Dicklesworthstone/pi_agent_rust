//! Explicit task destinations are resolved before publishing work. Keep only
//! the destination and effort in queued requests, never registry credentials.

use super::{
    AgentDefinition, BTreeMap, Config, Deadline, Error, Path, Result, SubagentTask, SubagentTool,
    Value,
};
use crate::agent_cx::AgentCx;
use crate::auth::AuthStorage;
use crate::model::ThinkingLevel;
use crate::models::{ModelEntry, ModelRegistry, ModelRole};
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;

/// A role is explicit syntax, never an interpretation of a literal model ID.
/// Resolve it from the host's trusted settings once at request admission.
#[derive(Debug, Clone)]
pub(super) enum Selector {
    Exact(String),
    Role(ModelRole),
}

impl Selector {
    fn parse(value: &Value) -> Result<Self> {
        if let Some(spec) = value.as_str() {
            validate_selector(spec)?;
            return Ok(Self::Exact(spec.to_string()));
        }
        if let Some(fields) = value.as_object()
            && fields.len() == 1
            && let Some(name) = fields.get("role").and_then(Value::as_str)
            && let Some(role) = ModelRole::ALL
                .into_iter()
                .find(|role| role.as_str() == name)
        {
            return Ok(Self::Role(role));
        }
        Err(invalid(
            "model must be an exact model string or an object containing only one canonical role name",
        ))
    }

    fn validate(&self) -> Result<()> {
        match self {
            Self::Exact(spec) => validate_selector(spec),
            Self::Role(_) => Ok(()),
        }
    }

    const fn fallback_role(&self) -> ModelRole {
        match self {
            Self::Exact(_) => ModelRole::Default,
            Self::Role(role) => *role,
        }
    }
}

impl<'de> Deserialize<'de> for Selector {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        Self::parse(&Value::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

pub(super) fn schema() -> Value {
    json!({
        "anyOf": [
            {"type":"string", "minLength":1, "maxLength":512},
            {"type":"object", "required":["role"], "additionalProperties":false,
             "properties":{"role":{"type":"string", "enum":ModelRole::ALL.map(ModelRole::as_str)}}}
        ],
        "description":"Explicit destination for one delegation, worker_start or pool_start: exact provider/model[:thinking], an unambiguous exact model ID, or {role: canonicalName} from trusted modelRoles settings. An explicit role must be configured; it never falls back to default role or model selection. Overrides definition/task-role/parent defaults. Unknown, unavailable, disabled or unsupported destinations fail before launch. Put model on each tasks/chain entry or workflow call. Queued tasks, retries and workers/pools freeze the resolved destination and role-specific fallback policy. Seance accepts only the exact-model string form."
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Selection {
    pub(super) provider: String,
    pub(super) model: String,
    pub(super) thinking: Option<ThinkingLevel>,
}

fn invalid(message: &str) -> Error {
    Error::tool("subagent", format!("PI_SUBAGENT_MODEL: {message}"))
}

fn validate_selector(spec: &str) -> Result<()> {
    if spec.is_empty()
        || spec.len() > 512
        || spec.chars().any(|ch| ch.is_whitespace() || ch.is_control())
        || spec.starts_with(['-', '@', '/'])
        || spec.contains(['*', '?', '[', ']', ','])
        || matches!(spec, "default" | "inherit")
    {
        return Err(invalid(
            "model strings must be an exact provider/model[:thinking] or unambiguous model ID, at most 512 bytes; use an object with role for configured roles, not aliases, patterns or lists",
        ));
    }
    Ok(())
}

fn field(value: &Value) -> Result<Option<Selector>> {
    value.get("model").map(Selector::parse).transpose()
}

/// Serde maps explicit null to None; validate before deserialization so an
/// SDK caller cannot silently lose an override by bypassing the tool schema.
pub(super) fn validate_request(input: &Value) -> Result<()> {
    let model = field(input)?;
    for key in ["tasks", "chain"] {
        if let Some(tasks) = input.get(key).and_then(Value::as_array) {
            if model.is_some() {
                return Err(invalid(
                    "model belongs on each tasks/chain entry, not the batch",
                ));
            }
            for task in tasks {
                field(task)?;
            }
        }
    }
    Ok(())
}

fn check(owner: &AgentCx, deadline: Deadline) -> Result<()> {
    owner
        .checkpoint()
        .map_err(|_| invalid("parent was cancelled before model admission"))?;
    deadline
        .check()
        .map_err(|message| Error::tool("subagent", message))?;
    let capabilities = owner.capabilities();
    if !capabilities.io || !capabilities.spawn || !capabilities.time {
        return Err(Error::tool(
            "subagent",
            "PI_SUBAGENT_PERMISSION: model admission requires I/O, spawn and timer capabilities",
        ));
    }
    Ok(())
}

struct Admission {
    config: Config,
    registry: ModelRegistry,
}

impl Admission {
    fn load(tool: &SubagentTool, config: Config, deadline: Deadline) -> Result<Self> {
        let mut auth = AuthStorage::load_with_lock_timeout(
            tool.global_dir.join("auth.json"),
            deadline.remaining().min(Duration::from_secs(1)),
        )
        .map_err(|_| invalid("could not load the configured credential store"))?;
        auth.set_command_working_directory(&tool.cwd)
            .map_err(|_| invalid("could not establish the credential command directory"))?;
        // Listing mode includes native, manual and validated persisted catalog
        // entries without remote discovery. Its credential snapshot resolves
        // each provider once; do not rerun credential helpers with a second
        // readiness lookup. Extensions cannot be loaded safely at admission.
        let registry =
            ModelRegistry::load_for_listing(&auth, Some(tool.global_dir.join("models.json")));
        if registry.error().is_some() {
            return Err(invalid(
                "could not load the model catalog; inspect models.json and the persisted catalog with pi --list-models",
            ));
        }
        Ok(Self { config, registry })
    }

    fn resolve_selector(&self, selector: &Selector, host: &Path, cwd: &Path) -> Result<Selection> {
        let spec = match selector {
            Selector::Exact(spec) => spec.as_str(),
            Selector::Role(role) => self
                .config
                .model_roles
                .as_ref()
                .and_then(|roles| crate::app::role_spec_from_settings(roles, *role))
                .map(str::trim)
                .ok_or_else(|| {
                    invalid(
                        "the requested role has no explicit trusted modelRoles assignment; no default-model fallback",
                    )
                })?,
        };
        // Use the same exact-ID-first admission as string selectors. A role
        // does not bypass ambiguity, supported effort, credentials or either
        // workspace's provider restrictions, nor clamp an unsupported effort.
        self.resolve(spec, host, cwd)
    }

    fn resolve(&self, spec: &str, host: &Path, cwd: &Path) -> Result<Selection> {
        validate_selector(spec)?;
        // A registered literal ID wins before parsing a suffix. Local models
        // may legitimately contain colons, including tags such as :1 or :high.
        let (entry, thinking) = if let Some(entry) = exact(&self.registry, spec)? {
            (entry, None)
        } else if let Some((model, suffix)) = spec.rsplit_once(':') {
            let thinking = suffix.parse::<ThinkingLevel>().map_err(|_| invalid(
                "unknown model or invalid thinking suffix; use an exact catalog ID and off|minimal|low|medium|high|xhigh|max",
            ))?;
            let entry = exact(&self.registry, model)?.ok_or_else(unknown)?;
            if !entry.available_thinking_levels().contains(&thinking) {
                return Err(invalid(
                    "the selected model does not support the requested thinking level",
                ));
            }
            (entry, Some(thinking))
        } else {
            return Err(unknown());
        };
        for directory in [host, cwd] {
            let scope = self
                .config
                .model_scope_overrides
                .as_deref()
                .and_then(|overrides| crate::failover::best_scope_override(overrides, directory));
            if crate::failover::provider_is_disabled(
                self.config
                    .disabled_providers
                    .as_deref()
                    .unwrap_or_default(),
                scope,
                &entry.model.provider,
            ) {
                return Err(invalid(
                    "the selected provider is disabled by the host or task workspace policy",
                ));
            }
        }
        if !crate::models::model_entry_is_ready(&entry) {
            return Err(invalid(
                "the selected model has no configured credentials; configure its provider before delegating",
            ));
        }
        Ok(Selection {
            provider: entry.model.provider,
            model: entry.model.id,
            thinking,
        })
    }
}

fn host_config(tool: &SubagentTool) -> Result<Config> {
    match &tool.model_config {
        Some(config) => Ok(config.clone()),
        None => Config::load_with_roots_and_project_trust(
            Config::config_path_override_from_env(&tool.cwd).as_deref(),
            &tool.global_dir,
            &tool.cwd,
            false,
        )
        .map_err(|_| invalid("could not load the host's model policy")),
    }
}

fn fallback_policy(
    tool: &SubagentTool,
    config: &Config,
    agent: Option<&AgentDefinition>,
    explicit_role: Option<ModelRole>,
    host: &Path,
    cwd: &Path,
) -> Result<String> {
    let role = explicit_role.unwrap_or_else(|| {
        if agent.is_some_and(|agent| agent.model.is_some()) {
            ModelRole::Default
        } else {
            tool.role_model_role.unwrap_or(ModelRole::Default)
        }
    });
    crate::failover::ChildFallbackPolicy::capture(config, role, &[host, cwd])
        .to_cli_arg()
        .map_err(|message| Error::tool("subagent", format!("PI_SUBAGENT_FALLBACK: {message}")))
}

fn unknown() -> Error {
    invalid(
        "unknown model; use a native or configured models.json catalog entry (extension-only models are not available for explicit delegation)",
    )
}

fn exact(registry: &ModelRegistry, spec: &str) -> Result<Option<ModelEntry>> {
    if let Some((provider, model)) = spec.split_once('/')
        && let Some(entry) = registry.find(provider, model)
    {
        return Ok(Some(entry));
    }
    let mut matches = registry
        .models()
        .iter()
        .filter(|entry| entry.model.id.eq_ignore_ascii_case(spec));
    let first = matches.next();
    if matches.next().is_some() {
        return Err(invalid(
            "ambiguous model ID; qualify the selector with its provider",
        ));
    }
    Ok(first.cloned())
}

fn directory(host: &Path, cwd: Option<&Path>) -> Result<std::path::PathBuf> {
    let path = cwd.map_or_else(|| host.to_path_buf(), |path| host.join(path));
    let path = std::fs::canonicalize(path)
        .map_err(|_| invalid("model admission requires an existing task cwd"))?;
    if !path.is_dir() {
        return Err(invalid("model admission requires a directory as task cwd"));
    }
    Ok(path)
}

/// Conditional workflow branches may name a workspace that will never be
/// used. Freeze path policy without requiring or preparing that directory.
fn policy_directory(host: &Path, cwd: Option<&Path>) -> std::path::PathBuf {
    let path = cwd.map_or_else(|| host.to_path_buf(), |path| host.join(path));
    std::fs::canonicalize(&path).unwrap_or_else(|_| {
        let mut normalized = std::path::PathBuf::new();
        for component in path.components() {
            if component == std::path::Component::ParentDir {
                normalized.pop();
            } else {
                normalized.push(component);
            }
        }
        normalized
    })
}

/// Saved-session consultation deliberately uses the saved or explicit model,
/// never the host's task/smol role. Its recovery still freezes host exclusions.
pub(super) fn prepare_saved_session(tool: &SubagentTool, deadline: Deadline) -> Result<String> {
    let owner = AgentCx::for_current_or_request();
    check(&owner, deadline)?;
    let host = directory(&tool.cwd, None)?;
    let config = host_config(tool)?;
    let policy = fallback_policy(tool, &config, None, Some(ModelRole::Default), &host, &host)?;
    check(&owner, deadline)?;
    Ok(policy)
}

pub(super) fn prepare<'a>(
    tool: &SubagentTool,
    agents: &BTreeMap<String, AgentDefinition>,
    tasks: impl IntoIterator<Item = &'a mut SubagentTask>,
    deadline: Deadline,
) -> Result<()> {
    let pending: Vec<_> = tasks.into_iter().collect();
    if pending.is_empty() {
        return Ok(());
    }
    for task in &pending {
        if let Some(selector) = &task.model {
            selector.validate()?;
        }
    }
    let owner = AgentCx::for_current_or_request();
    check(&owner, deadline)?;
    let host = directory(&tool.cwd, None)?;
    let config = host_config(tool)?;
    let admission = pending
        .iter()
        .any(|task| task.model.is_some())
        .then(|| Admission::load(tool, config.clone(), deadline))
        .transpose()?;
    for task in pending {
        check(&owner, deadline)?;
        let cwd = if task.model.is_some() {
            directory(&tool.cwd, task.cwd.as_deref())?
        } else {
            policy_directory(&host, task.cwd.as_deref())
        };
        task.fallback_policy = Some(fallback_policy(
            tool,
            &config,
            agents.get(&task.agent),
            task.model.as_ref().map(Selector::fallback_role),
            &host,
            &cwd,
        )?);
        if let Some(selector) = &task.model
            && let Some(admission) = &admission
        {
            task.model_selection = Some(admission.resolve_selector(selector, &host, &cwd)?);
        }
    }
    check(&owner, deadline)
}

/// Persistent children share one frozen destination for their whole lifetime.
/// Their send/push operations intentionally do not accept this field.
pub(super) fn prepare_worker(
    tool: &SubagentTool,
    input: &Value,
    agent: &AgentDefinition,
    cwd: &Path,
    deadline: Deadline,
) -> Result<(Option<Selection>, String)> {
    let selector = field(input)?;
    let owner = AgentCx::for_current_or_request();
    check(&owner, deadline)?;
    let host = directory(&tool.cwd, None)?;
    let config = host_config(tool)?;
    let policy = fallback_policy(
        tool,
        &config,
        Some(agent),
        selector.as_ref().map(Selector::fallback_role),
        &host,
        cwd,
    )?;
    let selected = selector
        .map(|selector| {
            Admission::load(tool, config, deadline)?.resolve_selector(&selector, &host, cwd)
        })
        .transpose()?;
    check(&owner, deadline)?;
    Ok((selected, policy))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn worker_admission_freezes_role_policy_and_the_shared_rpc_launch_argument() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            retry: Some(crate::config::RetrySettings {
                fallback_chains: Some(std::collections::HashMap::from([
                    ("task".to_string(), vec![]),
                    ("default".to_string(), vec!["openai/alternate:low".to_string()]),
                ])),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut tool = SubagentTool::with_paths(
            dir.path().to_path_buf(), dir.path().join("global"), dir.path().join("unused-child"),
        ).with_model_config(Some(config)).with_role_model_selection(Some((
            crate::models::ModelRole::Task, "openai/primary".to_string(),
        )));
        let mut agent = super::super::tan_agent_definition();
        let deadline = Deadline::for_request(None, None).unwrap();
        let (selection, frozen) = prepare_worker(&tool, &json!({}), &agent, dir.path(), deadline).unwrap();
        assert!(selection.is_none());
        tool.model_config.as_mut().unwrap().retry.as_mut().unwrap().fallback_chains.as_mut().unwrap()
            .insert("task".to_string(), vec!["openai/later".to_string()]);
        let policy: crate::failover::ChildFallbackPolicy = frozen.parse().unwrap();
        assert!(policy.chain("openai", "primary", &[]).unwrap().entries.is_empty());
        let args = super::super::with_fallback_policy_args(
            super::super::worker_args(&agent, tool.role_model_spec.as_deref(), None, selection.as_ref()),
            Some(&frozen),
        );
        assert_eq!(args[0], "--subagent-fallback-policy");
        assert_eq!(args[1], frozen.as_str());
        assert!(args.windows(2).any(|pair| pair[0] == "--mode" && pair[1] == "rpc"));
        // An explicit definition pin to the same ID does not inherit task.
        agent.model = Some("openai/primary".to_string());
        let (_, pinned) = prepare_worker(&tool, &json!({}), &agent, dir.path(), deadline).unwrap();
        let pinned: serde_json::Value = serde_json::from_str(&pinned).unwrap();
        assert_eq!(pinned["role"], "default");
    }

    fn admission(config: Config) -> Admission {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("models.json");
        std::fs::write(&path, json!({"providers": {
            "selection-local": {
                "baseUrl": "http://127.0.0.1:1/v1", "api": "openai-completions", "authHeader": false,
                "models": [
                    {"id": "selection-fast", "reasoning": false},
                    {"id": "selection-reasoner", "reasoning": true},
                    {"id": "vendor/model:1", "reasoning": false},
                    {"id": "smol", "reasoning": false},
                    {"id": "selection-shared", "reasoning": false}
                ]
            },
            "selection-other": {
                "baseUrl": "http://127.0.0.1:1/v1", "api": "openai-completions", "authHeader": false,
                "models": [{"id": "selection-shared", "reasoning": false}]
            },
            "selection-keyed": {
                "baseUrl": "http://127.0.0.1:1/v1", "api": "openai-completions", "authHeader": true,
                "models": [{"id": "selection-private", "reasoning": false}]
            }
        }}).to_string()).unwrap();
        let registry =
            ModelRegistry::load_for_listing_with_credential_resolver(Some(path), |_| None);
        assert!(registry.error().is_none(), "{:?}", registry.error());
        Admission { config, registry }
    }

    #[test]
    fn exact_selection_preserves_provider_literal_ids_and_supported_effort() {
        let admission = admission(Config::default());
        let cwd = Path::new("/workspace");
        let selected = admission
            .resolve("selection-local/selection-reasoner:high", cwd, cwd)
            .unwrap();
        assert_eq!(
            selected,
            Selection {
                provider: "selection-local".to_string(),
                model: "selection-reasoner".to_string(),
                thinking: Some(ThinkingLevel::High),
            }
        );
        assert_eq!(
            admission.resolve("selection-fast", cwd, cwd).unwrap().model,
            "selection-fast"
        );
        let literal = admission
            .resolve("selection-local/vendor/model:1", cwd, cwd)
            .unwrap();
        assert_eq!(literal.model, "vendor/model:1");
        assert_eq!(literal.thinking, None);
        assert_eq!(
            admission
                .resolve("selection-other/selection-shared", cwd, cwd)
                .unwrap()
                .provider,
            "selection-other"
        );
    }

    #[test]
    fn invalid_unready_or_ambiguous_selection_never_chooses_a_fallback() {
        let admission = admission(Config::default());
        let cwd = Path::new("/workspace");
        for spec in [
            "",
            " ",
            "default",
            "inherit",
            "@task",
            "selection-*",
            "selection-fast,selection-reasoner",
            "selection-shared",
            "unknown/selection-fast",
            "selection-local/selection-fast:high",
            "selection-local/selection-reasoner:max",
            "selection-keyed/selection-private",
            "private-credential-sentinel",
            "selection-local/selection-reasoner:private-credential-sentinel",
        ] {
            let error = admission.resolve(spec, cwd, cwd).unwrap_err().to_string();
            assert!(error.contains("PI_SUBAGENT_MODEL"), "{error}");
            assert!(!error.contains("private-credential-sentinel"), "{error}");
        }
    }

    #[test]
    fn configured_roles_keep_literal_ids_effort_and_explicit_fallback_identity() {
        let admission = admission(Config {
            model_roles: Some(crate::config::ModelRoleSettings {
                smol: Some(" selection-local/selection-reasoner:high ".to_string()),
                task: Some("selection-local/vendor/model:1".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        });
        let cwd = Path::new("/workspace");
        let role = Selector::parse(&json!({"role":"smol"})).unwrap();
        assert_eq!(
            admission.resolve_selector(&role, cwd, cwd).unwrap(),
            Selection {
                provider: "selection-local".to_string(),
                model: "selection-reasoner".to_string(),
                thinking: Some(ThinkingLevel::High),
            }
        );
        assert_eq!(role.fallback_role(), ModelRole::Smol);
        let literal = Selector::parse(&json!("smol")).unwrap();
        assert_eq!(
            admission
                .resolve_selector(&literal, cwd, cwd)
                .unwrap()
                .model,
            "smol",
            "a bare model ID must never become a role alias"
        );
        assert_eq!(literal.fallback_role(), ModelRole::Default);
        let literal_suffix = admission
            .resolve_selector(&Selector::Role(ModelRole::Task), cwd, cwd)
            .unwrap();
        assert_eq!(literal_suffix.model, "vendor/model:1");
        assert_eq!(literal_suffix.thinking, None);
    }

    #[test]
    fn missing_or_invalid_role_destinations_never_select_another_model() {
        let cwd = Path::new("/workspace");
        for spec in [
            None,
            Some(""),
            Some("selection-shared"),
            Some("unknown/selection-fast"),
            Some("selection-local/selection-fast:high"),
            Some("selection-local/selection-reasoner:max"),
            Some("selection-keyed/selection-private"),
            Some("private-credential-sentinel"),
        ] {
            let admission = admission(Config {
                model_roles: Some(crate::config::ModelRoleSettings {
                    default: Some("selection-local/selection-fast".to_string()),
                    smol: spec.map(str::to_string),
                    ..Default::default()
                }),
                ..Default::default()
            });
            let error = admission
                .resolve_selector(&Selector::Role(ModelRole::Smol), cwd, cwd)
                .unwrap_err()
                .to_string();
            assert!(error.contains("PI_SUBAGENT_MODEL"), "{error}");
            assert!(!error.contains("private-credential-sentinel"), "{error}");
        }
        let unconfigured = admission(Config::default());
        for role in ModelRole::ALL {
            assert!(
                unconfigured
                    .resolve_selector(&Selector::Role(role), cwd, cwd)
                    .is_err(),
                "{role} must have its own assignment, including default"
            );
        }
    }

    #[test]
    fn host_and_task_provider_restrictions_both_apply() {
        let host = Path::new("/host/project");
        let task = Path::new("/task/project");
        let roles = crate::config::ModelRoleSettings {
            smol: Some("selection-local/selection-fast".to_string()),
            ..Default::default()
        };
        for path in ["/host", "/task"] {
            let admission = admission(Config {
                model_roles: Some(roles.clone()),
                model_scope_overrides: Some(vec![crate::config::ModelScopeOverride {
                    path: path.to_string(),
                    disabled_providers: Some(vec!["selection-local".to_string()]),
                    ..Default::default()
                }]),
                ..Config::default()
            });
            assert!(admission.resolve("selection-fast", host, task).is_err());
            assert!(
                admission
                    .resolve_selector(&Selector::Role(ModelRole::Smol), host, task)
                    .is_err()
            );
        }
        let admission = admission(Config {
            model_roles: Some(roles),
            disabled_providers: Some(vec!["selection-local".to_string()]),
            ..Config::default()
        });
        assert!(admission.resolve("selection-fast", host, task).is_err());
        assert!(
            admission
                .resolve_selector(&Selector::Role(ModelRole::Smol), host, task)
                .is_err()
        );
    }

    #[test]
    fn explicit_null_and_batch_wide_selectors_cannot_disappear_during_parsing() {
        for input in [
            json!({"model": null}),
            json!({"model": ["selection-fast"]}),
            json!({"model": {"role":"Smol"}}),
            json!({"model": {"role":" smol"}}),
            json!({"model": {"role":"@smol"}}),
            json!({"model": {"role":"reviewer"}}),
            json!({"model": {"role":null}}),
            json!({"model": {"role":"smol", "model":"selection-fast"}}),
            json!({"tasks": [{"agent":"task", "task":"work", "model":null}]}),
            json!({"chain": [{"agent":"task", "task":"work", "model":42}]}),
            json!({"model":"selection-fast", "tasks":[{"agent":"task", "task":"work"}]}),
            json!({"model":{"role":"smol"}, "chain":[{"agent":"task", "task":"work"}]}),
        ] {
            assert!(validate_request(&input).is_err(), "{input}");
        }
        assert!(
            validate_request(
                &json!({"tasks":[{"agent":"task", "task":"work", "model":"selection-fast"}]})
            )
            .is_ok()
        );
    }

    #[test]
    fn role_schema_and_request_deserialization_agree_on_explicit_shape() {
        let validator = jsonschema::validator_for(&schema()).unwrap();
        for role in ModelRole::ALL {
            let model = json!({"role":role.as_str()});
            assert!(validator.is_valid(&model));
            let request = json!({"agent":"task", "task":"work", "model":model});
            validate_request(&request).unwrap();
            let request: super::super::SubagentRequest = serde_json::from_value(request).unwrap();
            assert_eq!(request.model.unwrap().fallback_role(), role);
        }
        for model in [
            json!({}),
            json!({"role":"Smol"}),
            json!({"role":"smol", "thinking":"high"}),
            json!({"role":["smol"]}),
        ] {
            assert!(!validator.is_valid(&model));
            assert!(serde_json::from_value::<Selector>(model).is_err());
        }
    }
}
