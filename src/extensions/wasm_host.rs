//! Wasmtime component host implementation for feature-gated extensions.

// The feature-gated component host implements the complete extension protocol
// and its in-module conformance tests. It therefore consumes the private policy,
// connector, manager, and protocol surface as one adapter boundary.
use super::*;

use crate::connectors::http::{HttpConnector, HttpConnectorConfig};
use std::collections::BTreeSet;
use wasmtime::component::{Component, Linker};

wasmtime::component::bindgen!({
    path: "docs/wit/extension.wit",
    world: "pi-extension",
    imports: { default: async },
    exports: { default: async },
});

use self::pi::extension::host;

pub(super) struct HostState {
    policy: ExtensionPolicy,
    cwd: PathBuf,
    tools: Arc<crate::tools::ToolRegistry>,
    manager: Option<ExtensionManagerHandle>,
    http: HttpConnector,
    fs: FsConnector,
    env_allowlist: BTreeSet<String>,
    manifest_schema: Option<String>,
    manifest_requirements: Vec<CapabilityRequirement>,
    extension_id: Option<String>,
}

impl HostState {
    pub(super) fn new(policy: ExtensionPolicy, cwd: PathBuf) -> Result<Self> {
        let tools = Arc::new(crate::tools::ToolRegistry::new(
            &["read", "bash", "edit", "write", "grep", "find", "ls"],
            &cwd,
            None,
        ));
        Self::new_with_tools(policy, cwd, tools, None)
    }

    pub(super) fn new_with_tools(
        policy: ExtensionPolicy,
        cwd: PathBuf,
        tools: Arc<crate::tools::ToolRegistry>,
        manager: Option<ExtensionManagerHandle>,
    ) -> Result<Self> {
        let scopes = FsScopes::least_privilege_for_cwd(&cwd)?;
        let fs = FsConnector::new(&cwd, policy.clone(), scopes)?;
        Ok(Self {
            policy,
            cwd,
            tools,
            manager,
            http: HttpConnector::new(HttpConnectorConfig {
                enforce_allowlist: true,
                ..Default::default()
            }),
            fs,
            env_allowlist: BTreeSet::new(),
            manifest_schema: None,
            manifest_requirements: Vec::new(),
            extension_id: None,
        })
    }

    fn env_allowlist_from_manifest(manifest: Option<&CapabilityManifest>) -> BTreeSet<String> {
        let Some(manifest) = manifest else {
            return BTreeSet::new();
        };

        let mut out = BTreeSet::new();
        for req in &manifest.capabilities {
            if !req.capability.trim().eq_ignore_ascii_case("env") {
                continue;
            }
            let Some(scope) = req.scope.as_ref() else {
                continue;
            };
            let Some(env) = scope.env.as_ref() else {
                continue;
            };
            for key in env {
                let key = key.trim();
                if !key.is_empty() {
                    out.insert(key.to_string());
                }
            }
        }
        out
    }

    fn http_allowlist_from_manifest(manifest: Option<&CapabilityManifest>) -> Vec<String> {
        let Some(manifest) = manifest else {
            return Vec::new();
        };

        let mut out = Vec::new();
        for req in &manifest.capabilities {
            if !req.capability.trim().eq_ignore_ascii_case("http") {
                continue;
            }
            let Some(scope) = req.scope.as_ref() else {
                continue;
            };
            let Some(hosts) = scope.hosts.as_ref() else {
                continue;
            };
            for host in hosts {
                let host = host.trim();
                if !host.is_empty() {
                    out.push(host.to_string());
                }
            }
        }
        out
    }

    pub fn apply_registration(&mut self, registration: &RegisterPayload) -> Result<()> {
        if !registration.name.trim().is_empty() {
            self.extension_id = Some(registration.name.trim().to_string());
        }

        let manifest = registration.capability_manifest.as_ref();
        self.manifest_schema = manifest.map(|value| value.schema.clone());
        self.manifest_requirements =
            manifest.map_or_else(Vec::new, |value| value.capabilities.clone());

        self.env_allowlist = Self::env_allowlist_from_manifest(manifest);

        let fs_scopes = FsScopes::from_manifest(manifest, &self.cwd)?;
        self.fs = FsConnector::new(&self.cwd, self.policy.clone(), fs_scopes)?;

        let http_allowlist = Self::http_allowlist_from_manifest(manifest);
        self.http = HttpConnector::new(HttpConnectorConfig {
            allowlist: http_allowlist,
            enforce_allowlist: true,
            ..Default::default()
        });

        Ok(())
    }

    fn manager(&self) -> Option<ExtensionManager> {
        self.manager
            .as_ref()
            .and_then(ExtensionManagerHandle::upgrade)
    }

    fn hostcall_op(params: &Value) -> Option<String> {
        params
            .get("op")
            .or_else(|| params.get("method"))
            .or_else(|| params.get("name"))
            .and_then(Value::as_str)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    }

    fn matches_manifest_class(classes: &[String], class_name: &str) -> bool {
        classes
            .iter()
            .any(|value| value.trim().eq_ignore_ascii_case(class_name))
    }

    fn connector_class_for_call(call: &HostCallPayload) -> Option<&'static str> {
        let method = call.method.trim();
        if method.eq_ignore_ascii_case("tool") {
            Some("tool")
        } else if method.eq_ignore_ascii_case("fs") {
            Some("fs")
        } else if method.eq_ignore_ascii_case("exec") {
            Some("exec")
        } else if method.eq_ignore_ascii_case("env") {
            Some("env")
        } else if method.eq_ignore_ascii_case("http") {
            Some("http")
        } else if method.eq_ignore_ascii_case("session") {
            Some("session")
        } else if method.eq_ignore_ascii_case("events") {
            Some("events")
        } else if method.eq_ignore_ascii_case("ui") {
            Some("ui")
        } else if method.eq_ignore_ascii_case("log") {
            Some("log")
        } else {
            None
        }
    }

    fn hostcall_class_for_call(call: &HostCallPayload) -> Option<String> {
        let method = call.method.trim();
        if method.eq_ignore_ascii_case("fs") {
            let op = call
                .params
                .get("op")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or_default();
            let op = FsOp::parse(op)?;
            let suffix = match op {
                FsOp::Read => "read",
                FsOp::Write => "write",
                FsOp::List => "list",
                FsOp::Stat => "stat",
                FsOp::Mkdir => "mkdir",
                FsOp::Delete => "delete",
            };
            return Some(format!("fs.{suffix}"));
        }

        if method.is_empty() {
            None
        } else {
            Some(method.to_ascii_lowercase())
        }
    }

    fn enforce_manifest_classes(
        &self,
        required: &str,
        call: &HostCallPayload,
    ) -> std::result::Result<(), String> {
        if self.manifest_schema.as_deref() != Some(CAPABILITY_MANIFEST_SCHEMA_V2) {
            return Ok(());
        }

        let Some(connector_class) = Self::connector_class_for_call(call) else {
            return Ok(());
        };
        let Some(hostcall_class) = Self::hostcall_class_for_call(call) else {
            // Let downstream call parsing return the canonical invalid-request error.
            return Ok(());
        };

        let matching_requirements = self
            .manifest_requirements
            .iter()
            .filter(|req| req.capability.trim().eq_ignore_ascii_case(required))
            .collect::<Vec<_>>();

        if matching_requirements.is_empty() {
            return Err(Self::host_error_json(
                HostCallErrorCode::Denied,
                format!("Capability '{required}' not declared in capability manifest"),
                Some(json!({
                    "capability": required,
                    "connector_class": connector_class,
                    "hostcall_class": hostcall_class,
                    "hint": "Declare this capability in capability_manifest.v2 before use."
                })),
                None,
            ));
        }

        let allowed = matching_requirements.iter().any(|req| {
            Self::matches_manifest_class(&req.connector_classes, connector_class)
                && Self::matches_manifest_class(&req.hostcall_classes, &hostcall_class)
        });
        if allowed {
            return Ok(());
        }

        let allowed_connector_classes = matching_requirements
            .iter()
            .flat_map(|req| req.connector_classes.iter())
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let allowed_hostcall_classes = matching_requirements
            .iter()
            .flat_map(|req| req.hostcall_classes.iter())
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();

        Err(Self::host_error_json(
            HostCallErrorCode::Denied,
            format!(
                "Capability scope mismatch: connector class '{connector_class}' / hostcall class '{hostcall_class}' is not allowed for capability '{required}'"
            ),
            Some(json!({
                "capability": required,
                "connector_class": connector_class,
                "hostcall_class": hostcall_class,
                "allowed_connector_classes": allowed_connector_classes,
                "allowed_hostcall_classes": allowed_hostcall_classes,
                "hint": "Update capability_manifest scope: connector_classes and hostcall_classes must include this call."
            })),
            None,
        ))
    }

    fn host_error_json(
        code: HostCallErrorCode,
        message: impl Into<String>,
        details: Option<Value>,
        retryable: Option<bool>,
    ) -> String {
        let payload = HostCallError {
            code,
            message: message.into(),
            details,
            retryable,
        };
        serde_json::to_string(&payload).unwrap_or_else(|_| {
            format!(
                "{{\"code\":\"internal\",\"message\":\"failed to serialize error: {}\"}}",
                payload.message
            )
        })
    }

    fn hostcall_outcome_code(code: &str) -> HostCallErrorCode {
        match code {
            "timeout" => HostCallErrorCode::Timeout,
            "denied" => HostCallErrorCode::Denied,
            "io" => HostCallErrorCode::Io,
            "invalid_request" => HostCallErrorCode::InvalidRequest,
            _ => HostCallErrorCode::Internal,
        }
    }

    fn hostcall_outcome_to_result(outcome: HostcallOutcome) -> std::result::Result<String, String> {
        let value = match outcome {
            HostcallOutcome::Success(value) => value,
            HostcallOutcome::StreamChunk {
                sequence,
                chunk,
                is_final,
            } => serde_json::json!({
                "sequence": sequence,
                "chunk": chunk,
                "isFinal": is_final,
            }),
            HostcallOutcome::Error { code, message } => {
                return Err(Self::host_error_json(
                    Self::hostcall_outcome_code(&code),
                    message,
                    None,
                    None,
                ));
            }
        };

        serde_json::to_string(&value).map_err(|err| {
            Self::host_error_json(
                HostCallErrorCode::Internal,
                format!("Failed to serialize hostcall output: {err}"),
                None,
                None,
            )
        })
    }

    async fn resolve_policy_decision(&self, required: &str) -> (PolicyDecision, String, String) {
        const UNKNOWN_EXTENSION_ID: &str = "<unknown>";
        let PolicyCheck {
            decision,
            capability,
            reason,
        } = self.policy.evaluate(required);

        if decision != PolicyDecision::Prompt {
            return (decision, reason, capability);
        }

        let Some(manager) = self.manager() else {
            return (
                PolicyDecision::Deny,
                "prompt_required_no_manager".to_string(),
                capability,
            );
        };

        if let Some(extension_id) = self.extension_id.as_deref()
            && let Some(allow) = manager.cached_policy_prompt_decision(extension_id, &capability)
        {
            let decision = if allow {
                PolicyDecision::Allow
            } else {
                PolicyDecision::Deny
            };
            let reason = if allow {
                "prompt_cache_allow".to_string()
            } else {
                "prompt_cache_deny".to_string()
            };
            return (decision, reason, capability);
        }

        let prompt_extension_id = self.extension_id.as_deref().unwrap_or(UNKNOWN_EXTENSION_ID);
        let allow = prompt_capability_once(&manager, prompt_extension_id, &capability).await;
        if let Some(extension_id) = self.extension_id.as_deref() {
            manager.cache_policy_prompt_decision(extension_id, &capability, allow);
        }
        let decision = if allow {
            PolicyDecision::Allow
        } else {
            PolicyDecision::Deny
        };
        let reason = if allow {
            "prompt_user_allow".to_string()
        } else {
            "prompt_user_deny".to_string()
        };
        (decision, reason, capability)
    }

    async fn dispatch_tool(&self, call: &HostCallPayload) -> std::result::Result<String, String> {
        let params = &call.params;
        let call_timeout_ms = call.timeout_ms.filter(|ms| *ms > 0);
        let tool_name = params
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .ok_or_else(|| {
                Self::host_error_json(
                    HostCallErrorCode::InvalidRequest,
                    "Missing tool name",
                    None,
                    None,
                )
            })?;
        let mut input = params
            .get("input")
            .cloned()
            .unwrap_or_else(|| Value::Object(serde_json::Map::default()));

        if tool_name.eq_ignore_ascii_case("bash")
            && input.get("timeout").is_none()
            && let Some(timeout_ms) = call_timeout_ms
        {
            let timeout_secs = timeout_ms.div_ceil(1000);
            if let Some(obj) = input.as_object_mut() {
                obj.insert("timeout".to_string(), json!(timeout_secs));
            }
        }

        let tool = self.tools.get(tool_name).ok_or_else(|| {
            Self::host_error_json(
                HostCallErrorCode::InvalidRequest,
                format!("Unknown tool: {tool_name}"),
                Some(json!({ "tool": tool_name })),
                None,
            )
        })?;

        let execute = tool.execute(&call.call_id, input, None);
        let output = if let Some(timeout_ms) = call_timeout_ms {
            match timeout(
                wall_now(),
                Duration::from_millis(timeout_ms),
                Box::pin(execute),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => {
                    return Err(Self::host_error_json(
                        HostCallErrorCode::Timeout,
                        format!("Tool execution timed out after {timeout_ms}ms"),
                        Some(json!({ "tool": tool_name, "timeout_ms": timeout_ms })),
                        Some(true),
                    ));
                }
            }
        } else {
            execute.await
        }
        .map_err(|err| match &err {
            Error::Validation(_) => Self::host_error_json(
                HostCallErrorCode::InvalidRequest,
                err.to_string(),
                Some(json!({ "tool": tool_name })),
                None,
            ),
            Error::Tool { .. } | Error::Io(_) => Self::host_error_json(
                HostCallErrorCode::Io,
                err.to_string(),
                Some(json!({ "tool": tool_name })),
                None,
            ),
            Error::Aborted => Self::host_error_json(
                HostCallErrorCode::Timeout,
                "Tool execution aborted",
                Some(json!({ "tool": tool_name })),
                Some(true),
            ),
            _ => Self::host_error_json(
                HostCallErrorCode::Internal,
                err.to_string(),
                Some(json!({ "tool": tool_name })),
                None,
            ),
        })?;

        serde_json::to_string(&output).map_err(|err| {
            Self::host_error_json(
                HostCallErrorCode::Internal,
                format!("Failed to serialize tool output: {err}"),
                Some(json!({ "tool": tool_name })),
                None,
            )
        })
    }

    async fn dispatch_http(&self, call: &HostCallPayload) -> std::result::Result<String, String> {
        let connector_call = crate::connectors::HostCallPayload {
            call_id: call.call_id.clone(),
            capability: call.capability.clone(),
            method: call.method.clone(),
            params: call.params.clone(),
            timeout_ms: call.timeout_ms,
            cancel_token: call.cancel_token.clone(),
            context: call.context.clone(),
        };

        let result = self.http.dispatch(&connector_call).await.map_err(|err| {
            Self::host_error_json(HostCallErrorCode::Internal, err.to_string(), None, None)
        })?;

        if result.is_error {
            let error = result.error.as_ref().map_or_else(
                || {
                    Self::host_error_json(
                        HostCallErrorCode::Internal,
                        "HTTP connector returned is_error=true but no error payload",
                        None,
                        None,
                    )
                },
                |payload| {
                    Self::host_error_json(
                        payload.code,
                        payload.message.clone(),
                        payload.details.clone(),
                        payload.retryable,
                    )
                },
            );
            return Err(error);
        }

        serde_json::to_string(&result.output).map_err(|err| {
            Self::host_error_json(
                HostCallErrorCode::Internal,
                format!("Failed to serialize HTTP output: {err}"),
                None,
                None,
            )
        })
    }

    async fn dispatch_exec(&self, call: &HostCallPayload) -> std::result::Result<String, String> {
        // Minimal: map exec -> bash tool (same sandbox semantics).
        let mut params = call.params.clone();
        if params.get("command").is_none() {
            let cmd = params
                .get("cmd")
                .and_then(Value::as_str)
                .map(ToString::to_string);
            let args_str = params.get("args").and_then(Value::as_array).map(|args| {
                args.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            });
            if let (Some(cmd), Some(args_str)) = (cmd, args_str) {
                let command = format!("{cmd} {args_str}");
                if let Some(obj) = params.as_object_mut() {
                    obj.insert("command".to_string(), Value::String(command));
                    obj.remove("cmd");
                    obj.remove("args");
                } else {
                    params = json!({ "command": command });
                }
            }
        }

        let bash_call = HostCallPayload {
            call_id: call.call_id.clone(),
            capability: call.capability.clone(),
            method: "tool".to_string(),
            params: json!({ "name": "bash", "input": params }),
            timeout_ms: call.timeout_ms,
            cancel_token: call.cancel_token.clone(),
            context: call.context.clone(),
        };

        self.dispatch_tool(&bash_call).await
    }

    async fn dispatch_fs(&self, call: &HostCallPayload) -> std::result::Result<String, String> {
        let result = self.fs.handle_host_call(call, self.extension_id.as_deref());

        if result.is_error {
            let error = result.error.as_ref().map_or_else(
                || {
                    Self::host_error_json(
                        HostCallErrorCode::Internal,
                        "FS connector returned is_error=true but no error payload",
                        None,
                        None,
                    )
                },
                |payload| {
                    Self::host_error_json(
                        payload.code,
                        payload.message.clone(),
                        payload.details.clone(),
                        payload.retryable,
                    )
                },
            );
            return Err(error);
        }

        serde_json::to_string(&result.output).map_err(|err| {
            Self::host_error_json(
                HostCallErrorCode::Internal,
                format!("Failed to serialize fs output: {err}"),
                None,
                None,
            )
        })
    }

    fn sha256_hex(input: &str) -> String {
        let mut hasher = sha2::Sha256::new();
        hasher.update(input.as_bytes());
        let digest = hasher.finalize();
        format!("{digest:x}")
    }

    fn canonicalize_json(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let mut keys = map.keys().cloned().collect::<Vec<_>>();
                keys.sort();
                let mut out = serde_json::Map::new();
                for key in keys {
                    if let Some(value) = map.get(&key) {
                        out.insert(key, Self::canonicalize_json(value));
                    }
                }
                Value::Object(out)
            }
            Value::Array(items) => {
                Value::Array(items.iter().map(Self::canonicalize_json).collect())
            }
            other => other.clone(),
        }
    }

    fn hostcall_params_hash(method: &str, params: &Value) -> String {
        let canonical = Self::canonicalize_json(&json!({ "method": method, "params": params }));
        let encoded = serde_json::to_string(&canonical)
            .unwrap_or_else(|_| "{\"error\":\"canonical_hostcall_failed\"}".to_string());
        Self::sha256_hex(&encoded)
    }

    async fn dispatch_env(&self, call: &HostCallPayload) -> std::result::Result<String, String> {
        let params = &call.params;
        let mut names = Vec::new();

        if let Some(name) = params.get("name").and_then(Value::as_str) {
            let name = name.trim();
            if !name.is_empty() {
                names.push(name.to_string());
            }
        } else if let Some(items) = params.get("names").and_then(Value::as_array) {
            for item in items {
                if let Some(name) = item.as_str() {
                    let name = name.trim();
                    if !name.is_empty() {
                        names.push(name.to_string());
                    }
                }
            }
        }

        if names.is_empty() {
            return Err(Self::host_error_json(
                HostCallErrorCode::InvalidRequest,
                "Missing env var name(s)",
                None,
                None,
            ));
        }

        if self.env_allowlist.is_empty() {
            return Err(Self::host_error_json(
                HostCallErrorCode::Denied,
                "Env access not configured (no allowlist)",
                Some(json!({ "capability": "env" })),
                None,
            ));
        }

        let mut denied_hashes = Vec::new();
        for name in &names {
            if !self.env_allowlist.contains(name) {
                denied_hashes.push(Self::sha256_hex(name));
            }
        }

        if !denied_hashes.is_empty() {
            return Err(Self::host_error_json(
                HostCallErrorCode::Denied,
                "Env var not allowed by scope",
                Some(json!({ "denied_hashes": denied_hashes })),
                None,
            ));
        }

        let mut values = serde_json::Map::new();
        let broker = &self.policy;
        for name in names {
            match std::env::var_os(&name) {
                None => {
                    values.insert(name, Value::Null);
                }
                Some(value) => match value.into_string() {
                    Ok(value) => {
                        // SEC-4.3: Apply secret broker redaction.
                        let final_value = broker.secret_broker.maybe_redact(&name, &value);
                        if final_value != value {
                            tracing::info!(
                                event = "secret_broker.redact",
                                name_hash = %Self::sha256_hex(&name),
                                "Secret broker redacted env var value"
                            );
                        }
                        values.insert(name, Value::String(final_value.to_string()));
                    }
                    Err(_) => {
                        return Err(Self::host_error_json(
                            HostCallErrorCode::Io,
                            "Env var value is not valid UTF-8",
                            Some(json!({ "name_hash": Self::sha256_hex(&name) })),
                            None,
                        ));
                    }
                },
            }
        }

        let output = json!({ "values": Value::Object(values) });
        serde_json::to_string(&output).map_err(|err| {
            Self::host_error_json(
                HostCallErrorCode::Internal,
                format!("Failed to serialize env output: {err}"),
                None,
                None,
            )
        })
    }
}

impl host::Host for HostState {
    #[allow(clippy::too_many_lines)]
    async fn call(
        &mut self,
        name: String,
        input_json: String,
    ) -> std::result::Result<String, String> {
        let payload: HostCallPayload = match serde_json::from_str(&input_json) {
            Ok(value) => value,
            Err(err) => {
                return Err(Self::host_error_json(
                    HostCallErrorCode::InvalidRequest,
                    format!("Invalid host_call JSON: {err}"),
                    None,
                    None,
                ));
            }
        };

        if !name.trim().is_empty() && !payload.method.eq_ignore_ascii_case(name.trim()) {
            return Err(Self::host_error_json(
                HostCallErrorCode::InvalidRequest,
                "host.call name must match host_call.method",
                Some(json!({ "name": name, "method": payload.method })),
                None,
            ));
        }

        let Some(required) = required_capability_for_host_call_static(&payload) else {
            return Err(Self::host_error_json(
                HostCallErrorCode::InvalidRequest,
                format!("Unknown host_call method: {}", payload.method),
                Some(json!({ "method": payload.method })),
                None,
            ));
        };

        if !payload.capability.trim().eq_ignore_ascii_case(required) {
            return Err(Self::host_error_json(
                HostCallErrorCode::InvalidRequest,
                "Capability mismatch: declared capability does not match derived capability",
                Some(json!({
                    "declared": payload.capability,
                    "required": required,
                    "method": payload.method,
                })),
                None,
            ));
        }
        self.enforce_manifest_classes(required, &payload)?;

        let call_timeout_ms = payload.timeout_ms.filter(|ms| *ms > 0);
        let params_hash = Self::hostcall_params_hash(&payload.method, &payload.params);
        let started_at = Instant::now();

        tracing::info!(
            event = "host_call.start",
            runtime = "wasm",
            call_id = %payload.call_id,
            extension_id = ?self.extension_id.as_deref(),
            capability = %required,
            method = %payload.method,
            params_hash = %params_hash,
            timeout_ms = call_timeout_ms,
            "Hostcall start"
        );

        let (decision, reason, capability) = self.resolve_policy_decision(required).await;
        if decision == PolicyDecision::Allow {
            tracing::info!(
                event = "policy.decision",
                runtime = "wasm",
                call_id = %payload.call_id,
                extension_id = ?self.extension_id.as_deref(),
                capability = %capability,
                decision = ?decision,
                reason = %reason,
                params_hash = %params_hash,
                "Hostcall allowed by policy"
            );
        } else {
            tracing::warn!(
                event = "policy.decision",
                runtime = "wasm",
                call_id = %payload.call_id,
                extension_id = ?self.extension_id.as_deref(),
                capability = %capability,
                decision = ?decision,
                reason = %reason,
                params_hash = %params_hash,
                "Hostcall denied by policy"
            );
        }

        let method = payload.method.trim().to_ascii_lowercase();
        let outcome = if decision == PolicyDecision::Allow {
            let dispatch = async {
                match method.as_str() {
                    "tool" => self.dispatch_tool(&payload).await,
                    "http" => self.dispatch_http(&payload).await,
                    "exec" => self.dispatch_exec(&payload).await,
                    "fs" => self.dispatch_fs(&payload).await,
                    "env" => self.dispatch_env(&payload).await,
                    "session" | "ui" | "events" => {
                        let op = Self::hostcall_op(&payload.params).ok_or_else(|| {
                            Self::host_error_json(
                                HostCallErrorCode::InvalidRequest,
                                format!("Missing host_call op for {method}"),
                                Some(json!({ "method": method })),
                                None,
                            )
                        })?;
                        let manager = self.manager().ok_or_else(|| {
                            Self::host_error_json(
                                HostCallErrorCode::Denied,
                                "No extension manager configured for host_call",
                                Some(json!({ "method": method })),
                                None,
                            )
                        })?;
                        let outcome = match method.as_str() {
                            "session" => {
                                dispatch_hostcall_session(
                                    &payload.call_id,
                                    &manager,
                                    &op,
                                    payload.params.clone(),
                                )
                                .await
                            }
                            "ui" => {
                                dispatch_hostcall_ui(
                                    &payload.call_id,
                                    &manager,
                                    &op,
                                    payload.params.clone(),
                                    self.extension_id.as_deref(),
                                )
                                .await
                            }
                            "events" => {
                                dispatch_hostcall_events(
                                    &payload.call_id,
                                    &manager,
                                    self.tools.as_ref(),
                                    &op,
                                    payload.params.clone(),
                                )
                                .await
                            }
                            _ => HostcallOutcome::Error {
                                code: "invalid_request".to_string(),
                                message: format!("Unsupported host_call method: {method}"),
                            },
                        };
                        Self::hostcall_outcome_to_result(outcome)
                    }
                    _ => Err(Self::host_error_json(
                        HostCallErrorCode::InvalidRequest,
                        format!("Unsupported host_call method: {method}"),
                        Some(json!({ "method": method })),
                        None,
                    )),
                }
            };

            match call_timeout_ms {
                Some(timeout_ms) => timeout(
                    wall_now(),
                    Duration::from_millis(timeout_ms),
                    Box::pin(dispatch),
                )
                .await
                .unwrap_or_else(|_| {
                    Err(Self::host_error_json(
                        HostCallErrorCode::Timeout,
                        format!("Hostcall timed out after {timeout_ms}ms"),
                        Some(json!({ "capability": required, "method": method })),
                        Some(true),
                    ))
                }),
                None => dispatch.await,
            }
        } else {
            Err(Self::host_error_json(
                HostCallErrorCode::Denied,
                format!("Capability '{capability}' denied by policy ({reason})"),
                Some(json!({
                    "capability": capability,
                    "decision": format!("{:?}", decision),
                    "reason": reason,
                })),
                None,
            ))
        };

        let duration_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
        let (is_error, error_code) = match &outcome {
            Ok(_) => (false, None),
            Err(err_json) => (
                true,
                serde_json::from_str::<HostCallError>(err_json)
                    .ok()
                    .map(|err| err.code),
            ),
        };

        if is_error {
            tracing::warn!(
                event = "host_call.end",
                runtime = "wasm",
                call_id = %payload.call_id,
                extension_id = ?self.extension_id.as_deref(),
                capability = %required,
                method = %payload.method,
                params_hash = %params_hash,
                duration_ms,
                error_code = ?error_code,
                "Hostcall end (error)"
            );
        } else {
            tracing::info!(
                event = "host_call.end",
                runtime = "wasm",
                call_id = %payload.call_id,
                extension_id = ?self.extension_id.as_deref(),
                capability = %required,
                method = %payload.method,
                params_hash = %params_hash,
                duration_ms,
                "Hostcall end (success)"
            );
        }

        outcome
    }
}

pub struct Instance {
    store: wasmtime::Store<HostState>,
    bindings: PiExtension,
}

impl Instance {
    pub(super) async fn instantiate(
        engine: &wasmtime::Engine,
        path: &Path,
        state: HostState,
    ) -> Result<Self> {
        let component = Component::from_file(engine, path).map_err(|err| {
            Error::extension(format!(
                "Failed to load WASM component {}: {err:#}",
                path.display()
            ))
        })?;

        let mut linker = Linker::<HostState>::new(engine);
        host::add_to_linker::<HostState, wasmtime::component::HasSelf<HostState>>(
            &mut linker,
            |data| data,
        )
        .map_err(|err| Error::extension(format!("Failed to link WASM host imports: {err}")))?;

        let mut store = wasmtime::Store::new(engine, state);
        let bindings = PiExtension::instantiate_async(&mut store, &component, &linker)
            .await
            .map_err(|err| {
                Error::extension(format!("Failed to instantiate WASM extension: {err:#}"))
            })?;

        Ok(Self { store, bindings })
    }

    pub async fn init(&mut self, manifest_json: &str) -> Result<String> {
        let result = self
            .bindings
            .interface0
            .call_init(&mut self.store, manifest_json)
            .await
            .map_err(|err| Error::extension(format!("WASM init failed: {err}")))?;

        let registration_json = result.map_err(Error::extension)?;
        let registration: RegisterPayload =
            serde_json::from_str(&registration_json).map_err(|err| {
                Error::extension(format!(
                    "WASM init returned invalid registration payload: {err}"
                ))
            })?;
        validate_register(&registration)?;
        self.store.data_mut().apply_registration(&registration)?;

        Ok(registration_json)
    }

    pub async fn handle_tool(&mut self, name: &str, input_json: &str) -> Result<String> {
        let result = self
            .bindings
            .interface0
            .call_handle_tool(&mut self.store, name, input_json)
            .await
            .map_err(|err| Error::extension(format!("WASM handle-tool failed: {err}")))?;

        result.map_err(Error::extension)
    }

    pub async fn handle_slash(
        &mut self,
        command: &str,
        args: &[String],
        input_json: &str,
    ) -> Result<String> {
        let result = self
            .bindings
            .interface0
            .call_handle_slash(&mut self.store, command, args, input_json)
            .await
            .map_err(|err| Error::extension(format!("WASM handle-slash failed: {err}")))?;

        result.map_err(Error::extension)
    }

    pub async fn handle_event(&mut self, event_json: &str) -> Result<String> {
        let result = self
            .bindings
            .interface0
            .call_handle_event(&mut self.store, event_json)
            .await
            .map_err(|err| Error::extension(format!("WASM handle-event failed: {err}")))?;

        result.map_err(Error::extension)
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        self.bindings
            .interface0
            .call_shutdown(&mut self.store)
            .await
            .map_err(|err| Error::extension(format!("WASM shutdown failed: {err}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::WasmExtensionHost;
    use super::*;
    use crate::connectors::http::HttpConnectorConfig;
    use crate::model::{ContentBlock, TextContent};
    use crate::tools::{Tool, ToolOutput, ToolRegistry, ToolUpdate};
    use asupersync::runtime::RuntimeBuilder;
    use asupersync::time::{sleep, wall_now};
    use async_trait::async_trait;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::future::Future;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;
    use tempfile::tempdir;

    fn run_async<T, Fut>(future: Fut) -> T
    where
        Fut: Future<Output = T>,
    {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("build asupersync runtime");
        runtime.block_on(future)
    }

    fn permissive_policy() -> ExtensionPolicy {
        ExtensionPolicy {
            mode: ExtensionPolicyMode::Permissive,
            max_memory_mb: 256,
            default_caps: Vec::new(),
            deny_caps: Vec::new(),
            ..Default::default()
        }
    }

    fn strict_policy(default_caps: &[&str], deny_caps: &[&str]) -> ExtensionPolicy {
        ExtensionPolicy {
            mode: ExtensionPolicyMode::Strict,
            max_memory_mb: 256,
            default_caps: default_caps.iter().map(|cap| (*cap).to_string()).collect(),
            deny_caps: deny_caps.iter().map(|cap| (*cap).to_string()).collect(),
            ..Default::default()
        }
    }

    fn registration_payload() -> RegisterPayload {
        RegisterPayload {
            name: "ext.test".to_string(),
            version: "0.1.0".to_string(),
            api_version: PROTOCOL_VERSION.to_string(),
            capabilities: Vec::new(),
            capability_manifest: Some(CapabilityManifest {
                schema: "pi.ext.cap.v1".to_string(),
                capabilities: vec![
                    CapabilityRequirement {
                        capability: "env".to_string(),
                        methods: vec!["env".to_string()],
                        intents: Vec::new(),
                        connector_classes: Vec::new(),
                        hostcall_classes: Vec::new(),
                        risk_tier: None,
                        scope: Some(CapabilityScope {
                            env: Some(vec!["PI_TEST_ENV".to_string()]),
                            paths: None,
                            hosts: None,
                            allowed_tools: None,
                        }),
                        provenance: None,
                    },
                    CapabilityRequirement {
                        capability: "read".to_string(),
                        methods: vec!["fs".to_string()],
                        intents: Vec::new(),
                        connector_classes: Vec::new(),
                        hostcall_classes: Vec::new(),
                        risk_tier: None,
                        scope: Some(CapabilityScope {
                            paths: Some(vec![".".to_string()]),
                            hosts: None,
                            env: None,
                            allowed_tools: None,
                        }),
                        provenance: None,
                    },
                ],
            }),
            tools: Vec::new(),
            slash_commands: Vec::new(),
            shortcuts: Vec::new(),
            flags: Vec::new(),
            event_hooks: Vec::new(),
        }
    }

    fn registration_payload_with_write_scope() -> RegisterPayload {
        let mut payload = registration_payload();
        let CapabilityManifest { capabilities, .. } = payload
            .capability_manifest
            .get_or_insert_with(|| CapabilityManifest {
                schema: "pi.ext.cap.v1".to_string(),
                capabilities: Vec::new(),
            });
        capabilities.push(CapabilityRequirement {
            capability: "write".to_string(),
            methods: vec!["fs".to_string()],
            intents: Vec::new(),
            connector_classes: Vec::new(),
            hostcall_classes: Vec::new(),
            risk_tier: None,
            scope: Some(CapabilityScope {
                paths: Some(vec![".".to_string()]),
                hosts: None,
                env: None,
                allowed_tools: None,
            }),
            provenance: None,
        });
        payload
    }

    fn registration_payload_v2_read_fs_scope() -> RegisterPayload {
        RegisterPayload {
            name: "ext.test".to_string(),
            version: "0.2.0".to_string(),
            api_version: PROTOCOL_VERSION.to_string(),
            capabilities: Vec::new(),
            capability_manifest: Some(CapabilityManifest {
                schema: CAPABILITY_MANIFEST_SCHEMA_V2.to_string(),
                capabilities: vec![CapabilityRequirement {
                    capability: "read".to_string(),
                    methods: Vec::new(),
                    intents: vec!["file_read".to_string()],
                    connector_classes: vec!["fs".to_string()],
                    hostcall_classes: vec!["fs.read".to_string()],
                    risk_tier: Some("low".to_string()),
                    scope: Some(CapabilityScope {
                        paths: Some(vec![".".to_string()]),
                        hosts: None,
                        env: None,
                        allowed_tools: None,
                    }),
                    provenance: Some(CapabilityProvenance {
                        source: "local".to_string(),
                        integrity: CapabilityIntegrityAttestation {
                            algorithm: "sha256".to_string(),
                            digest:
                                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                                    .to_string(),
                        },
                        publisher: CapabilityPublisherAttestation {
                            id: "publisher.local.test".to_string(),
                            verification: "unsigned".to_string(),
                        },
                    }),
                }],
            }),
            tools: Vec::new(),
            slash_commands: Vec::new(),
            shortcuts: Vec::new(),
            flags: Vec::new(),
            event_hooks: Vec::new(),
        }
    }

    fn wat_data(text: &str) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut encoded = String::with_capacity(text.len() * 3);
        for byte in text.bytes() {
            encoded.push('\\');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        encoded
    }

    const REAL_COMPONENT_WAT_TEMPLATE: &str = r#"
(component
  (type $host-interface
    (instance
      (type $host-result (result string (error string)))
      (type $host-call
        (func
          (param "name" string)
          (param "input-json" string)
          (result $host-result)))
      (export "call" (func (type $host-call)))))
  (import "pi:extension/host" (instance $host (type $host-interface)))

  (core module $guest
    (memory (export "memory") 2)
    (global $heap (mut i32) (i32.const 8192))
    (data (i32.const 0) "@@REGISTRATION_DATA@@")
    (data (i32.const 2048) "@@TOOL_DATA@@")
    (data (i32.const 4096) "@@SLASH_DATA@@")

    (func $realloc
      (export "cabi_realloc")
      (param $old-ptr i32)
      (param $old-size i32)
      (param $align i32)
      (param $new-size i32)
      (result i32)
      (local $result i32)
      (if (result i32) (i32.eqz (local.get $new-size))
        (then (i32.const 0))
        (else
          (local.set $result
            (i32.and
              (i32.add (global.get $heap) (i32.const 7))
              (i32.const -8)))
          (global.set $heap
            (i32.add (local.get $result) (local.get $new-size)))
          (local.get $result))))

    (func $ok-result (param $data i32) (param $len i32) (result i32)
      (i32.store (i32.const 7168) (i32.const 0))
      (i32.store offset=4 (i32.const 7168) (local.get $data))
      (i32.store offset=8 (i32.const 7168) (local.get $len))
      (i32.const 7168))

    (func (export "init") (param i32 i32) (result i32)
      (call $ok-result (i32.const 0) (i32.const @@REGISTRATION_LEN@@)))
    (func (export "cabi_post_init") (param i32))

    (func (export "handle-tool") (param i32 i32 i32 i32) (result i32)
      (call $ok-result (i32.const 2048) (i32.const @@TOOL_LEN@@)))
    (func (export "cabi_post_handle-tool") (param i32))

    (func (export "handle-slash")
      (param i32 i32 i32 i32 i32 i32)
      (result i32)
      (call $ok-result (i32.const 4096) (i32.const @@SLASH_LEN@@)))
    (func (export "cabi_post_handle-slash") (param i32))

    (func (export "handle-event") (param i32 i32) (result i32)
      unreachable)
    (func (export "cabi_post_handle-event") (param i32))
    (func (export "shutdown")))

  (core module $host-call-adapter
    (type $lowered-host-call-type
      (func (param i32 i32 i32 i32 i32)))
    (import "host" "call"
      (func $imported-lowered-host-call (type $lowered-host-call-type)))
    (func (export "handle-tool")
      (param $name-ptr i32)
      (param $name-len i32)
      (param $input-ptr i32)
      (param $input-len i32)
      (result i32)
      (local.get $name-ptr)
      (local.get $name-len)
      (local.get $input-ptr)
      (local.get $input-len)
      (i32.const 7168)
      (call $imported-lowered-host-call)
      (i32.const 7168))
    (func (export "cabi_post_handle-tool") (param i32)))

  (core instance $guest-instance (instantiate $guest))
  (alias core export $guest-instance "memory" (core memory $memory))
  (alias core export $guest-instance "cabi_realloc" (core func $realloc))
  (alias export $host "call" (func $host-call))
  (core func $lowered-host-call
    (canon lower
      (func $host-call)
      (memory $memory)
      (realloc $realloc)
      string-encoding=utf8))
  (core instance $host-call-imports
    (export "call" (func $lowered-host-call)))
  (core instance $host-call-adapter-instance
    (instantiate $host-call-adapter
      (with "host" (instance $host-call-imports))))
  (alias core export $host-call-adapter-instance "handle-tool"
    (core func $adapter-handle-tool))
  (alias core export $host-call-adapter-instance "cabi_post_handle-tool"
    (core func $adapter-post-handle-tool))

  (type $string-result (result string (error string)))
  (type $init-type
    (func (param "manifest-json" string) (result $string-result)))
  (type $handle-tool-type
    (func
      (param "name" string)
      (param "input-json" string)
      (result $string-result)))
  (type $string-list (list string))
  (type $handle-slash-type
    (func
      (param "command" string)
      (param "args" $string-list)
      (param "input-json" string)
      (result $string-result)))
  (type $handle-event-type
    (func (param "event-json" string) (result $string-result)))
  (type $shutdown-type (func))

  (alias core export $guest-instance "init" (core func $core-init))
  (alias core export $guest-instance "cabi_post_init" (core func $post-init))
  (func $init (type $init-type)
    (canon lift
      (core func $core-init)
      (memory $memory)
      (realloc $realloc)
      string-encoding=utf8
      (post-return $post-init)))

  (func $handle-tool (type $handle-tool-type)
    (canon lift
      (core func $adapter-handle-tool)
      (memory $memory)
      (realloc $realloc)
      string-encoding=utf8
      (post-return $adapter-post-handle-tool)))

  (alias core export $guest-instance "handle-slash" (core func $core-handle-slash))
  (alias core export $guest-instance "cabi_post_handle-slash" (core func $post-handle-slash))
  (func $handle-slash (type $handle-slash-type)
    (canon lift
      (core func $core-handle-slash)
      (memory $memory)
      (realloc $realloc)
      string-encoding=utf8
      (post-return $post-handle-slash)))

  (alias core export $guest-instance "handle-event" (core func $core-handle-event))
  (alias core export $guest-instance "cabi_post_handle-event" (core func $post-handle-event))
  (func $handle-event (type $handle-event-type)
    (canon lift
      (core func $core-handle-event)
      (memory $memory)
      (realloc $realloc)
      string-encoding=utf8
      (post-return $post-handle-event)))

  (alias core export $guest-instance "shutdown" (core func $core-shutdown))
  (func $shutdown (type $shutdown-type)
    (canon lift (core func $core-shutdown)))

  (instance $extension
    (export "init" (func $init))
    (export "handle-tool" (func $handle-tool))
    (export "handle-slash" (func $handle-slash))
    (export "handle-event" (func $handle-event))
    (export "shutdown" (func $shutdown)))
  (export "pi:extension/extension" (instance $extension)))
"#;

    fn real_component_fixture() -> Vec<u8> {
        let registration =
            serde_json::to_string(&registration_payload()).expect("serialize registration");
        let tool_output = serde_json::to_string(&ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new("wasm-tool-ok"))],
            details: None,
            is_error: false,
        })
        .expect("serialize tool output");
        let slash_output = serde_json::to_string(&ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new("wasm-slash-ok"))],
            details: None,
            is_error: false,
        })
        .expect("serialize slash output");
        assert!(
            registration.len() < 2048,
            "registration fixture is too large"
        );
        assert!(tool_output.len() < 2048, "tool fixture is too large");
        assert!(slash_output.len() < 3072, "slash fixture is too large");

        let registration_data = wat_data(&registration);
        let tool_data = wat_data(&tool_output);
        let slash_data = wat_data(&slash_output);
        let registration_len = registration.len();
        let tool_len = tool_output.len();
        let slash_len = slash_output.len();

        let component = REAL_COMPONENT_WAT_TEMPLATE
            .replace("@@REGISTRATION_DATA@@", &registration_data)
            .replace("@@TOOL_DATA@@", &tool_data)
            .replace("@@SLASH_DATA@@", &slash_data)
            .replace("@@REGISTRATION_LEN@@", &registration_len.to_string())
            .replace("@@TOOL_LEN@@", &tool_len.to_string())
            .replace("@@SLASH_LEN@@", &slash_len.to_string());

        wat::parse_str(component).expect("compile real component fixture")
    }

    #[test]
    fn wasm_host_loads_instantiates_and_calls_real_component() {
        let dir = tempdir().expect("tempdir");
        let component_path = dir.path().join("real-extension.wasm");
        std::fs::write(&component_path, real_component_fixture()).expect("write component fixture");

        let host = WasmExtensionHost::new(dir.path(), permissive_policy())
            .expect("create WASM extension host");
        let extension = host
            .load_from_path(&component_path)
            .expect("load component path");
        let mut instance =
            run_async(host.instantiate(&extension)).expect("instantiate real component");

        let registration_json = run_async(instance.init(r#"{"name":"fixture-manifest"}"#))
            .expect("initialize real component");
        let registration: RegisterPayload =
            serde_json::from_str(&registration_json).expect("parse registration");
        assert_eq!(registration.name, "ext.test");
        assert_eq!(registration.api_version, PROTOCOL_VERSION);

        let env_call = HostCallPayload {
            call_id: "call-component-env".to_string(),
            capability: "env".to_string(),
            method: "env".to_string(),
            params: json!({ "name": "PI_TEST_ENV" }),
            timeout_ms: None,
            cancel_token: None,
            context: None,
        };
        let env_call_json = serde_json::to_string(&env_call).expect("serialize env hostcall");
        let tool_json = run_async(instance.handle_tool("env", &env_call_json))
            .expect("call imported host function through the real component ABI");
        let tool_output: Value = serde_json::from_str(&tool_json).expect("parse hostcall output");
        assert!(
            tool_output
                .get("values")
                .and_then(Value::as_object)
                .is_some_and(|values| values.contains_key("PI_TEST_ENV")),
            "unexpected env hostcall output: {tool_output}"
        );

        let slash_json = run_async(instance.handle_slash(
            "fixture",
            &["alpha".to_string(), "beta".to_string()],
            r#"{"value":2}"#,
        ))
        .expect("call real component slash export");
        let slash_output: ToolOutput =
            serde_json::from_str(&slash_json).expect("parse slash output");
        assert!(matches!(
            &slash_output.content[0],
            ContentBlock::Text(text) if text.text == "wasm-slash-ok"
        ));

        run_async(instance.shutdown()).expect("shut down real component");
    }

    #[test]
    fn wasm_host_real_component_reports_malformed_artifact_and_guest_trap() {
        let dir = tempdir().expect("tempdir");
        let host = WasmExtensionHost::new(dir.path(), permissive_policy())
            .expect("create WASM extension host");

        let malformed_path = dir.path().join("core-module-not-component.wasm");
        std::fs::write(
            &malformed_path,
            wat::parse_str("(module)").expect("compile core module"),
        )
        .expect("write malformed component fixture");
        let malformed = host
            .load_from_path(&malformed_path)
            .expect("existing artifact passes path validation");
        let malformed_error = run_async(host.instantiate(&malformed))
            .err()
            .expect("core module must not instantiate as a component");
        assert!(
            malformed_error
                .to_string()
                .contains("Failed to load WASM component"),
            "unexpected malformed-component error: {malformed_error}"
        );

        let component_path = dir.path().join("trapping-extension.wasm");
        std::fs::write(&component_path, real_component_fixture())
            .expect("write trapping component fixture");
        let extension = host
            .load_from_path(&component_path)
            .expect("load trapping component path");
        let mut instance =
            run_async(host.instantiate(&extension)).expect("instantiate trapping component");
        let trap = run_async(instance.handle_event(r#"{"type":"fixture"}"#))
            .expect_err("guest unreachable must surface as a trap");
        let trap_message = trap.to_string();
        assert!(
            trap_message.contains("WASM handle-event failed")
                && trap_message.contains("error while executing at wasm backtrace"),
            "unexpected guest trap error: {trap_message}"
        );
    }

    #[derive(Debug, Clone)]
    struct CapturedEvent {
        level: tracing::Level,
        fields: BTreeMap<String, String>,
    }

    #[derive(Clone, Default)]
    struct CaptureLayer {
        events: Arc<Mutex<Vec<CapturedEvent>>>,
    }

    impl CaptureLayer {
        fn snapshot(&self) -> Vec<CapturedEvent> {
            self.events
                .lock()
                .expect("events mutex")
                .iter()
                .cloned()
                .collect()
        }
    }

    struct FieldVisitor<'a> {
        fields: &'a mut BTreeMap<String, String>,
    }

    impl tracing::field::Visit for FieldVisitor<'_> {
        fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }

        fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }

        fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }

        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.fields
                .insert(field.name().to_string(), format!("{value:?}"));
        }
    }

    impl<S> tracing_subscriber::Layer<S> for CaptureLayer
    where
        S: tracing::Subscriber,
    {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut fields = BTreeMap::new();
            let mut visitor = FieldVisitor {
                fields: &mut fields,
            };
            event.record(&mut visitor);
            self.events
                .lock()
                .expect("events mutex")
                .push(CapturedEvent {
                    level: *event.metadata().level(),
                    fields,
                });
        }
    }

    fn capture_tracing_events<T>(f: impl FnOnce() -> T) -> (T, Vec<CapturedEvent>) {
        use tracing_subscriber::layer::SubscriberExt as _;

        let capture = CaptureLayer::default();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        let result = tracing::subscriber::with_default(subscriber, f);
        (result, capture.snapshot())
    }

    fn find_policy_decisions<'a>(
        events: &'a [CapturedEvent],
        call_id: &str,
    ) -> Vec<&'a CapturedEvent> {
        events
            .iter()
            .filter(|event| {
                event
                    .fields
                    .get("event")
                    .is_some_and(|value| value == "policy.decision")
                    && event
                        .fields
                        .get("call_id")
                        .is_some_and(|value| value == call_id)
            })
            .collect()
    }

    fn assert_policy_decision_logged(
        events: &[CapturedEvent],
        call_id: &str,
        capability: &str,
        decision: &str,
    ) {
        let matching = find_policy_decisions(events, call_id);
        assert!(
            !matching.is_empty(),
            "expected policy.decision log for call_id={call_id}; got events: {events:#?}"
        );
        assert!(
            matching.iter().any(|event| {
                event
                    .fields
                    .get("capability")
                    .is_some_and(|value| value == capability)
                    && event
                        .fields
                        .get("decision")
                        .is_some_and(|value| value == decision)
                    && event
                        .fields
                        .get("extension_id")
                        .is_some_and(|value| value.contains("ext.test"))
            }),
            "expected policy.decision with capability={capability} decision={decision} extension_id=ext.test; got: {matching:#?}"
        );
    }

    #[derive(Debug)]
    struct SleepTool;

    #[async_trait]
    impl Tool for SleepTool {
        fn name(&self) -> &'static str {
            "sleep"
        }

        fn label(&self) -> &'static str {
            "sleep"
        }

        fn description(&self) -> &'static str {
            "sleep tool"
        }

        fn parameters(&self) -> Value {
            json!({ "type": "object" })
        }

        async fn execute(
            &self,
            _tool_call_id: &str,
            _input: Value,
            _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
        ) -> Result<ToolOutput> {
            sleep(wall_now(), Duration::from_millis(200)).await;
            Ok(ToolOutput {
                content: vec![],
                details: None,
                is_error: false,
            })
        }
    }

    #[test]
    fn wasm_host_env_requires_allowlist() {
        let dir = tempdir().expect("tempdir");
        let cwd = dir.path().to_path_buf();

        let mut state = HostState::new(permissive_policy(), cwd).expect("host state");
        state
            .apply_registration(&registration_payload())
            .expect("apply registration");

        let allowed_call = HostCallPayload {
            call_id: "call-env-1".to_string(),
            capability: "env".to_string(),
            method: "env".to_string(),
            params: json!({ "name": "PI_TEST_ENV" }),
            timeout_ms: None,
            cancel_token: None,
            context: None,
        };

        let allowed_json = serde_json::to_string(&allowed_call).expect("serialize hostcall");
        let allowed_out = run_async(async {
            host::Host::call(&mut state, "env".to_string(), allowed_json).await
        })
        .expect("env hostcall ok");

        let out: Value = serde_json::from_str(&allowed_out).expect("parse env output");
        let values = out
            .get("values")
            .and_then(Value::as_object)
            .expect("values object");
        assert!(values.get("PI_TEST_ENV").is_some());

        let denied_call = HostCallPayload {
            call_id: "call-env-2".to_string(),
            capability: "env".to_string(),
            method: "env".to_string(),
            params: json!({ "name": "NOT_ALLOWED_ENV" }),
            timeout_ms: None,
            cancel_token: None,
            context: None,
        };

        let denied_json = serde_json::to_string(&denied_call).expect("serialize hostcall");
        let err_json =
            run_async(async { host::Host::call(&mut state, "env".to_string(), denied_json).await })
                .expect_err("env hostcall denied");
        let err: HostCallError = serde_json::from_str(&err_json).expect("parse error json");
        assert_eq!(err.code, HostCallErrorCode::Denied);
    }

    #[test]
    fn wasm_host_env_denied_by_policy_even_when_allowlisted() {
        let dir = tempdir().expect("tempdir");
        let cwd = dir.path().to_path_buf();

        let mut state = HostState::new(ExtensionPolicy::default(), cwd).expect("host state");
        state
            .apply_registration(&registration_payload())
            .expect("apply registration");

        let call = HostCallPayload {
            call_id: "call-env-policy-deny".to_string(),
            capability: "env".to_string(),
            method: "env".to_string(),
            params: json!({ "name": "PI_TEST_ENV" }),
            timeout_ms: None,
            cancel_token: None,
            context: None,
        };

        let ((outcome, ()), events) = capture_tracing_events(|| {
            let json = serde_json::to_string(&call).expect("serialize hostcall");
            let outcome =
                run_async(async { host::Host::call(&mut state, "env".to_string(), json).await });
            (outcome, ())
        });

        let err_json = outcome.expect_err("env hostcall denied by policy");
        let err: HostCallError = serde_json::from_str(&err_json).expect("parse error json");
        assert_eq!(err.code, HostCallErrorCode::Denied);
        assert_policy_decision_logged(&events, &call.call_id, "env", "Deny");
    }

    #[test]
    fn wasm_host_fs_respects_manifest_scopes() {
        let dir = tempdir().expect("tempdir");
        let cwd = dir.path().to_path_buf();
        std::fs::write(dir.path().join("file.txt"), "hello").expect("write file");

        let mut state = HostState::new(permissive_policy(), cwd).expect("host state");
        state
            .apply_registration(&registration_payload())
            .expect("apply registration");

        let read_call = HostCallPayload {
            call_id: "call-fs-read".to_string(),
            capability: "read".to_string(),
            method: "fs".to_string(),
            params: json!({ "op": "read", "path": "file.txt" }),
            timeout_ms: None,
            cancel_token: None,
            context: None,
        };

        let read_json = serde_json::to_string(&read_call).expect("serialize hostcall");
        let read_out =
            run_async(async { host::Host::call(&mut state, "fs".to_string(), read_json).await })
                .expect("fs read ok");
        let out: Value = serde_json::from_str(&read_out).expect("parse fs output");
        assert_eq!(out.get("text").and_then(Value::as_str), Some("hello"));

        let write_call = HostCallPayload {
            call_id: "call-fs-write".to_string(),
            capability: "write".to_string(),
            method: "fs".to_string(),
            params: json!({ "op": "write", "path": "out.txt", "encoding": "utf8", "data": "hi" }),
            timeout_ms: None,
            cancel_token: None,
            context: None,
        };

        let write_json = serde_json::to_string(&write_call).expect("serialize hostcall");
        let err_json =
            run_async(async { host::Host::call(&mut state, "fs".to_string(), write_json).await })
                .expect_err("fs write denied");
        let err: HostCallError = serde_json::from_str(&err_json).expect("parse error json");
        assert_eq!(err.code, HostCallErrorCode::Denied);
    }

    #[test]
    fn wasm_host_v2_manifest_denies_connector_class_mismatch() {
        let dir = tempdir().expect("tempdir");
        let cwd = dir.path().to_path_buf();
        std::fs::write(dir.path().join("file.txt"), "hello").expect("write file");

        let mut state = HostState::new(permissive_policy(), cwd).expect("host state");
        state
            .apply_registration(&registration_payload_v2_read_fs_scope())
            .expect("apply registration");

        let call = HostCallPayload {
            call_id: "call-v2-connector-class-deny".to_string(),
            capability: "read".to_string(),
            method: "tool".to_string(),
            params: json!({ "name": "read", "input": { "file_path": "file.txt" } }),
            timeout_ms: None,
            cancel_token: None,
            context: None,
        };

        let err_json = {
            let json = serde_json::to_string(&call).expect("serialize hostcall");
            run_async(async { host::Host::call(&mut state, "tool".to_string(), json).await })
                .expect_err("tool read should be denied by connector class scope")
        };
        let err: HostCallError = serde_json::from_str(&err_json).expect("parse error json");
        assert_eq!(err.code, HostCallErrorCode::Denied);
        assert!(
            err.message.contains("connector class"),
            "expected connector class guidance, got: {}",
            err.message
        );
    }

    #[test]
    fn wasm_host_v2_manifest_denies_hostcall_class_mismatch() {
        let dir = tempdir().expect("tempdir");
        let cwd = dir.path().to_path_buf();
        std::fs::write(dir.path().join("file.txt"), "hello").expect("write file");

        let mut state = HostState::new(permissive_policy(), cwd).expect("host state");
        state
            .apply_registration(&registration_payload_v2_read_fs_scope())
            .expect("apply registration");

        let call = HostCallPayload {
            call_id: "call-v2-hostcall-class-deny".to_string(),
            capability: "read".to_string(),
            method: "fs".to_string(),
            params: json!({ "op": "list", "path": "." }),
            timeout_ms: None,
            cancel_token: None,
            context: None,
        };

        let err_json = {
            let json = serde_json::to_string(&call).expect("serialize hostcall");
            run_async(async { host::Host::call(&mut state, "fs".to_string(), json).await })
                .expect_err("fs.list should be denied when only fs.read is allowed")
        };
        let err: HostCallError = serde_json::from_str(&err_json).expect("parse error json");
        assert_eq!(err.code, HostCallErrorCode::Denied);
        assert!(
            err.message.contains("hostcall class"),
            "expected hostcall class guidance, got: {}",
            err.message
        );
    }

    #[test]
    fn wasm_host_v2_manifest_allows_matching_connector_and_hostcall_classes() {
        let dir = tempdir().expect("tempdir");
        let cwd = dir.path().to_path_buf();
        std::fs::write(dir.path().join("file.txt"), "hello").expect("write file");

        let mut state = HostState::new(permissive_policy(), cwd).expect("host state");
        state
            .apply_registration(&registration_payload_v2_read_fs_scope())
            .expect("apply registration");

        let call = HostCallPayload {
            call_id: "call-v2-hostcall-class-allow".to_string(),
            capability: "read".to_string(),
            method: "fs".to_string(),
            params: json!({ "op": "read", "path": "file.txt" }),
            timeout_ms: None,
            cancel_token: None,
            context: None,
        };

        let out_json = {
            let json = serde_json::to_string(&call).expect("serialize hostcall");
            run_async(async { host::Host::call(&mut state, "fs".to_string(), json).await })
                .expect("fs.read should be allowed by matching v2 scope classes")
        };
        let out: Value = serde_json::from_str(&out_json).expect("parse fs output");
        assert_eq!(out.get("text").and_then(Value::as_str), Some("hello"));
    }

    #[test]
    fn wasm_host_fs_defaults_to_read_only_without_manifest() {
        let dir = tempdir().expect("tempdir");
        let cwd = dir.path().to_path_buf();
        std::fs::write(dir.path().join("file.txt"), "hello").expect("write file");

        let mut state = HostState::new(permissive_policy(), cwd).expect("host state");

        let read_call = HostCallPayload {
            call_id: "call-fs-read-default".to_string(),
            capability: "read".to_string(),
            method: "fs".to_string(),
            params: json!({ "op": "read", "path": "file.txt" }),
            timeout_ms: None,
            cancel_token: None,
            context: None,
        };
        let read_json = serde_json::to_string(&read_call).expect("serialize hostcall");
        let read_out =
            run_async(async { host::Host::call(&mut state, "fs".to_string(), read_json).await })
                .expect("fs read ok");
        let out: Value = serde_json::from_str(&read_out).expect("parse fs output");
        assert_eq!(out.get("text").and_then(Value::as_str), Some("hello"));

        let write_call = HostCallPayload {
            call_id: "call-fs-write-default".to_string(),
            capability: "write".to_string(),
            method: "fs".to_string(),
            params: json!({
                "op": "write",
                "path": "out.txt",
                "encoding": "utf8",
                "data": "hi"
            }),
            timeout_ms: None,
            cancel_token: None,
            context: None,
        };
        let write_json = serde_json::to_string(&write_call).expect("serialize hostcall");
        let err_json =
            run_async(async { host::Host::call(&mut state, "fs".to_string(), write_json).await })
                .expect_err("fs write denied by least-privilege default");
        let err: HostCallError = serde_json::from_str(&err_json).expect("parse error json");
        assert_eq!(err.code, HostCallErrorCode::Denied);
        assert!(
            err.message.contains("No allowed roots configured"),
            "expected denial message, got: {}",
            err.message
        );
    }

    #[test]
    fn wasm_host_fs_write_succeeds_with_write_scope_and_logs_policy() {
        let dir = tempdir().expect("tempdir");
        let cwd = dir.path().to_path_buf();

        let mut state = HostState::new(permissive_policy(), cwd).expect("host state");
        state
            .apply_registration(&registration_payload_with_write_scope())
            .expect("apply registration");

        let call = HostCallPayload {
            call_id: "call-fs-write-ok".to_string(),
            capability: "write".to_string(),
            method: "fs".to_string(),
            params: json!({ "op": "write", "path": "out.txt", "encoding": "utf8", "data": "hi" }),
            timeout_ms: None,
            cancel_token: None,
            context: None,
        };

        let ((out, ()), events) = capture_tracing_events(|| {
            let json = serde_json::to_string(&call).expect("serialize hostcall");
            let out =
                run_async(async { host::Host::call(&mut state, "fs".to_string(), json).await })
                    .expect("fs write ok");
            (out, ())
        });

        let out: Value = serde_json::from_str(&out).expect("parse fs output");
        assert_eq!(out.get("bytes_written").and_then(Value::as_u64), Some(2));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("out.txt")).expect("read out.txt"),
            "hi"
        );
        assert_policy_decision_logged(&events, &call.call_id, "write", "Allow");
    }

    #[test]
    fn wasm_host_tool_call_times_out_and_returns_timeout_error() {
        let dir = tempdir().expect("tempdir");
        let cwd = dir.path().to_path_buf();

        let mut state = HostState::new(permissive_policy(), cwd).expect("host state");
        state.tools = Arc::new(ToolRegistry::from_tools(vec![Box::new(SleepTool)]));
        state
            .apply_registration(&registration_payload())
            .expect("apply registration");

        let call = HostCallPayload {
            call_id: "call-tool-timeout".to_string(),
            capability: "tool".to_string(),
            method: "tool".to_string(),
            params: json!({ "name": "sleep", "input": {} }),
            timeout_ms: Some(50),
            cancel_token: None,
            context: None,
        };

        let ((outcome, ()), events) = capture_tracing_events(|| {
            let json = serde_json::to_string(&call).expect("serialize hostcall");
            let outcome =
                run_async(async { host::Host::call(&mut state, "tool".to_string(), json).await });
            (outcome, ())
        });

        let err_json = outcome.expect_err("tool hostcall timeout");
        let err: HostCallError = serde_json::from_str(&err_json).expect("parse error json");
        assert_eq!(err.code, HostCallErrorCode::Timeout);
        assert_policy_decision_logged(&events, &call.call_id, "tool", "Allow");
    }

    #[test]
    fn wasm_host_exec_denied_by_default_policy_and_logs_decision() {
        let dir = tempdir().expect("tempdir");
        let cwd = dir.path().to_path_buf();

        let mut state = HostState::new(ExtensionPolicy::default(), cwd).expect("host state");
        state
            .apply_registration(&registration_payload())
            .expect("apply registration");

        let call = HostCallPayload {
            call_id: "call-exec-deny".to_string(),
            capability: "exec".to_string(),
            method: "exec".to_string(),
            params: json!({ "command": "echo hi" }),
            timeout_ms: None,
            cancel_token: None,
            context: None,
        };

        let ((outcome, ()), events) = capture_tracing_events(|| {
            let json = serde_json::to_string(&call).expect("serialize hostcall");
            let outcome =
                run_async(async { host::Host::call(&mut state, "exec".to_string(), json).await });
            (outcome, ())
        });

        let err_json = outcome.expect_err("exec denied");
        let err: HostCallError = serde_json::from_str(&err_json).expect("parse error json");
        assert_eq!(err.code, HostCallErrorCode::Denied);
        assert_policy_decision_logged(&events, &call.call_id, "exec", "Deny");
    }

    #[test]
    fn wasm_host_exec_succeeds_when_policy_allows() {
        let dir = tempdir().expect("tempdir");
        let cwd = dir.path().to_path_buf();

        let mut state = HostState::new(permissive_policy(), cwd).expect("host state");
        state
            .apply_registration(&registration_payload())
            .expect("apply registration");

        let call = HostCallPayload {
            call_id: "call-exec-ok".to_string(),
            capability: "exec".to_string(),
            method: "exec".to_string(),
            params: json!({ "command": "echo hello" }),
            timeout_ms: None,
            cancel_token: None,
            context: None,
        };

        let out_json = {
            let json = serde_json::to_string(&call).expect("serialize hostcall");
            run_async(async { host::Host::call(&mut state, "exec".to_string(), json).await })
                .expect("exec ok")
        };

        let output: ToolOutput = serde_json::from_str(&out_json).expect("parse tool output");
        assert!(!output.is_error);
        let text = output
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("hello"));
    }

    #[test]
    fn wasm_host_http_get_succeeds_against_local_server_when_configured() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().expect("local addr");

        let join = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let response = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
            let _ = stream.write_all(response);
        });

        let dir = tempdir().expect("tempdir");
        let cwd = dir.path().to_path_buf();

        let mut state = HostState::new(strict_policy(&["http"], &[]), cwd).expect("host state");
        state
            .apply_registration(&registration_payload())
            .expect("apply registration");
        state.http = HttpConnector::new(HttpConnectorConfig {
            require_tls: false,
            allowlist: vec!["127.0.0.1".to_string()],
            ..Default::default()
        });

        let url = format!("http://127.0.0.1:{}/", addr.port());
        let call = HostCallPayload {
            call_id: "call-http-ok".to_string(),
            capability: "http".to_string(),
            method: "http".to_string(),
            params: json!({ "url": url, "method": "GET" }),
            timeout_ms: Some(2000),
            cancel_token: None,
            context: None,
        };

        let out_json = {
            let json = serde_json::to_string(&call).expect("serialize hostcall");
            run_async(async { host::Host::call(&mut state, "http".to_string(), json).await })
                .expect("http ok")
        };

        let out: Value = serde_json::from_str(&out_json).expect("parse http output");
        assert_eq!(out.get("status").and_then(Value::as_u64), Some(200));
        assert_eq!(out.get("body").and_then(Value::as_str), Some("ok"));

        join.join().expect("server thread join");
    }

    #[test]
    fn wasm_host_http_denied_by_default_without_http_allowlist_scope() {
        let dir = tempdir().expect("tempdir");
        let cwd = dir.path().to_path_buf();

        let mut state = HostState::new(strict_policy(&["http"], &[]), cwd).expect("host state");
        state
            .apply_registration(&registration_payload())
            .expect("apply registration");

        let call = HostCallPayload {
            call_id: "call-http-deny-default".to_string(),
            capability: "http".to_string(),
            method: "http".to_string(),
            params: json!({ "url": "https://example.com", "method": "GET" }),
            timeout_ms: Some(500),
            cancel_token: None,
            context: None,
        };

        let err_json = {
            let json = serde_json::to_string(&call).expect("serialize hostcall");
            run_async(async { host::Host::call(&mut state, "http".to_string(), json).await })
                .expect_err("http should be denied without scoped allowlist")
        };
        let err: HostCallError = serde_json::from_str(&err_json).expect("parse error");
        assert_eq!(err.code, HostCallErrorCode::Denied);
        assert!(
            err.message.contains("allowlist"),
            "expected allowlist guidance, got: {}",
            err.message
        );
    }

    #[test]
    fn wasm_host_http_denied_when_http_capability_has_no_hosts_scope() {
        let dir = tempdir().expect("tempdir");
        let cwd = dir.path().to_path_buf();

        let mut payload = registration_payload();
        payload
            .capability_manifest
            .as_mut()
            .expect("capability manifest")
            .capabilities
            .push(CapabilityRequirement {
                capability: "http".to_string(),
                methods: vec!["http".to_string()],
                intents: Vec::new(),
                connector_classes: Vec::new(),
                hostcall_classes: Vec::new(),
                risk_tier: None,
                scope: None,
                provenance: None,
            });

        let mut state = HostState::new(strict_policy(&["http"], &[]), cwd).expect("host state");
        state
            .apply_registration(&payload)
            .expect("apply registration");

        let call = HostCallPayload {
            call_id: "call-http-deny-empty-scope".to_string(),
            capability: "http".to_string(),
            method: "http".to_string(),
            params: json!({ "url": "https://example.com", "method": "GET" }),
            timeout_ms: Some(500),
            cancel_token: None,
            context: None,
        };

        let err_json = {
            let json = serde_json::to_string(&call).expect("serialize hostcall");
            run_async(async { host::Host::call(&mut state, "http".to_string(), json).await })
                .expect_err("http should be denied when hosts scope is omitted")
        };
        let err: HostCallError = serde_json::from_str(&err_json).expect("parse error");
        assert_eq!(err.code, HostCallErrorCode::Denied);
        assert!(
            err.message.contains("allowlist"),
            "expected allowlist guidance, got: {}",
            err.message
        );
    }
}
