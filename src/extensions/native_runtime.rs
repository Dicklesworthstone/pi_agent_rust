//! Active deterministic native extension runtime scaffold.

use super::{
    Error, ExtensionRepairEvent, ExtensionToolDef, JsExtensionLoadSpec, JsExtensionRuntimeHandle,
    JsExtensionSnapshot, NativeRustExtensionLoadSpec, Result, extract_slash_command_name,
    parse_extension_tool_defs,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs;
use std::sync::{Arc, RwLock};
use std::time::Duration;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct NativeRustExtensionDescriptor {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    api_version: String,
    #[serde(default)]
    tools: Vec<Value>,
    #[serde(default)]
    slash_commands: Vec<Value>,
    #[serde(default)]
    shortcuts: Vec<Value>,
    #[serde(default)]
    providers: Vec<Value>,
    #[serde(default)]
    mcp_servers: Vec<Value>,
    #[serde(default)]
    flags: Vec<Value>,
    #[serde(default)]
    event_hooks: Vec<String>,
    #[serde(default)]
    active_tools: Option<Vec<String>>,
    #[serde(default)]
    event_responses: HashMap<String, Value>,
    #[serde(default)]
    tool_outputs: HashMap<String, Value>,
    #[serde(default)]
    command_outputs: HashMap<String, Value>,
    #[serde(default)]
    shortcut_outputs: HashMap<String, Value>,
    #[serde(default)]
    provider_streams: HashMap<String, Vec<Value>>,
}

#[derive(Debug, Clone)]
struct NativeRustLoadedExtension {
    snapshot: JsExtensionSnapshot,
    event_responses: HashMap<String, Value>,
    tool_outputs: HashMap<String, Value>,
    command_outputs: HashMap<String, Value>,
    shortcut_outputs: HashMap<String, Value>,
    provider_streams: HashMap<String, Arc<[Value]>>,
}

#[derive(Debug, Clone)]
struct NativeRustProviderStreamCursor {
    chunks: Arc<[Value]>,
    next_index: usize,
}

#[derive(Debug, Default)]
struct NativeRustRuntimeState {
    extensions: Vec<NativeRustLoadedExtension>,
    tool_extension_index: HashMap<String, usize>,
    command_extension_index: HashMap<String, usize>,
    shortcut_extension_index: HashMap<String, usize>,
    provider_stream_extension_index: HashMap<String, usize>,
    event_hook_extension_indexes: HashMap<String, Vec<usize>>,
    registered_tools: Vec<ExtensionToolDef>,
    streams: HashMap<String, NativeRustProviderStreamCursor>,
    next_stream_id: u64,
    flags: HashMap<(String, String), Value>,
    repair_events: Vec<ExtensionRepairEvent>,
}

impl NativeRustRuntimeState {
    fn load_extensions(
        &mut self,
        loaded: Vec<NativeRustLoadedExtension>,
    ) -> Vec<JsExtensionSnapshot> {
        let snapshots = loaded
            .iter()
            .map(|extension| extension.snapshot.clone())
            .collect::<Vec<_>>();
        self.extensions = loaded;
        self.streams.clear();
        self.next_stream_id = 0;
        self.rebuild_indexes();
        snapshots
    }

    fn rebuild_indexes(&mut self) {
        self.tool_extension_index.clear();
        self.command_extension_index.clear();
        self.shortcut_extension_index.clear();
        self.provider_stream_extension_index.clear();
        self.event_hook_extension_indexes.clear();
        self.registered_tools.clear();

        for (extension_index, extension) in self.extensions.iter().enumerate() {
            self.registered_tools
                .extend(parse_extension_tool_defs(&extension.snapshot.tools));

            for tool in &extension.snapshot.tools {
                if let Some(name) = tool.get("name").and_then(Value::as_str) {
                    self.tool_extension_index
                        .entry(name.to_string())
                        .or_insert(extension_index);
                }
            }

            for command in &extension.snapshot.slash_commands {
                if let Some(name) = extract_slash_command_name(command) {
                    self.command_extension_index
                        .entry(name)
                        .or_insert(extension_index);
                }
            }

            for shortcut in &extension.snapshot.shortcuts {
                if let Some(key_id) = shortcut.get("key_id").and_then(Value::as_str) {
                    self.shortcut_extension_index
                        .entry(key_id.to_string())
                        .or_insert(extension_index);
                }
            }

            for provider_id in extension.provider_streams.keys() {
                self.provider_stream_extension_index
                    .entry(provider_id.clone())
                    .or_insert(extension_index);
            }

            for hook in &extension.snapshot.event_hooks {
                self.event_hook_extension_indexes
                    .entry(hook.clone())
                    .or_default()
                    .push(extension_index);
            }
        }
    }

    fn find_tool_extension(&self, tool_name: &str) -> Option<&NativeRustLoadedExtension> {
        let extension_index = *self.tool_extension_index.get(tool_name)?;
        self.extensions.get(extension_index)
    }

    fn find_command_extension(&self, command_name: &str) -> Option<&NativeRustLoadedExtension> {
        let extension_index = *self.command_extension_index.get(command_name)?;
        self.extensions.get(extension_index)
    }

    fn find_shortcut_extension(&self, key_id: &str) -> Option<&NativeRustLoadedExtension> {
        let extension_index = *self.shortcut_extension_index.get(key_id)?;
        self.extensions.get(extension_index)
    }

    fn provider_stream_chunks(&self, provider_id: &str) -> Option<Arc<[Value]>> {
        let extension_index = *self.provider_stream_extension_index.get(provider_id)?;
        self.extensions
            .get(extension_index)?
            .provider_streams
            .get(provider_id)
            .cloned()
    }

    fn dispatch_event(
        &self,
        event_name: &str,
        event_payload: &Value,
        ctx_payload: &Value,
    ) -> Value {
        let mut response = Value::Null;
        let Some(extension_indexes) = self.event_hook_extension_indexes.get(event_name) else {
            return response;
        };

        for extension_index in extension_indexes {
            let Some(extension) = self.extensions.get(*extension_index) else {
                continue;
            };

            if let Some(explicit) = extension.event_responses.get(event_name) {
                response = explicit.clone();
                continue;
            }

            response = json!({
                "type": event_name,
                "nativeRuntime": true,
                "extensionId": extension.snapshot.id,
                "event": event_payload,
                "ctx": ctx_payload,
            });
        }
        response
    }

    fn reset_transient_state(&mut self) {
        self.streams.clear();
        self.flags.clear();
        self.repair_events.clear();
    }
}

#[derive(Clone)]
pub struct NativeRustExtensionRuntimeHandle {
    state: Arc<RwLock<NativeRustRuntimeState>>,
}

// The native runtime handle mirrors the async JS runtime handle so
// `ExtensionRuntimeHandle` can switch implementations without wrapper
// shims. Several methods complete synchronously in this in-process lane.
#[allow(clippy::unused_async)]
impl NativeRustExtensionRuntimeHandle {
    pub async fn start() -> Result<Self> {
        tracing::info!(
            event = "native_extension_runtime.mode",
            mode = "single-fast-path",
            "Native-rust extension runtime started"
        );
        Ok(Self {
            state: Arc::new(RwLock::new(NativeRustRuntimeState::default())),
        })
    }

    pub async fn shutdown(&self, _budget: Duration) -> bool {
        true
    }

    async fn load_extensions_snapshots(
        &self,
        specs: Vec<NativeRustExtensionLoadSpec>,
    ) -> Result<Vec<JsExtensionSnapshot>> {
        let loaded = load_native_extensions_from_specs(&specs)?;
        let mut state = self
            .state
            .write()
            .map_err(|_| Error::extension("native-rust runtime state lock poisoned"))?;
        Ok(state.load_extensions(loaded))
    }

    pub async fn get_registered_tools(&self) -> Result<Vec<ExtensionToolDef>> {
        let state = self
            .state
            .read()
            .map_err(|_| Error::extension("native-rust runtime state lock poisoned"))?;
        Ok(state.registered_tools.clone())
    }

    pub async fn pump_once(&self) -> Result<bool> {
        Ok(false)
    }

    pub async fn dispatch_event(
        &self,
        event_name: String,
        event_payload: Value,
        ctx_payload: Arc<Value>,
        _timeout_ms: u64,
    ) -> Result<Value> {
        let state = self
            .state
            .read()
            .map_err(|_| Error::extension("native-rust runtime state lock poisoned"))?;
        Ok(state.dispatch_event(&event_name, &event_payload, ctx_payload.as_ref()))
    }

    pub async fn dispatch_event_batch(
        &self,
        events: Vec<(String, Value)>,
        ctx_payload: Arc<Value>,
        _timeout_ms: u64,
    ) -> Result<Vec<Result<Value>>> {
        let out = {
            let state = self
                .state
                .read()
                .map_err(|_| Error::extension("native-rust runtime state lock poisoned"))?;
            let mut out = Vec::with_capacity(events.len());
            for (event_name, payload) in events {
                out.push(Ok(state.dispatch_event(
                    &event_name,
                    &payload,
                    ctx_payload.as_ref(),
                )));
            }
            out
        };
        Ok(out)
    }

    #[allow(clippy::option_if_let_else)]
    pub async fn execute_tool(
        &self,
        tool_name: String,
        tool_call_id: String,
        input: Value,
        timeout_ms: u64,
    ) -> Result<Value> {
        self.execute_tool_ref(&tool_name, &tool_call_id, input, timeout_ms)
            .await
    }

    #[allow(clippy::option_if_let_else)]
    pub async fn execute_tool_ref(
        &self,
        tool_name: &str,
        tool_call_id: &str,
        input: Value,
        _timeout_ms: u64,
    ) -> Result<Value> {
        enum Lookup {
            Output(Value),
            RegisteredWithoutOutput,
            Missing,
        }

        let lookup = {
            let state = self
                .state
                .read()
                .map_err(|_| Error::extension("native-rust runtime state lock poisoned"))?;
            if let Some(extension) = state.find_tool_extension(tool_name) {
                extension
                    .tool_outputs
                    .get(tool_name)
                    .cloned()
                    .map_or(Lookup::RegisteredWithoutOutput, Lookup::Output)
            } else {
                Lookup::Missing
            }
        };

        match lookup {
            Lookup::Output(value) => Ok(value),
            Lookup::RegisteredWithoutOutput => Ok(json!({
                "content": [
                    {
                        "type": "text",
                        "text": format!("native-rust tool `{tool_name}` executed")
                    }
                ],
                "details": {
                    "runtime": "native-rust",
                    "toolName": tool_name,
                    "toolCallId": tool_call_id,
                    "input": input
                }
            })),
            Lookup::Missing => Err(Error::extension(format!(
                "native-rust tool `{tool_name}` is not registered"
            ))),
        }
    }

    pub async fn execute_command(
        &self,
        command_name: String,
        args: String,
        _timeout_ms: u64,
    ) -> Result<Value> {
        enum Lookup {
            Output(Value),
            RegisteredWithoutOutput,
            Missing,
        }

        let lookup = {
            let state = self
                .state
                .read()
                .map_err(|_| Error::extension("native-rust runtime state lock poisoned"))?;
            state
                .find_command_extension(&command_name)
                .map_or(Lookup::Missing, |extension| {
                    extension
                        .command_outputs
                        .get(&command_name)
                        .cloned()
                        .map_or(Lookup::RegisteredWithoutOutput, Lookup::Output)
                })
        };

        match lookup {
            Lookup::Output(value) => Ok(value),
            Lookup::RegisteredWithoutOutput => Ok(json!({
                "runtime": "native-rust",
                "command": command_name,
                "args": args,
            })),
            Lookup::Missing => Err(Error::extension(format!(
                "native-rust command `{command_name}` is not registered"
            ))),
        }
    }

    pub async fn execute_shortcut(&self, key_id: String, _timeout_ms: u64) -> Result<Value> {
        enum Lookup {
            Output(Value),
            RegisteredWithoutOutput,
            Missing,
        }

        let lookup = {
            let state = self
                .state
                .read()
                .map_err(|_| Error::extension("native-rust runtime state lock poisoned"))?;
            state
                .find_shortcut_extension(&key_id)
                .map_or(Lookup::Missing, |extension| {
                    extension
                        .shortcut_outputs
                        .get(&key_id)
                        .cloned()
                        .map_or(Lookup::RegisteredWithoutOutput, Lookup::Output)
                })
        };

        match lookup {
            Lookup::Output(value) => Ok(value),
            Lookup::RegisteredWithoutOutput => Ok(json!({
                "runtime": "native-rust",
                "shortcut": key_id,
            })),
            Lookup::Missing => Err(Error::extension(format!(
                "native-rust shortcut `{key_id}` is not registered"
            ))),
        }
    }

    pub async fn set_flag_value(
        &self,
        extension_id: String,
        flag_name: String,
        value: Value,
    ) -> Result<()> {
        self.state
            .write()
            .map_err(|_| Error::extension("native-rust runtime state lock poisoned"))?
            .flags
            .insert((extension_id, flag_name), value);
        Ok(())
    }

    pub async fn drain_repair_events(&self) -> Vec<ExtensionRepairEvent> {
        let Ok(mut state) = self.state.write() else {
            return Vec::new();
        };
        let mut drained = Vec::new();
        std::mem::swap(&mut drained, &mut state.repair_events);
        drained
    }

    pub async fn reset_transient_state(&self) -> Result<()> {
        self.state
            .write()
            .map_err(|_| Error::extension("native-rust runtime state lock poisoned"))?
            .reset_transient_state();
        Ok(())
    }

    pub async fn provider_stream_simple_start(
        &self,
        provider_id: String,
        _model: Value,
        _context: Value,
        _options: Value,
        _timeout_ms: u64,
    ) -> Result<String> {
        let (stream_id, chunk_count) = {
            let mut state = self
                .state
                .write()
                .map_err(|_| Error::extension("native-rust runtime state lock poisoned"))?;
            let stream_chunks = state.provider_stream_chunks(&provider_id).ok_or_else(|| {
                Error::extension(format!(
                    "native-rust provider `{provider_id}` has no streamSimple handler"
                ))
            })?;
            let chunk_count = stream_chunks.len();
            state.next_stream_id = state.next_stream_id.saturating_add(1);
            let stream_id = format!("native-stream-{}", state.next_stream_id);
            state.streams.insert(
                stream_id.clone(),
                NativeRustProviderStreamCursor {
                    chunks: stream_chunks,
                    next_index: 0,
                },
            );
            drop(state);
            (stream_id, chunk_count)
        };
        tracing::debug!(
            event = "native_extension_runtime.provider_stream.start",
            provider_id = %provider_id,
            stream_id = %stream_id,
            chunk_count,
            "Started native-rust streamSimple stream"
        );
        Ok(stream_id)
    }

    pub async fn provider_stream_simple_next(
        &self,
        stream_id: String,
        _timeout_ms: u64,
    ) -> Result<Option<Value>> {
        let next_value = {
            let mut state = self
                .state
                .write()
                .map_err(|_| Error::extension("native-rust runtime state lock poisoned"))?;
            let Some(cursor) = state.streams.get_mut(&stream_id) else {
                return Ok(None);
            };
            let next = cursor.chunks.get(cursor.next_index).cloned();
            let exhausted = if next.is_some() {
                cursor.next_index = cursor.next_index.saturating_add(1);
                cursor.next_index >= cursor.chunks.len()
            } else {
                true
            };
            if exhausted {
                state.streams.remove(&stream_id);
            }
            next
        };
        Ok(next_value)
    }

    pub async fn provider_stream_simple_cancel(
        &self,
        stream_id: String,
        _timeout_ms: u64,
    ) -> Result<()> {
        self.state
            .write()
            .map_err(|_| Error::extension("native-rust runtime state lock poisoned"))?
            .streams
            .remove(&stream_id);
        Ok(())
    }

    #[allow(clippy::needless_pass_by_value)]
    pub fn provider_stream_simple_cancel_best_effort(&self, stream_id: String) {
        if let Ok(mut state) = self.state.write() {
            state.streams.remove(&stream_id);
        }
    }
}

fn load_native_extensions_from_specs(
    specs: &[NativeRustExtensionLoadSpec],
) -> Result<Vec<NativeRustLoadedExtension>> {
    let mut loaded = Vec::with_capacity(specs.len());
    for spec in specs {
        loaded.push(load_native_extension_from_spec(spec)?);
    }
    Ok(loaded)
}

fn load_native_extension_from_spec(
    spec: &NativeRustExtensionLoadSpec,
) -> Result<NativeRustLoadedExtension> {
    let descriptor_bytes = fs::read(&spec.entry_path).map_err(|err| {
        Error::extension(format!(
            "Failed to read native-rust extension descriptor {}: {err}",
            spec.entry_path.display()
        ))
    })?;
    let descriptor: NativeRustExtensionDescriptor = serde_json::from_slice(&descriptor_bytes)
        .map_err(|err| {
            Error::extension(format!(
                "Failed to parse native-rust extension descriptor {}: {err}",
                spec.entry_path.display()
            ))
        })?;

    let extension_id = if descriptor.id.trim().is_empty() {
        spec.extension_id.clone()
    } else {
        descriptor.id.clone()
    };
    let name = if descriptor.name.trim().is_empty() {
        spec.name.clone()
    } else {
        descriptor.name.clone()
    };
    let version = if descriptor.version.trim().is_empty() {
        spec.version.clone()
    } else {
        descriptor.version.clone()
    };
    let api_version = if descriptor.api_version.trim().is_empty() {
        spec.api_version.clone()
    } else {
        descriptor.api_version.clone()
    };
    let provider_streams = descriptor
        .provider_streams
        .into_iter()
        .map(|(provider_id, chunks)| (provider_id, Arc::<[Value]>::from(chunks)))
        .collect::<HashMap<_, _>>();

    Ok(NativeRustLoadedExtension {
        snapshot: JsExtensionSnapshot {
            id: extension_id,
            name,
            version,
            api_version,
            tools: descriptor.tools,
            slash_commands: descriptor.slash_commands,
            shortcuts: descriptor.shortcuts,
            providers: descriptor.providers,
            mcp_servers: descriptor.mcp_servers,
            flags: descriptor.flags,
            event_hooks: descriptor.event_hooks,
            active_tools: descriptor.active_tools,
        },
        event_responses: descriptor.event_responses,
        tool_outputs: descriptor.tool_outputs,
        command_outputs: descriptor.command_outputs,
        shortcut_outputs: descriptor.shortcut_outputs,
        provider_streams,
    })
}

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
    pub const fn runtime_name(&self) -> &'static str {
        match self {
            Self::Js(_) => "quickjs",
            Self::NativeRust(_) => "native-rust",
        }
    }

    pub(super) const fn compat_scan_mode(&self) -> bool {
        match self {
            Self::Js(runtime) => runtime.compat_scan_mode(),
            Self::NativeRust(_) => false,
        }
    }

    pub async fn shutdown(&self, budget: Duration) -> bool {
        match self {
            Self::Js(runtime) => runtime.shutdown(budget).await,
            Self::NativeRust(runtime) => runtime.shutdown(budget).await,
        }
    }

    pub(super) async fn load_js_extensions_snapshots(
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

    pub(super) async fn load_native_extensions_snapshots(
        &self,
        specs: Vec<NativeRustExtensionLoadSpec>,
    ) -> Result<Vec<JsExtensionSnapshot>> {
        match self {
            Self::Js(_) => Err(Error::extension(
                "QuickJS runtime does not support native-rust extension load specs".to_string(),
            )),
            Self::NativeRust(runtime) => runtime.load_extensions_snapshots(specs).await,
        }
    }

    pub async fn get_registered_tools(&self) -> Result<Vec<ExtensionToolDef>> {
        match self {
            Self::Js(runtime) => runtime.get_registered_tools().await,
            Self::NativeRust(runtime) => runtime.get_registered_tools().await,
        }
    }

    pub async fn pump_once(&self) -> Result<bool> {
        match self {
            Self::Js(runtime) => runtime.pump_once().await,
            Self::NativeRust(runtime) => runtime.pump_once().await,
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
                let _ = ctx_payload;
                runtime
                    .execute_tool_ref(tool_name, tool_call_id, input, timeout_ms)
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
                let _ = ctx_payload;
                runtime
                    .execute_command(command_name, args, timeout_ms)
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
                let _ = ctx_payload;
                runtime.execute_shortcut(key_id, timeout_ms).await
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

    pub async fn drain_repair_events(&self) -> Vec<ExtensionRepairEvent> {
        match self {
            Self::Js(runtime) => runtime.drain_repair_events().await,
            Self::NativeRust(runtime) => runtime.drain_repair_events().await,
        }
    }

    pub async fn reset_transient_state(&self) -> Result<()> {
        match self {
            Self::Js(runtime) => runtime.reset_transient_state().await,
            Self::NativeRust(runtime) => runtime.reset_transient_state().await,
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
