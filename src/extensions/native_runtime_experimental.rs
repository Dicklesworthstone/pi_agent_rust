//! Retained disabled prototype of the native extension runtime.

// This source snapshot is compiled out with `cfg(any())`. Preserve its broad
// import until reactivation work can validate a real, explicit dependency set.
use super::*;

const NATIVE_RUST_EXTENSION_SCHEMA: &str = "pi.ext.native-rust.v1";

fn default_native_rust_extension_schema() -> String {
    NATIVE_RUST_EXTENSION_SCHEMA.to_string()
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct NativeRustExtensionEntrypoint {
    #[serde(default = "default_native_rust_extension_schema")]
    schema: String,
    tools: Vec<Value>,
    slash_commands: Vec<Value>,
    shortcuts: Vec<Value>,
    providers: Vec<Value>,
    flags: Vec<Value>,
    event_hooks: Vec<String>,
    active_tools: Option<Vec<String>>,
    handlers: NativeRustExtensionHandlers,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct NativeRustExtensionHandlers {
    events: HashMap<String, Value>,
    tools: HashMap<String, Value>,
    commands: HashMap<String, Value>,
    shortcuts: HashMap<String, Value>,
    providers: HashMap<String, NativeRustProviderHandler>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct NativeRustProviderHandler {
    chunks: Vec<Value>,
}

#[derive(Debug, Clone)]
struct NativeRustExtensionSnapshot {
    id: String,
    name: String,
    version: String,
    api_version: String,
    tools: Vec<Value>,
    slash_commands: Vec<Value>,
    shortcuts: Vec<Value>,
    providers: Vec<Value>,
    flags: Vec<Value>,
    event_hooks: Vec<String>,
    active_tools: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
struct NativeRustProviderStreamState {
    chunks: Vec<Value>,
    cursor: usize,
}

#[derive(Debug, Default)]
struct NativeRustRuntimeState {
    snapshots: Vec<NativeRustExtensionSnapshot>,
    handlers_by_extension: HashMap<String, NativeRustExtensionHandlers>,
    provider_streams: HashMap<String, NativeRustProviderStreamState>,
    flag_values: HashMap<(String, String), Value>,
    next_stream_id: u64,
}

fn resolve_native_template_ref<'a>(
    bindings: &'a HashMap<String, Value>,
    path: &str,
) -> Option<&'a Value> {
    let mut segments = path.split('.');
    let first = segments.next()?;
    let mut current = bindings.get(first)?;
    for segment in segments {
        if segment.is_empty() {
            return None;
        }
        if let Ok(index) = segment.parse::<usize>() {
            current = current.get(index)?;
        } else {
            current = current.get(segment)?;
        }
    }
    Some(current)
}

fn render_native_template_value(template: &Value, bindings: &HashMap<String, Value>) -> Value {
    match template {
        Value::String(text) => {
            if let Some(path) = text.strip_prefix('$') {
                resolve_native_template_ref(bindings, path)
                    .cloned()
                    .unwrap_or_else(|| template.clone())
            } else {
                template.clone()
            }
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| render_native_template_value(item, bindings))
                .collect(),
        ),
        Value::Object(map) => {
            if let Some(path) = map.get("$ref").and_then(Value::as_str) {
                resolve_native_template_ref(bindings, path)
                    .cloned()
                    .unwrap_or(Value::Null)
            } else {
                let mut out = serde_json::Map::with_capacity(map.len());
                for (key, value) in map {
                    out.insert(key.clone(), render_native_template_value(value, bindings));
                }
                Value::Object(out)
            }
        }
        _ => template.clone(),
    }
}

fn normalize_native_provider_specs(
    providers: &mut Vec<Value>,
    handlers: &NativeRustExtensionHandlers,
) -> Result<()> {
    let mut seen = HashSet::new();
    for provider in providers.iter_mut() {
        let Some(obj) = provider.as_object_mut() else {
            continue;
        };
        let Some(provider_id) = obj.get("id").and_then(Value::as_str) else {
            continue;
        };
        seen.insert(provider_id.to_string());
        if handlers.providers.contains_key(provider_id) {
            obj.insert("hasStreamSimple".to_string(), Value::Bool(true));
            obj.insert("streamSimple".to_string(), Value::Bool(true));
        }
    }

    for provider_id in handlers.providers.keys() {
        if seen.contains(provider_id) {
            continue;
        }
        providers.push(json!({
            "id": provider_id,
            "name": provider_id,
            "streamSimple": true,
            "hasStreamSimple": true,
            "models": []
        }));
    }

    // Validate all provider specs still parse as objects with IDs.
    for provider in providers {
        let Some(provider_id) = provider.get("id").and_then(Value::as_str) else {
            return Err(Error::validation(
                "Native Rust provider spec missing required string field `id`",
            ));
        };
        if provider_id.trim().is_empty() {
            return Err(Error::validation(
                "Native Rust provider spec has empty `id`",
            ));
        }
    }

    Ok(())
}

fn load_native_rust_entrypoint(
    spec: &NativeRustExtensionLoadSpec,
) -> Result<NativeRustExtensionEntrypoint> {
    let raw = fs::read_to_string(&spec.entry_path).map_err(|err| {
        Error::validation(format!(
            "Failed to read native Rust extension entry {}: {err}",
            spec.entry_path.display()
        ))
    })?;

    let mut entrypoint: NativeRustExtensionEntrypoint =
        serde_json::from_str(&raw).map_err(|err| {
            Error::validation(format!(
                "Failed to parse native Rust extension entry {}: {err}",
                spec.entry_path.display()
            ))
        })?;

    if entrypoint.schema != NATIVE_RUST_EXTENSION_SCHEMA {
        return Err(Error::validation(format!(
            "Unsupported native Rust extension entry schema '{}' in {} (expected '{}')",
            entrypoint.schema,
            spec.entry_path.display(),
            NATIVE_RUST_EXTENSION_SCHEMA
        )));
    }

    normalize_native_provider_specs(&mut entrypoint.providers, &entrypoint.handlers)?;
    Ok(entrypoint)
}

/// Handle to the native Rust extension runtime.
///
/// This runtime is a deterministic Rust-native execution path for extension
/// hooks/tool handlers/provider streamSimple behavior expressed as structured
/// JSON templates in `*.native.json` entry files.
#[derive(Clone)]
pub struct NativeRustExtensionRuntimeHandle {
    state: Arc<RwLock<NativeRustRuntimeState>>,
}

#[allow(
    clippy::manual_let_else,
    clippy::needless_pass_by_value,
    clippy::option_if_let_else,
    clippy::significant_drop_tightening
)]
#[allow(
    clippy::manual_let_else,
    clippy::needless_pass_by_value,
    clippy::option_if_let_else,
    clippy::significant_drop_tightening
)]
impl NativeRustExtensionRuntimeHandle {
    pub async fn start() -> Result<Self> {
        Ok(Self {
            state: Arc::new(Mutex::new(NativeRustRuntimeState::default())),
        })
    }

    pub async fn shutdown(&self, _budget: Duration) -> bool {
        true
    }

    async fn load_extensions_snapshots(
        &self,
        specs: Vec<NativeRustExtensionLoadSpec>,
    ) -> Result<Vec<NativeRustExtensionSnapshot>> {
        let mut snapshots = Vec::with_capacity(specs.len());
        let mut handlers_by_extension = HashMap::with_capacity(specs.len());

        for spec in specs {
            let entrypoint = load_native_rust_entrypoint(&spec)?;
            let snapshot = NativeRustExtensionSnapshot {
                id: spec.extension_id.clone(),
                name: spec.name.clone(),
                version: spec.version.clone(),
                api_version: spec.api_version.clone(),
                tools: entrypoint.tools,
                slash_commands: entrypoint.slash_commands,
                shortcuts: entrypoint.shortcuts,
                providers: entrypoint.providers,
                flags: entrypoint.flags,
                event_hooks: entrypoint.event_hooks,
                active_tools: entrypoint.active_tools,
            };
            handlers_by_extension.insert(spec.extension_id, entrypoint.handlers);
            snapshots.push(snapshot);
        }

        {
            let mut guard = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.snapshots = snapshots.clone();
            guard.handlers_by_extension = handlers_by_extension;
            guard.provider_streams.clear();
            guard.next_stream_id = 0;
            guard.flag_values.clear();
        }

        Ok(snapshots)
    }

    pub async fn get_registered_tools(&self) -> Result<Vec<ExtensionToolDef>> {
        let guard = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut defs = Vec::new();
        for snapshot in &guard.snapshots {
            defs.extend(parse_extension_tool_defs(&snapshot.tools));
        }
        Ok(defs)
    }

    fn find_handler_template(
        state: &NativeRustRuntimeState,
        kind: NativeRustHandlerKind,
        key: &str,
    ) -> Option<Value> {
        for snapshot in &state.snapshots {
            let Some(handlers) = state.handlers_by_extension.get(&snapshot.id) else {
                continue;
            };
            let template = match kind {
                NativeRustHandlerKind::Event => handlers.events.get(key),
                NativeRustHandlerKind::Tool => handlers.tools.get(key),
                NativeRustHandlerKind::Command => handlers.commands.get(key),
                NativeRustHandlerKind::Shortcut => handlers.shortcuts.get(key),
            };
            if let Some(template) = template {
                return Some(template.clone());
            }
        }
        None
    }

    pub async fn dispatch_event(
        &self,
        event_name: String,
        event_payload: Value,
        ctx_payload: Arc<Value>,
        _timeout_ms: u64,
    ) -> Result<Value> {
        let guard = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let template =
            Self::find_handler_template(&guard, NativeRustHandlerKind::Event, &event_name);
        let Some(template) = template else {
            return Ok(Value::Null);
        };

        let mut bindings = HashMap::new();
        bindings.insert("event".to_string(), event_payload);
        bindings.insert("ctx".to_string(), (*ctx_payload).clone());
        bindings.insert("event_name".to_string(), Value::String(event_name));
        Ok(render_native_template_value(&template, &bindings))
    }

    pub async fn dispatch_event_batch(
        &self,
        events: Vec<(String, Value)>,
        ctx_payload: Arc<Value>,
        timeout_ms: u64,
    ) -> Result<Vec<Result<Value>>> {
        let mut out = Vec::with_capacity(events.len());
        for (event_name, event_payload) in events {
            out.push(
                self.dispatch_event(
                    event_name,
                    event_payload,
                    Arc::clone(&ctx_payload),
                    timeout_ms,
                )
                .await,
            );
        }
        Ok(out)
    }

    pub async fn execute_tool(
        &self,
        tool_name: String,
        tool_call_id: String,
        input: Value,
        ctx_payload: Arc<Value>,
        _timeout_ms: u64,
    ) -> Result<Value> {
        self.execute_tool_ref(&tool_name, &tool_call_id, input, ctx_payload)
            .await
    }

    pub async fn execute_tool_ref(
        &self,
        tool_name: &str,
        tool_call_id: &str,
        input: Value,
        ctx_payload: Arc<Value>,
    ) -> Result<Value> {
        let guard = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let template = Self::find_handler_template(&guard, NativeRustHandlerKind::Tool, tool_name)
            .ok_or_else(|| {
                Error::extension(format!(
                    "Native Rust extension tool handler not found for '{}'",
                    tool_name
                ))
            })?;
        let mut bindings = HashMap::new();
        bindings.insert(
            "tool_name".to_string(),
            Value::String(tool_name.to_string()),
        );
        bindings.insert(
            "tool_call_id".to_string(),
            Value::String(tool_call_id.to_string()),
        );
        bindings.insert("input".to_string(), input);
        bindings.insert("ctx".to_string(), (*ctx_payload).clone());
        Ok(render_native_template_value(&template, &bindings))
    }

    pub async fn execute_command(
        &self,
        command_name: String,
        args: String,
        ctx_payload: Arc<Value>,
        _timeout_ms: u64,
    ) -> Result<Value> {
        let guard = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let template =
            Self::find_handler_template(&guard, NativeRustHandlerKind::Command, &command_name)
                .ok_or_else(|| {
                    Error::extension(format!(
                        "Native Rust extension command handler not found for '{}'",
                        command_name
                    ))
                })?;
        let mut bindings = HashMap::new();
        bindings.insert("command_name".to_string(), Value::String(command_name));
        bindings.insert("args".to_string(), Value::String(args));
        bindings.insert("ctx".to_string(), (*ctx_payload).clone());
        Ok(render_native_template_value(&template, &bindings))
    }

    pub async fn execute_shortcut(
        &self,
        key_id: String,
        ctx_payload: Arc<Value>,
        _timeout_ms: u64,
    ) -> Result<Value> {
        let guard = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let template =
            Self::find_handler_template(&guard, NativeRustHandlerKind::Shortcut, &key_id)
                .ok_or_else(|| {
                    Error::extension(format!(
                        "Native Rust extension shortcut handler not found for '{}'",
                        key_id
                    ))
                })?;
        let mut bindings = HashMap::new();
        bindings.insert("key_id".to_string(), Value::String(key_id));
        bindings.insert("ctx".to_string(), (*ctx_payload).clone());
        Ok(render_native_template_value(&template, &bindings))
    }

    pub async fn set_flag_value(
        &self,
        extension_id: String,
        flag_name: String,
        value: Value,
    ) -> Result<()> {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.flag_values.insert((extension_id, flag_name), value);
        Ok(())
    }

    pub async fn drain_repair_events(&self) -> Vec<ExtensionRepairEvent> {
        Vec::new()
    }

    pub async fn provider_stream_simple_start(
        &self,
        provider_id: String,
        model: Value,
        context: Value,
        options: Value,
        _timeout_ms: u64,
    ) -> Result<String> {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut chunks = None;
        for snapshot in &guard.snapshots {
            let Some(handlers) = guard.handlers_by_extension.get(&snapshot.id) else {
                continue;
            };
            if let Some(provider) = handlers.providers.get(&provider_id) {
                chunks = Some(provider.chunks.clone());
                break;
            }
        }

        let templates = chunks.ok_or_else(|| {
            Error::extension(format!(
                "Native Rust provider '{}' has no streamSimple handler",
                provider_id
            ))
        })?;

        let mut bindings = HashMap::new();
        bindings.insert("provider_id".to_string(), Value::String(provider_id));
        bindings.insert("model".to_string(), model);
        bindings.insert("context".to_string(), context);
        bindings.insert("options".to_string(), options);
        let rendered_chunks = templates
            .iter()
            .map(|chunk| render_native_template_value(chunk, &bindings))
            .collect::<Vec<_>>();

        guard.next_stream_id = guard.next_stream_id.saturating_add(1);
        let stream_id = format!("native-rust-stream-{}", guard.next_stream_id);
        guard.provider_streams.insert(
            stream_id.clone(),
            NativeRustProviderStreamState {
                chunks: rendered_chunks,
                cursor: 0,
            },
        );
        Ok(stream_id)
    }

    pub async fn provider_stream_simple_next(
        &self,
        stream_id: String,
        _timeout_ms: u64,
    ) -> Result<Option<Value>> {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let done = {
            let Some(stream) = guard.provider_streams.get_mut(&stream_id) else {
                return Err(Error::extension(format!(
                    "Native Rust provider stream not found: {stream_id}"
                )));
            };
            if stream.cursor >= stream.chunks.len() {
                true
            } else {
                let value = stream.chunks[stream.cursor].clone();
                stream.cursor = stream.cursor.saturating_add(1);
                return Ok(Some(value));
            }
        };

        if done {
            guard.provider_streams.remove(&stream_id);
        }
        Ok(None)
    }

    pub async fn provider_stream_simple_cancel(
        &self,
        stream_id: String,
        _timeout_ms: u64,
    ) -> Result<()> {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.provider_streams.remove(&stream_id);
        Ok(())
    }

    pub fn provider_stream_simple_cancel_best_effort(&self, stream_id: String) {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.provider_streams.remove(&stream_id);
    }
}

#[derive(Clone, Copy)]
enum NativeRustHandlerKind {
    Event,
    Tool,
    Command,
    Shortcut,
}

/// Runtime-agnostic extension runtime handle.
#[derive(Clone)]
pub enum ExtensionRuntimeHandle {
    Js(JsExtensionRuntimeHandle),
    NativeRust(NativeRustExtensionRuntimeHandle),
}

impl From<JsExtensionRuntimeHandle> for ExtensionRuntimeHandle {
    fn from(value: JsExtensionRuntimeHandle) -> Self {
        Self::Js(value)
    }
}

impl From<NativeRustExtensionRuntimeHandle> for ExtensionRuntimeHandle {
    fn from(value: NativeRustExtensionRuntimeHandle) -> Self {
        Self::NativeRust(value)
    }
}

impl ExtensionRuntimeHandle {
    pub const fn runtime_kind(&self) -> ExtensionRuntime {
        match self {
            Self::Js(_) => ExtensionRuntime::Js,
            Self::NativeRust(_) => ExtensionRuntime::NativeRust,
        }
    }

    pub const fn runtime_name(&self) -> &'static str {
        match self {
            Self::Js(_) => "quickjs",
            Self::NativeRust(_) => "native-rust",
        }
    }

    async fn load_js_extensions_snapshots(
        &self,
        specs: Vec<JsExtensionLoadSpec>,
    ) -> Result<Vec<JsExtensionSnapshot>> {
        match self {
            Self::Js(runtime) => runtime.load_extensions_snapshots(specs).await,
            Self::NativeRust(_) => Err(Error::extension(
                "Native-rust runtime does not support JS extension load specs".to_string(),
            )),
        }
    }

    async fn load_native_extensions_snapshots(
        &self,
        specs: Vec<NativeRustExtensionLoadSpec>,
    ) -> Result<Vec<JsExtensionSnapshot>> {
        match self {
            Self::Js(_) => Err(Error::extension(
                "QuickJS runtime does not support native-rust extension load specs".to_string(),
            )),
            Self::NativeRust(runtime) => {
                runtime
                    .load_extensions_snapshots(specs)
                    .await
                    .map(|snapshots| {
                        snapshots
                            .into_iter()
                            .map(|snapshot| JsExtensionSnapshot {
                                id: snapshot.id,
                                name: snapshot.name,
                                version: snapshot.version,
                                api_version: snapshot.api_version,
                                tools: snapshot.tools,
                                slash_commands: snapshot.slash_commands,
                                shortcuts: snapshot.shortcuts,
                                providers: snapshot.providers,
                                mcp_servers: snapshot.mcp_servers,
                                flags: snapshot.flags,
                                event_hooks: snapshot.event_hooks,
                                active_tools: snapshot.active_tools,
                            })
                            .collect()
                    })
            }
        }
    }

    pub async fn shutdown(&self, budget: Duration) -> bool {
        match self {
            Self::Js(runtime) => runtime.shutdown(budget).await,
            Self::NativeRust(runtime) => runtime.shutdown(budget).await,
        }
    }

    pub async fn get_registered_tools(&self) -> Result<Vec<ExtensionToolDef>> {
        match self {
            Self::Js(runtime) => runtime.get_registered_tools().await,
            Self::NativeRust(runtime) => runtime.get_registered_tools().await,
        }
    }

    pub async fn dispatch_event(
        &self,
        event_name: String,
        event_payload: Value,
        ctx_payload: Arc<Value>,
        timeout_ms: u64,
    ) -> Result<Value> {
        match self {
            Self::Js(runtime) => {
                runtime
                    .dispatch_event(event_name, event_payload, ctx_payload, timeout_ms)
                    .await
            }
            Self::NativeRust(runtime) => {
                runtime
                    .dispatch_event(event_name, event_payload, ctx_payload, timeout_ms)
                    .await
            }
        }
    }

    pub async fn dispatch_event_batch(
        &self,
        events: Vec<(String, Value)>,
        ctx_payload: Arc<Value>,
        timeout_ms: u64,
    ) -> Result<Vec<Result<Value>>> {
        match self {
            Self::Js(runtime) => {
                runtime
                    .dispatch_event_batch(events, ctx_payload, timeout_ms)
                    .await
            }
            Self::NativeRust(runtime) => {
                runtime
                    .dispatch_event_batch(events, ctx_payload, timeout_ms)
                    .await
            }
        }
    }

    pub async fn execute_tool(
        &self,
        tool_name: String,
        tool_call_id: String,
        input: Value,
        ctx_payload: Arc<Value>,
        timeout_ms: u64,
    ) -> Result<Value> {
        self.execute_tool_ref(&tool_name, &tool_call_id, input, ctx_payload, timeout_ms)
            .await
    }

    pub async fn execute_tool_ref(
        &self,
        tool_name: &str,
        tool_call_id: &str,
        input: Value,
        ctx_payload: Arc<Value>,
        timeout_ms: u64,
    ) -> Result<Value> {
        match self {
            Self::Js(runtime) => {
                runtime
                    .execute_tool(
                        tool_name.to_string(),
                        tool_call_id.to_string(),
                        input,
                        ctx_payload,
                        timeout_ms,
                    )
                    .await
            }
            Self::NativeRust(runtime) => {
                runtime
                    .execute_tool_ref(tool_name, tool_call_id, input, ctx_payload)
                    .await
            }
        }
    }

    pub async fn execute_command(
        &self,
        command_name: String,
        args: String,
        ctx_payload: Arc<Value>,
        timeout_ms: u64,
    ) -> Result<Value> {
        match self {
            Self::Js(runtime) => {
                runtime
                    .execute_command(command_name, args, ctx_payload, timeout_ms)
                    .await
            }
            Self::NativeRust(runtime) => {
                runtime
                    .execute_command(command_name, args, ctx_payload, timeout_ms)
                    .await
            }
        }
    }

    pub async fn execute_shortcut(
        &self,
        key_id: String,
        ctx_payload: Arc<Value>,
        timeout_ms: u64,
    ) -> Result<Value> {
        match self {
            Self::Js(runtime) => {
                runtime
                    .execute_shortcut(key_id, ctx_payload, timeout_ms)
                    .await
            }
            Self::NativeRust(runtime) => {
                runtime
                    .execute_shortcut(key_id, ctx_payload, timeout_ms)
                    .await
            }
        }
    }

    pub async fn set_flag_value(
        &self,
        extension_id: String,
        flag_name: String,
        value: Value,
    ) -> Result<()> {
        match self {
            Self::Js(runtime) => runtime.set_flag_value(extension_id, flag_name, value).await,
            Self::NativeRust(runtime) => {
                runtime.set_flag_value(extension_id, flag_name, value).await
            }
        }
    }

    pub async fn provider_stream_simple_start(
        &self,
        provider_id: String,
        model: Value,
        context: Value,
        options: Value,
        timeout_ms: u64,
    ) -> Result<String> {
        match self {
            Self::Js(runtime) => {
                runtime
                    .provider_stream_simple_start(provider_id, model, context, options, timeout_ms)
                    .await
            }
            Self::NativeRust(runtime) => {
                runtime
                    .provider_stream_simple_start(provider_id, model, context, options, timeout_ms)
                    .await
            }
        }
    }

    pub async fn provider_stream_simple_next(
        &self,
        stream_id: String,
        timeout_ms: u64,
    ) -> Result<Option<Value>> {
        match self {
            Self::Js(runtime) => {
                runtime
                    .provider_stream_simple_next(stream_id, timeout_ms)
                    .await
            }
            Self::NativeRust(runtime) => {
                runtime
                    .provider_stream_simple_next(stream_id, timeout_ms)
                    .await
            }
        }
    }

    pub async fn provider_stream_simple_cancel(
        &self,
        stream_id: String,
        timeout_ms: u64,
    ) -> Result<()> {
        match self {
            Self::Js(runtime) => {
                runtime
                    .provider_stream_simple_cancel(stream_id, timeout_ms)
                    .await
            }
            Self::NativeRust(runtime) => {
                runtime
                    .provider_stream_simple_cancel(stream_id, timeout_ms)
                    .await
            }
        }
    }

    pub fn provider_stream_simple_cancel_best_effort(&self, stream_id: String) {
        match self {
            Self::Js(runtime) => runtime.provider_stream_simple_cancel_best_effort(stream_id),
            Self::NativeRust(runtime) => {
                runtime.provider_stream_simple_cancel_best_effort(stream_id);
            }
        }
    }

    pub async fn drain_repair_events(&self) -> Vec<ExtensionRepairEvent> {
        match self {
            Self::Js(runtime) => runtime.drain_repair_events().await,
            Self::NativeRust(runtime) => runtime.drain_repair_events().await,
        }
    }

    pub fn as_js(&self) -> Option<JsExtensionRuntimeHandle> {
        match self {
            Self::Js(runtime) => Some(runtime.clone()),
            Self::NativeRust(_) => None,
        }
    }

    pub fn as_native_rust(&self) -> Option<NativeRustExtensionRuntimeHandle> {
        match self {
            Self::NativeRust(runtime) => Some(runtime.clone()),
            Self::Js(_) => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionRuntimeEngineSelection {
    NativeRust,
}

impl ExtensionRuntimeEngineSelection {
    pub const ENV_VAR: &'static str = "PI_EXTENSION_RUNTIME_ENGINE";

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NativeRust => "native-rust",
        }
    }

    pub const fn from_env_value(_value: &str) -> Self {
        Self::NativeRust
    }

    #[must_use]
    pub fn from_env() -> Self {
        let value = std::env::var(Self::ENV_VAR).unwrap_or_default();
        Self::from_env_value(&value)
    }
}
