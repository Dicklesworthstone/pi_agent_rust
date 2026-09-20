//! On-demand MCP resource and prompt access over existing trusted transports.
//!
//! Listing returns one bounded page; cursors and resource URIs are opaque and
//! forwarded unchanged. Nothing here opens a URI as a file or network address.
//! Requests are never replayed after failure, and a superseded or revoked
//! transport cannot return context into a replacement session.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::super::{Arc, McpManager, McpTransport, ServerEntry, tool_err};
use super::{CatalogByteBudget, DEFAULT_MCP_TIMEOUT, Instant, MAX_CURSOR_BYTES};
use crate::error::{Error, Result};

const MAX_CONTEXT_ITEMS: usize = 1024;
const MAX_CONTEXT_NAME_BYTES: usize = 1024;
const MAX_RESOURCE_URI_BYTES: usize = 16 * 1024;
const MAX_CONTEXT_PAGE_BYTES: usize = 2 * 1024 * 1024;
const MAX_CONTEXT_CONTENT_BYTES: usize = 10 * 1024 * 1024;
const MAX_PROMPT_ARGUMENTS: usize = 128;
const MAX_PROMPT_ARGUMENT_BYTES: usize = 64 * 1024;
// MCP 2025-06-18 CompleteResult defines at most 100 suggestions per response.
const MAX_COMPLETION_VALUES: usize = 100;
const MAX_COMPLETION_VALUE_BYTES: usize = 16 * 1024;
const MAX_COMPLETION_REQUEST_BYTES: usize = 128 * 1024;
// Each message becomes a role label plus a native content block. Leave room
// within the shared content shaper's block budget for the reference heading.
const MAX_PROMPT_MESSAGES: usize = 256;

#[derive(Clone, Copy)]
enum ContextResult {
    Resources,
    ResourceTemplates,
    ResourceContents,
    Prompts,
    PromptMessages,
    Completion,
}

fn invalid_request(reason: &str) -> Error {
    tool_err("MCP_REQUEST_INVALID", reason)
}

fn invalid_response(reason: &str) -> Error {
    tool_err("MCP_PROTOCOL", reason)
}

fn checked_identifier(value: &str, limit: usize, field: &str) -> Result<()> {
    if value.is_empty() || value.len() > limit || value.chars().any(char::is_control) {
        return Err(invalid_request(&format!(
            "{field} must be nonempty, contain no control characters, and fit within {limit} bytes"
        )));
    }
    Ok(())
}

fn page_params(cursor: Option<&str>) -> Result<Value> {
    match cursor {
        Some(cursor) if cursor.len() > MAX_CURSOR_BYTES => Err(invalid_request(
            "MCP pagination cursor exceeds the byte limit",
        )),
        Some(cursor) => Ok(json!({"cursor": cursor})),
        None => Ok(json!({})),
    }
}

fn server_error(error: &Error) -> bool {
    matches!(error, Error::Tool { tool, message }
        if tool == "mcp" && message.starts_with("[MCP_SERVER_ERROR]"))
}

/// The transport's error taxonomy, not server-controlled prose, identifies
/// MethodNotFound. Never classify an arbitrary message containing -32601.
pub(super) fn is_method_not_found(error: &Error) -> bool {
    matches!(error, Error::Tool { tool, message }
        if tool == "mcp"
            && message.starts_with("[MCP_SERVER_ERROR] server error -32601:"))
}

fn response_string<'a>(value: &'a Value, key: &str, limit: usize) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty() && text.len() <= limit)
        .ok_or_else(|| invalid_response(&format!("MCP context field {key} is missing or invalid")))
}

fn optional_strings(value: &Value, keys: &[&str]) -> Result<()> {
    for key in keys {
        if value.get(key).is_some_and(|value| !value.is_string()) {
            return Err(invalid_response(&format!(
                "MCP context field {key} must be a string"
            )));
        }
    }
    Ok(())
}

fn validate_prompt_definition(prompt: &Value) -> Result<()> {
    response_string(prompt, "name", MAX_CONTEXT_NAME_BYTES)?;
    let Some(arguments) = prompt.get("arguments") else {
        return Ok(());
    };
    let arguments = arguments
        .as_array()
        .filter(|arguments| arguments.len() <= MAX_PROMPT_ARGUMENTS)
        .ok_or_else(|| invalid_response("MCP prompt arguments must be a bounded array"))?;
    let mut names = std::collections::HashSet::new();
    for argument in arguments {
        let name = response_string(argument, "name", MAX_CONTEXT_NAME_BYTES)?;
        if !names.insert(name) {
            return Err(invalid_response("MCP prompt has duplicate argument names"));
        }
        optional_strings(argument, &["title", "description"])?;
        if argument
            .get("required")
            .is_some_and(|value| !value.is_boolean())
        {
            return Err(invalid_response(
                "MCP prompt argument required must be a boolean",
            ));
        }
    }
    Ok(())
}

fn validate_arguments(arguments: &BTreeMap<String, String>) -> Result<()> {
    if arguments.len() > MAX_PROMPT_ARGUMENTS {
        return Err(invalid_request("MCP context has too many arguments"));
    }
    for key in arguments.keys() {
        checked_identifier(key, MAX_CONTEXT_NAME_BYTES, "argument name")?;
    }
    let mut budget = CatalogByteBudget {
        remaining: MAX_PROMPT_ARGUMENT_BYTES,
    };
    serde_json::to_writer(&mut budget, arguments)
        .map_err(|_| invalid_request("MCP context arguments exceed the byte limit"))
}

fn validate_completion(result: &Value) -> Result<()> {
    let completion = result
        .get("completion")
        .filter(|value| value.is_object())
        .ok_or_else(|| {
            invalid_response("MCP completion result must contain a completion object")
        })?;
    let values = completion
        .get("values")
        .and_then(Value::as_array)
        .filter(|values| values.len() <= MAX_COMPLETION_VALUES)
        .ok_or_else(|| {
            invalid_response("MCP completion values must be an array of at most 100 strings")
        })?;
    if values.iter().any(|value| {
        value
            .as_str()
            .is_none_or(|value| value.len() > MAX_COMPLETION_VALUE_BYTES)
    }) {
        return Err(invalid_response(
            "MCP completion suggestions must be bounded strings",
        ));
    }
    if let Some(total) = completion.get("total") {
        let total = total
            .as_u64()
            .ok_or_else(|| invalid_response("MCP completion total must be an unsigned integer"))?;
        if total < u64::try_from(values.len()).unwrap_or(u64::MAX) {
            return Err(invalid_response(
                "MCP completion total is smaller than its returned values",
            ));
        }
    }
    if completion
        .get("hasMore")
        .is_some_and(|value| !value.is_boolean())
    {
        return Err(invalid_response("MCP completion hasMore must be a boolean"));
    }
    Ok(())
}

fn validate_response(result: &Value, shape: ContextResult) -> Result<()> {
    if !result.is_object() {
        return Err(invalid_response("MCP context result must be an object"));
    }
    let (field, limit) = match shape {
        ContextResult::Resources => ("resources", MAX_CONTEXT_PAGE_BYTES),
        ContextResult::ResourceTemplates => ("resourceTemplates", MAX_CONTEXT_PAGE_BYTES),
        ContextResult::ResourceContents => ("contents", MAX_CONTEXT_CONTENT_BYTES),
        ContextResult::Prompts => ("prompts", MAX_CONTEXT_PAGE_BYTES),
        ContextResult::PromptMessages => ("messages", MAX_CONTEXT_CONTENT_BYTES),
        ContextResult::Completion => ("completion", MAX_CONTEXT_PAGE_BYTES),
    };
    let mut budget = CatalogByteBudget { remaining: limit };
    serde_json::to_writer(&mut budget, result)
        .map_err(|_| invalid_response("MCP context response exceeds the byte limit"))?;
    if matches!(shape, ContextResult::Completion) {
        return validate_completion(result);
    }
    let item_limit = if matches!(shape, ContextResult::PromptMessages) {
        MAX_PROMPT_MESSAGES
    } else {
        MAX_CONTEXT_ITEMS
    };
    let items = result
        .get(field)
        .and_then(Value::as_array)
        .filter(|items| items.len() <= item_limit)
        .ok_or_else(|| {
            invalid_response("MCP context result has a missing or oversized item array")
        })?;
    optional_strings(result, &["description"])?;
    if matches!(
        shape,
        ContextResult::Resources | ContextResult::ResourceTemplates | ContextResult::Prompts
    ) && let Some(cursor) = result.get("nextCursor")
        && cursor
            .as_str()
            .is_none_or(|cursor| cursor.len() > MAX_CURSOR_BYTES)
    {
        return Err(invalid_response(
            "MCP context nextCursor must be a bounded string",
        ));
    }
    for item in items {
        optional_strings(item, &["title", "description", "mimeType"])?;
        match shape {
            ContextResult::Resources | ContextResult::ResourceTemplates => {
                response_string(item, "name", MAX_CONTEXT_NAME_BYTES)?;
                let uri_field = if matches!(shape, ContextResult::Resources) {
                    "uri"
                } else {
                    "uriTemplate"
                };
                response_string(item, uri_field, MAX_RESOURCE_URI_BYTES)?;
                if item.get("size").is_some_and(|size| size.as_u64().is_none()) {
                    return Err(invalid_response(
                        "MCP resource size must be an unsigned integer",
                    ));
                }
            }
            ContextResult::ResourceContents => {
                response_string(item, "uri", MAX_RESOURCE_URI_BYTES)?;
                match (item.get("text"), item.get("blob")) {
                    (Some(Value::String(_)), None) | (None, Some(Value::String(_))) => {}
                    _ => {
                        return Err(invalid_response(
                            "MCP resource content must contain exactly one string text or blob",
                        ));
                    }
                }
            }
            ContextResult::Prompts => validate_prompt_definition(item)?,
            ContextResult::PromptMessages => {
                if !matches!(
                    item.get("role").and_then(Value::as_str),
                    Some("user" | "assistant")
                ) {
                    return Err(invalid_response(
                        "MCP prompt roles must be user or assistant",
                    ));
                }
                let content = item
                    .get("content")
                    .filter(|content| content.is_object())
                    .ok_or_else(|| {
                        invalid_response("MCP prompt message must contain a content object")
                    })?;
                response_string(content, "type", MAX_CONTEXT_NAME_BYTES)?;
            }
            ContextResult::Completion => {
                return Err(invalid_response(
                    "MCP completion was not validated as an object",
                ));
            }
        }
    }
    Ok(())
}

/// Dropping a request must not leave its connection usable while delivery is
/// uncertain. The manager's generation check protects any replacement from
/// this old request's cancellation. No async cleanup task is detached.
struct ContextRequestGuard {
    entry: Arc<ServerEntry>,
    transport: Arc<dyn McpTransport>,
    armed: bool,
}

impl Drop for ContextRequestGuard {
    fn drop(&mut self) {
        if self.armed {
            McpManager::fail_transport_generation(
                &self.entry,
                &self.transport,
                &tool_err(
                    "MCP_CANCELLED",
                    "MCP context request was cancelled; it was not replayed",
                ),
            );
        }
    }
}

impl McpManager {
    /// List one page of a trusted server's resources. Pass the returned
    /// `nextCursor` unchanged for the next page, including an empty cursor.
    ///
    /// # Errors
    /// Returns trust, connection, server, or response-validation errors.
    pub async fn list_resources(&self, server: &str, cursor: Option<&str>) -> Result<Value> {
        self.request_context(
            server,
            "resources/list",
            page_params(cursor)?,
            ContextResult::Resources,
        )
        .await
    }

    /// List URI templates without expanding them or reading their resources.
    ///
    /// # Errors
    /// Returns trust, connection, server, or response-validation errors.
    pub async fn list_resource_templates(
        &self,
        server: &str,
        cursor: Option<&str>,
    ) -> Result<Value> {
        self.request_context(
            server,
            "resources/templates/list",
            page_params(cursor)?,
            ContextResult::ResourceTemplates,
        )
        .await
    }

    /// Read an opaque URI through a trusted MCP server, never through local I/O.
    ///
    /// # Errors
    /// Returns invalid-input, trust, connection, server, or content errors.
    pub async fn read_resource(&self, server: &str, uri: &str) -> Result<Value> {
        checked_identifier(uri, MAX_RESOURCE_URI_BYTES, "resource URI")?;
        self.request_context(
            server,
            "resources/read",
            json!({"uri": uri}),
            ContextResult::ResourceContents,
        )
        .await
    }

    /// List a single page of server-provided prompt templates and their
    /// argument declarations. This does not retrieve or execute a prompt.
    ///
    /// # Errors
    /// Returns trust, connection, server, or response-validation errors.
    pub async fn list_prompts(&self, server: &str, cursor: Option<&str>) -> Result<Value> {
        self.request_context(
            server,
            "prompts/list",
            page_params(cursor)?,
            ContextResult::Prompts,
        )
        .await
    }

    /// Retrieve a named prompt with optional string arguments. The result is
    /// reference material: callers must not elevate its roles to system
    /// instructions or silently execute its requested actions.
    ///
    /// # Errors
    /// Returns invalid-input, trust, connection, server, or content errors.
    pub async fn get_prompt(
        &self,
        server: &str,
        name: &str,
        arguments: Option<&BTreeMap<String, String>>,
    ) -> Result<Value> {
        checked_identifier(name, MAX_CONTEXT_NAME_BYTES, "prompt name")?;
        let mut params = json!({"name": name});
        if let Some(arguments) = arguments {
            validate_arguments(arguments)?;
            params["arguments"] = serde_json::to_value(arguments)?;
        }
        self.request_context(server, "prompts/get", params, ContextResult::PromptMessages)
            .await
    }

    /// Ask the trusted server for argument suggestions for a prompt or URI
    /// template. Empty prefixes and already-resolved values are preserved.
    /// This performs one on-demand request; it never polls or fetches the
    /// referenced resource, retrieves the prompt, or accepts a suggestion.
    ///
    /// # Errors
    /// Returns invalid-input, trust, connection, server, or response errors.
    pub async fn complete_argument(
        &self,
        server: &str,
        reference: &crate::mcp::McpCompletionReference,
        argument_name: &str,
        argument_value: &str,
        context: Option<&BTreeMap<String, String>>,
    ) -> Result<Value> {
        match reference {
            crate::mcp::McpCompletionReference::Prompt { name } => {
                checked_identifier(name, MAX_CONTEXT_NAME_BYTES, "prompt name")?;
            }
            crate::mcp::McpCompletionReference::Resource { uri } => {
                checked_identifier(uri, MAX_RESOURCE_URI_BYTES, "resource URI template")?;
            }
        }
        checked_identifier(
            argument_name,
            MAX_CONTEXT_NAME_BYTES,
            "completion argument name",
        )?;
        if argument_value.len() > MAX_PROMPT_ARGUMENT_BYTES {
            return Err(invalid_request(
                "MCP completion argument exceeds the byte limit",
            ));
        }
        let mut params = json!({
            "ref": reference,
            "argument": {"name": argument_name, "value": argument_value},
        });
        if let Some(context) = context {
            validate_arguments(context)?;
            params["context"] = json!({"arguments": context});
        }
        // Prefixes may contain JSON-escaped control characters. Bound the
        // complete encoded request, not only its unescaped component lengths,
        // before acquiring a connection or allowing a server-side effect.
        let mut budget = CatalogByteBudget {
            remaining: MAX_COMPLETION_REQUEST_BYTES,
        };
        serde_json::to_writer(&mut budget, &params).map_err(|_| {
            invalid_request("MCP completion request exceeds the encoded byte limit")
        })?;
        self.request_context(
            server,
            "completion/complete",
            params,
            ContextResult::Completion,
        )
        .await
    }

    /// Eligible context tools are derived from configuration and current trust,
    /// not tools/list: resource-only servers have no ordinary tool schemas.
    pub(crate) fn context_server_names(&self, only: Option<&str>) -> Vec<String> {
        if self.check_running().is_err() {
            return Vec::new();
        }
        let entries: Vec<_> = Self::lock(&self.inner.servers).values().cloned().collect();
        let mut names: Vec<_> = entries
            .iter()
            .filter(|entry| only.is_none_or(|name| name == entry.config.name))
            .filter(|entry| self.check_trust(entry).is_ok())
            .map(|entry| entry.config.name.clone())
            .collect();
        names.sort();
        names
    }

    async fn check_context_transport(
        &self,
        entry: &Arc<ServerEntry>,
        transport: &Arc<dyn McpTransport>,
    ) -> Result<()> {
        self.check_running()?;
        if let Err(error) = self.check_trust(entry) {
            Self::close_revoked_transport(entry, transport).await;
            return Err(error);
        }
        let current = Self::lock(&entry.transport)
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, transport));
        if !current {
            transport.abort();
            return Err(tool_err(
                "MCP_TRANSPORT_SUPERSEDED",
                "connection changed during MCP context access",
            ));
        }
        Ok(())
    }

    async fn request_context(
        &self,
        server: &str,
        method: &'static str,
        params: Value,
        shape: ContextResult,
    ) -> Result<Value> {
        let owner = crate::agent_cx::AgentCx::for_current_or_request();
        super::check_request_owner(&owner)?;
        owner
            .with_current(self.request_context_with_timeout(
                server,
                method,
                params,
                shape,
                DEFAULT_MCP_TIMEOUT,
            ))
            .await
    }

    async fn request_context_with_timeout(
        &self,
        server: &str,
        method: &'static str,
        params: Value,
        shape: ContextResult,
        timeout: std::time::Duration,
    ) -> Result<Value> {
        let owner = crate::agent_cx::AgentCx::for_current_or_request();
        // Check before connection setup or any secret/process resolution.
        super::check_request_owner(&owner)?;
        let entry = self.entry(server)?;
        self.ensure_ready(&entry).await?;
        let transport = Self::lock(&entry.transport).clone().ok_or_else(|| {
            tool_err(
                "MCP_TRANSPORT_CLOSED",
                "connection disappeared before MCP context access",
            )
        })?;
        self.check_context_transport(&entry, &transport).await?;
        let mut guard = ContextRequestGuard {
            entry: Arc::clone(&entry),
            transport: Arc::clone(&transport),
            armed: true,
        };
        let started = Instant::now();
        let result = super::request_with_owner(&owner, &transport, method, params, timeout).await;
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                if server_error(&error) {
                    self.check_context_transport(&entry, &transport).await?;
                } else {
                    // Preserve the original cancellation/timeout taxonomy,
                    // even if dropping the request already closed its socket.
                    Self::fail_transport_generation(&entry, &transport, &error);
                }
                guard.armed = false;
                return Err(error);
            }
        };
        // Cleanup ownership lasts through validation and final publication,
        // not just until the transport has produced a value.
        self.check_context_transport(&entry, &transport).await?;
        if let Err(error) = validate_response(&result, shape) {
            Self::fail_transport_generation(&entry, &transport, &error);
            guard.armed = false;
            return Err(error);
        }
        if started.elapsed() >= timeout {
            let error = tool_err(
                "MCP_TIMEOUT",
                "MCP context response arrived after its deadline",
            );
            Self::fail_transport_generation(&entry, &transport, &error);
            guard.armed = false;
            return Err(error);
        }
        self.check_context_transport(&entry, &transport).await?;
        if let Err(error) = super::check_request_owner(&owner) {
            Self::fail_transport_generation(&entry, &transport, &error);
            guard.armed = false;
            return Err(error);
        }
        if !Self::record_operational_success(&entry, &transport) {
            return Err(tool_err(
                "MCP_TRANSPORT_SUPERSEDED",
                "connection changed before MCP context publication",
            ));
        }
        guard.armed = false;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use async_trait::async_trait;

    use super::super::super::{ConfiguredServer, McpDiscovery, Provenance, TrustStore};
    use super::*;

    struct OwnerBlockedContextTransport {
        requests: std::sync::atomic::AtomicUsize,
        closed: AtomicBool,
    }

    impl OwnerBlockedContextTransport {
        fn new() -> Self {
            Self {
                requests: std::sync::atomic::AtomicUsize::new(0),
                closed: AtomicBool::new(false),
            }
        }
    }

    #[async_trait]
    impl McpTransport for OwnerBlockedContextTransport {
        async fn request(
            &self,
            method: &str,
            _params: Value,
            _timeout: std::time::Duration,
        ) -> Result<Value> {
            assert_eq!(method, "resources/list", "no reconnect or replay");
            self.requests.fetch_add(1, Ordering::SeqCst);
            let budget = asupersync::Cx::current().expect("request owner").budget();
            std::future::poll_fn(|_| {
                assert_eq!(
                    asupersync::Cx::current().expect("owner on every poll").budget(),
                    budget
                );
                std::task::Poll::<Result<Value>>::Pending
            })
            .await
        }

        async fn notify(&self, _method: &str, _params: Value) -> Result<()> {
            Ok(())
        }

        fn is_alive(&self) -> bool {
            !self.closed.load(Ordering::Acquire)
        }

        fn abort(&self) {
            self.closed.store(true, Ordering::Release);
        }

        async fn close(&self) {
            self.abort();
        }

        fn diagnostics_tail(&self) -> String {
            String::new()
        }
    }

    #[test]
    fn owner_cancellation_ends_idle_context_access_without_transport_cooperation() {
        use std::future::Future as _;
        use std::task::{Context, Poll};

        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, _) = fixture(&temp, vec![], true);
        let transport = Arc::new(OwnerBlockedContextTransport::new());
        let erased: Arc<dyn McpTransport> = transport.clone();
        *McpManager::lock(&entry.transport) = Some(erased);
        let runtime = runtime();
        let owner = crate::agent_cx::AgentCx::from_cx(runtime.request_cx_with_budget(
            asupersync::Budget::new().with_poll_quota(1000),
        ));
        runtime.block_on(async {
            let parent = asupersync::Cx::current().expect("parent");
            let mut task = Context::from_waker(futures::task::noop_waker_ref());
            let mut request = Box::pin(manager.list_resources("docs", None));
            {
                let _guard = owner.cx().clone().set_current_restricted();
                assert!(request.as_mut().poll(&mut task).is_pending());
            }
            assert!(request.as_mut().poll(&mut task).is_pending());
            owner.cancel_with(asupersync::types::CancelKind::User, Some("cancel context"));
            let Poll::Ready(Err(error)) = request.as_mut().poll(&mut task) else {
                panic!("context cancellation must complete without a network response");
            };
            assert!(error.to_string().contains("MCP_CANCELLED"));
            assert!(!parent.is_cancel_requested());
            assert_eq!(asupersync::Cx::current().unwrap().budget(), parent.budget());
        });
        assert_eq!(transport.requests.load(Ordering::SeqCst), 1);
        assert!(transport.closed.load(Ordering::Acquire));
        assert!(McpManager::lock(&entry.transport).is_none());
        assert_eq!(McpManager::lock(&entry.restarts).count, 1);
    }

    #[test]
    fn context_manager_deadline_terminates_an_uncooperative_transport() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, _) = fixture(&temp, vec![], true);
        let transport = Arc::new(OwnerBlockedContextTransport::new());
        let erased: Arc<dyn McpTransport> = transport.clone();
        *McpManager::lock(&entry.transport) = Some(erased);
        let error = runtime()
            .block_on(manager.request_context_with_timeout(
                "docs",
                "resources/list",
                json!({}),
                ContextResult::Resources,
                std::time::Duration::from_millis(20),
            ))
            .expect_err("manager timeout");
        assert!(error.to_string().contains("MCP_TIMEOUT"));
        assert_eq!(transport.requests.load(Ordering::SeqCst), 1);
        assert!(transport.closed.load(Ordering::Acquire));
        assert!(McpManager::lock(&entry.transport).is_none());
        assert_eq!(McpManager::lock(&entry.restarts).count, 1);
    }

    #[test]
    fn pre_cancelled_context_owner_has_no_connection_effects() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, transport) = fixture(&temp, vec![], true);
        let owner = crate::agent_cx::AgentCx::for_request();
        owner.cancel_with(asupersync::types::CancelKind::User, Some("before context access"));
        let error = runtime()
            .block_on(owner.with_current(manager.list_resources("docs", None)))
            .expect_err("owner cancelled before dispatch");
        assert!(error.to_string().contains("MCP_CANCELLED"));
        assert!(McpManager::lock(&transport.requests).is_empty());
        assert!(!transport.closed.load(Ordering::Acquire));
        assert_eq!(McpManager::lock(&entry.restarts).count, 0);
    }

    #[test]
    fn simultaneous_context_reply_and_owner_cancellation_does_not_publish_context() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, transport) = fixture(
            &temp,
            vec![Ok(json!({"resources": [{"name": "private", "uri": "db://private"}]}))],
            true,
        );
        let runtime = runtime();
        let owner = crate::agent_cx::AgentCx::from_cx(
            runtime.request_cx_with_budget(asupersync::Budget::new()),
        );
        let owner_for_hook = owner.clone();
        *McpManager::lock(&transport.after_response) = Some(Arc::new(move || {
            owner_for_hook.cancel_with(asupersync::types::CancelKind::User, Some("reply race"));
        }));
        let error = runtime
            .block_on(owner.with_current(manager.list_resources("docs", None)))
            .expect_err("cancelled response cannot publish");
        assert!(error.to_string().contains("MCP_CANCELLED"));
        assert!(!error.to_string().contains("db://private"));
        assert!(transport.closed.load(Ordering::Acquire));
        assert!(McpManager::lock(&entry.transport).is_none());
        assert_eq!(McpManager::lock(&transport.requests).len(), 1);
        assert_eq!(McpManager::lock(&entry.restarts).count, 1);
    }

    type ResponseHook = dyn Fn() + Send + Sync;

    struct ContextTransport {
        replies: Mutex<VecDeque<Result<Value>>>,
        requests: Mutex<Vec<(String, Value, std::time::Duration)>>,
        closed: AtomicBool,
        after_response: Mutex<Option<Arc<ResponseHook>>>,
    }

    #[async_trait]
    impl McpTransport for ContextTransport {
        async fn request(
            &self,
            method: &str,
            params: Value,
            timeout: std::time::Duration,
        ) -> Result<Value> {
            McpManager::lock(&self.requests).push((method.to_string(), params, timeout));
            let reply = McpManager::lock(&self.replies)
                .pop_front()
                .expect("unexpected extra request or replay");
            let hook = McpManager::lock(&self.after_response).clone();
            if let Some(hook) = hook {
                hook();
            }
            reply
        }

        async fn notify(&self, _method: &str, _params: Value) -> Result<()> {
            Ok(())
        }
        fn is_alive(&self) -> bool {
            !self.closed.load(Ordering::Acquire)
        }
        fn abort(&self) {
            self.closed.store(true, Ordering::Release);
        }
        async fn close(&self) {
            self.abort();
        }
        fn diagnostics_tail(&self) -> String {
            String::new()
        }
    }

    fn fixture(
        temp: &tempfile::TempDir,
        replies: Vec<Result<Value>>,
        trusted: bool,
    ) -> (Arc<McpManager>, Arc<ServerEntry>, Arc<ContextTransport>) {
        let config = ConfiguredServer {
            name: "docs".to_string(),
            command: None,
            args: Vec::new(),
            env: Vec::new(),
            url: Some("https://context.invalid/mcp".to_string()),
            headers: Vec::new(),
            transport_hint: Some("http".to_string()),
            provenance: Provenance::ProjectPi,
            source_file: temp.path().join("mcp.json"),
        };
        let manager = Arc::new(McpManager::new(
            temp.path(),
            temp.path(),
            McpDiscovery {
                servers: vec![config],
                warnings: Vec::new(),
            },
        ));
        let entry = manager.entry("docs").expect("server");
        if trusted {
            TrustStore::load(&manager.inner.trust_path)
                .expect("trust store")
                .acknowledge("docs", &manager.trust_fingerprint_for(&entry), "operator")
                .expect("trust server");
        }
        let transport = Arc::new(ContextTransport {
            replies: Mutex::new(replies.into()),
            requests: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
            after_response: Mutex::new(None),
        });
        let erased: Arc<dyn McpTransport> = transport.clone();
        *McpManager::lock(&entry.transport) = Some(erased);
        (manager, entry, transport)
    }

    fn runtime() -> asupersync::runtime::Runtime {
        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
    }

    fn rendered(output: &crate::tools::ToolOutput) -> String {
        output
            .content
            .iter()
            .filter_map(|block| match block {
                crate::model::ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn completion_preserves_prefix_context_and_server_relevance_order() {
        let temp = tempfile::tempdir().expect("tempdir");
        let result = json!({"completion": {
            "values": ["z-last-alphabetically", "a-first-alphabetically", "", "Ω"],
            "total": 9, "hasMore": true, "_meta": {"secret": "private-inner"}
        }, "_meta": {"secret": "private-outer"}});
        let (manager, _, transport) = fixture(&temp, vec![Ok(result.clone())], true);
        let tools = crate::mcp::mount_tools(&manager);
        let output = runtime()
            .block_on(tools[0].execute(
                "complete",
                json!({
                    "action": "complete_argument",
                    "reference": {"type": "ref/prompt", "name": "code_review"},
                    "argument": {"name": "framework", "value": "  fl\n"},
                    "context": {"language": "日本語", "source": "line 1\n\"line 2\""}
                }),
                None,
            ))
            .expect("completion");
        assert!(!output.is_error);
        let public: Value = serde_json::from_str(&rendered(&output)).expect("public JSON");
        assert_eq!(
            public["completion"]["values"],
            result["completion"]["values"]
        );
        assert_eq!(public["completion"]["total"], 9);
        assert_eq!(public["completion"]["hasMore"], true);
        assert!(!rendered(&output).contains("private-"));
        assert_eq!(output.details, Some(result));
        let requests = McpManager::lock(&transport.requests);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].0, "completion/complete");
        assert_eq!(
            requests[0].1,
            json!({
                "ref": {"type": "ref/prompt", "name": "code_review"},
                "argument": {"name": "framework", "value": "  fl\n"},
                "context": {"arguments": {"language": "日本語", "source": "line 1\n\"line 2\""}}
            })
        );
    }

    #[test]
    fn resource_completions_keep_templates_opaque_and_do_not_fetch_them() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, _, transport) = fixture(
            &temp,
            vec![
                Ok(json!({"completion": {"values": []}})),
                Ok(json!({"completion": {"values": [""], "hasMore": false}})),
            ],
            true,
        );
        let reference = crate::mcp::McpCompletionReference::Resource {
            uri: "file:///{+path}{?revision}".to_string(),
        };
        runtime().block_on(async {
            manager
                .complete_argument("docs", &reference, "path", "", None)
                .await
                .expect("empty prefix");
            manager
                .complete_argument("docs", &reference, "path", "", Some(&BTreeMap::new()))
                .await
                .expect("explicit empty context");
        });
        let requests = McpManager::lock(&transport.requests);
        assert_eq!(requests.len(), 2);
        assert!(
            requests
                .iter()
                .all(|request| request.0 == "completion/complete")
        );
        assert_eq!(requests[0].1["ref"]["uri"], "file:///{+path}{?revision}");
        assert_eq!(requests[0].1["argument"]["value"], "");
        assert!(requests[0].1.get("context").is_none());
        assert_eq!(requests[1].1["context"], json!({"arguments": {}}));
        assert!(!transport.closed.load(Ordering::Acquire));
    }

    #[test]
    fn completion_input_errors_have_no_transport_effects() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, _, transport) = fixture(&temp, Vec::new(), true);
        let tools = crate::mcp::mount_tools(&manager);
        runtime().block_on(async {
            for reference in [
                json!({"type": "ref/tool", "name": "exec"}),
                json!({"type": "ref/prompt", "name": "", "uri": "db://ignored"}),
                json!({"type": "ref/prompt", "name": ""}),
                json!({"type": "ref/resource", "uri": "db://secret\nother"}),
                json!({"type": "ref/resource", "uri": 9}),
            ] {
                assert!(
                    tools[0]
                        .execute(
                            "bad",
                            json!({
                                "action": "complete_argument", "reference": reference,
                                "argument": {"name": "arg", "value": ""}
                            }),
                            None
                        )
                        .await
                        .is_err()
                );
            }
            for argument in [
                json!({"name": "arg"}),
                json!({"name": "arg", "value": 1}),
                json!({"name": "", "value": ""}),
                json!({"name": "arg", "value": "", "secret": true}),
                json!({"name": "arg", "value": "x".repeat(MAX_PROMPT_ARGUMENT_BYTES + 1)}),
            ] {
                assert!(
                    tools[0]
                        .execute(
                            "bad",
                            json!({
                                "action": "complete_argument",
                                "reference": {"type": "ref/prompt", "name": "review"},
                                "argument": argument
                            }),
                            None
                        )
                        .await
                        .is_err()
                );
            }
            assert!(
                tools[0]
                    .execute(
                        "bad",
                        json!({
                            "action": "complete_argument",
                            "reference": {"type": "ref/prompt", "name": "review"},
                            "argument": {"name": "arg", "value": ""},
                            "context": {"arg": 1}
                        }),
                        None
                    )
                    .await
                    .is_err()
            );
        });
        assert!(McpManager::lock(&transport.requests).is_empty());
        assert!(!transport.closed.load(Ordering::Acquire));
    }

    #[test]
    fn completion_context_is_bounded_before_connection_or_dispatch() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, _, transport) = fixture(&temp, Vec::new(), true);
        let reference = crate::mcp::McpCompletionReference::Prompt {
            name: "review".to_string(),
        };
        let too_many: BTreeMap<String, String> = (0..=MAX_PROMPT_ARGUMENTS)
            .map(|index| (format!("arg{index}"), String::new()))
            .collect();
        let too_large =
            BTreeMap::from([("source".to_string(), "\n".repeat(MAX_PROMPT_ARGUMENT_BYTES))]);
        let bad_name = BTreeMap::from([("\0name".to_string(), String::new())]);
        runtime().block_on(async {
            for arguments in [&too_many, &too_large, &bad_name] {
                let error = manager
                    .complete_argument("docs", &reference, "arg", "", Some(arguments))
                    .await
                    .expect_err("reject invalid context");
                assert!(error.to_string().contains("MCP_REQUEST_INVALID"));
            }
        });
        assert!(McpManager::lock(&transport.requests).is_empty());
    }

    #[test]
    fn completion_request_budget_counts_prefix_escaping_before_dispatch() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, _, transport) =
            fixture(&temp, vec![Ok(json!({"completion": {"values": []}}))], true);
        let reference = crate::mcp::McpCompletionReference::Prompt {
            name: "review".to_string(),
        };
        let prefix = "\0".repeat(MAX_PROMPT_ARGUMENT_BYTES / 2);
        let error = runtime()
            .block_on(manager.complete_argument("docs", &reference, "code", &prefix, None))
            .expect_err("escaping exceeds the aggregate request budget");
        assert!(error.to_string().contains("encoded byte limit"));
        assert!(McpManager::lock(&transport.requests).is_empty());
        assert!(!transport.closed.load(Ordering::Acquire));

        let prefix = "x".repeat(MAX_PROMPT_ARGUMENT_BYTES);
        runtime()
            .block_on(manager.complete_argument("docs", &reference, "code", &prefix, None))
            .expect("unescaped prefix at its exact bound remains supported");
        let requests = McpManager::lock(&transport.requests);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].1["argument"]["value"], prefix);
    }

    #[test]
    fn completion_response_limits_and_types_are_enforced() {
        for completion in [
            json!(null),
            json!({}),
            json!({"values": null}),
            json!({"values": [7]}),
            json!({"values": vec!["x"; MAX_COMPLETION_VALUES + 1]}),
            json!({"values": ["x".repeat(MAX_COMPLETION_VALUE_BYTES + 1)]}),
            json!({"values": ["x"], "total": 0}),
            json!({"values": [], "total": -1}),
            json!({"values": [], "total": 1.5}),
            json!({"values": [], "total": "100"}),
            json!({"values": [], "hasMore": "yes"}),
        ] {
            assert!(
                validate_response(
                    &json!({"completion": completion}),
                    ContextResult::Completion
                )
                .is_err()
            );
        }
        for completion in [
            json!({"values": []}),
            json!({"values": [""], "total": 1, "hasMore": false}),
            json!({"values": vec!["x"; MAX_COMPLETION_VALUES], "hasMore": true}),
            json!({"values": ["x".repeat(MAX_COMPLETION_VALUE_BYTES)]}),
        ] {
            validate_response(
                &json!({"completion": completion}),
                ContextResult::Completion,
            )
            .expect("valid bounded completion");
        }
        assert!(
            validate_response(
                &json!({"completion": {"values": []},
            "_meta": "x".repeat(MAX_CONTEXT_PAGE_BYTES)}),
                ContextResult::Completion
            )
            .is_err()
        );
    }

    #[test]
    fn malformed_completion_retires_the_connection_without_retrying() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, transport) = fixture(
            &temp,
            vec![Ok(json!({"completion": {"values": [null]}}))],
            true,
        );
        let reference = crate::mcp::McpCompletionReference::Prompt {
            name: "review".to_string(),
        };
        let error = runtime()
            .block_on(manager.complete_argument("docs", &reference, "arg", "", None))
            .expect_err("invalid response");
        assert!(error.to_string().contains("MCP_PROTOCOL"));
        assert_eq!(McpManager::lock(&transport.requests).len(), 1);
        assert!(transport.closed.load(Ordering::Acquire));
        assert!(McpManager::lock(&entry.transport).is_none());
        assert_eq!(McpManager::lock(&entry.restarts).count, 1);
    }

    #[test]
    fn unsupported_completion_does_not_disable_other_server_features() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, transport) = fixture(
            &temp,
            vec![
                Err(tool_err(
                    "MCP_SERVER_ERROR",
                    "server error -32601: no completions",
                )),
                Ok(json!({"resources": []})),
            ],
            true,
        );
        let reference = crate::mcp::McpCompletionReference::Prompt {
            name: "review".to_string(),
        };
        runtime().block_on(async {
            assert!(
                manager
                    .complete_argument("docs", &reference, "arg", "", None)
                    .await
                    .is_err()
            );
            manager
                .list_resources("docs", None)
                .await
                .expect("resource access remains available");
        });
        assert_eq!(McpManager::lock(&transport.requests).len(), 2);
        assert_eq!(McpManager::lock(&entry.restarts).count, 0);
        assert!(!transport.closed.load(Ordering::Acquire));
    }

    #[test]
    fn revocation_during_completion_blocks_sensitive_suggestions() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, transport) = fixture(
            &temp,
            vec![Ok(
                json!({"completion": {"values": ["sensitive-candidate"]}}),
            )],
            true,
        );
        let path = manager.inner.trust_path.clone();
        let fingerprint = manager.trust_fingerprint_for(&entry);
        *McpManager::lock(&transport.after_response) = Some(Arc::new(move || {
            TrustStore::load(&path)
                .expect("trust store")
                .deny("docs", &fingerprint, "operator")
                .expect("revoke");
        }));
        let reference = crate::mcp::McpCompletionReference::Prompt {
            name: "review".to_string(),
        };
        let error = runtime()
            .block_on(manager.complete_argument("docs", &reference, "arg", "", None))
            .expect_err("revoked suggestions");
        assert!(error.to_string().contains("MCP_TRUST_DENIED"));
        assert!(!error.to_string().contains("sensitive-candidate"));
        assert!(transport.closed.load(Ordering::Acquire));
    }

    #[test]
    fn mounted_resource_tool_lists_one_page_and_preserves_opaque_cursors() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, _, transport) = fixture(
            &temp,
            vec![
                Ok(json!({"resources": [], "nextCursor": "", "_meta": {"private": "secret"}})),
                Ok(
                    json!({"resources": [{"name":"schema","uri":"db://schema","_meta":{"private":"secret"}}]}),
                ),
            ],
            true,
        );
        let tools = crate::mcp::mount_tools(&manager);
        assert_eq!(tools.len(), 1, "resource-only server gets a context tool");
        let tool = &tools[0];
        runtime().block_on(async {
            let first = tool
                .execute("one", json!({"action":"list_resources"}), None)
                .await
                .expect("page");
            assert_eq!(
                transport.requests.lock().unwrap().len(),
                1,
                "no eager page traversal"
            );
            assert!(rendered(&first).contains("nextCursor"));
            assert!(!rendered(&first).contains("secret"));
            assert!(first.details.unwrap()["_meta"].is_object());
            let second = tool
                .execute("two", json!({"action":"list_resources","cursor":""}), None)
                .await
                .expect("page");
            assert!(rendered(&second).contains("db://schema"));
            assert!(!rendered(&second).contains("secret"));
        });
        let requests = McpManager::lock(&transport.requests).clone();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].0, "resources/list");
        assert_eq!(requests[0].1, json!({}));
        assert_eq!(requests[1].1, json!({"cursor":""}));
        assert!(
            requests
                .iter()
                .all(|(_, _, timeout)| *timeout == DEFAULT_MCP_TIMEOUT)
        );
    }

    #[test]
    fn resource_templates_are_discovered_without_reading_or_expanding_them() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, _, transport) = fixture(
            &temp,
            vec![Ok(json!({"resourceTemplates":[
            {"name":"source","uriTemplate":"repo://{path}{?ref}","description":"Sources"}
        ],"nextCursor":"+/= opaque"}))],
            true,
        );
        let tools = crate::mcp::mount_server_tools(&manager, "docs");
        let output = runtime()
            .block_on(tools[0].execute(
                "templates",
                json!({
                    "action":"list_resource_templates", "cursor":" +/= "
                }),
                None,
            ))
            .expect("templates");
        assert!(rendered(&output).contains("repo://{path}{?ref}"));
        let requests = McpManager::lock(&transport.requests);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].0, "resources/templates/list");
        assert_eq!(requests[0].1, json!({"cursor":" +/= "}));
        assert!(crate::mcp::mount_server_tools(&manager, "unknown").is_empty());
    }

    #[test]
    fn resource_reads_preserve_text_and_native_images_without_fetching_uris_locally() {
        let temp = tempfile::tempdir().expect("tempdir");
        let uri = "file:///path-that-does-not-exist/secret.txt";
        let (manager, _, transport) = fixture(
            &temp,
            vec![Ok(json!({"contents":[
                {"uri":uri,"text":"server document","_meta":{"secret":"not-prompt-text"}},
                {"uri":"image://diagram","mimeType":"image/png","blob":"AQID"}
            ]}))],
            true,
        );
        let tools = crate::mcp::mount_tools(&manager);
        let output = runtime()
            .block_on(tools[0].execute(
                "read",
                json!({
                    "action":"read_resource", "uri":uri
                }),
                None,
            ))
            .expect("resource");
        assert!(!output.is_error);
        assert!(rendered(&output).contains("server document"));
        assert!(!rendered(&output).contains("not-prompt-text"));
        assert!(output.content.iter().any(|block| matches!(block,
            crate::model::ContentBlock::Image(image) if image.data == "AQID" && image.mime_type == "image/png"
        )));
        let requests = McpManager::lock(&transport.requests);
        assert_eq!(requests[0].0, "resources/read");
        assert_eq!(requests[0].1, json!({"uri":uri}));
    }

    #[test]
    fn context_tools_never_mount_or_dispatch_for_pending_or_revoked_trust() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, _, transport) = fixture(&temp, Vec::new(), false);
        assert!(crate::mcp::mount_tools(&manager).is_empty());
        assert!(
            runtime()
                .block_on(manager.read_resource("docs", "db://secret"))
                .is_err()
        );
        assert!(McpManager::lock(&transport.requests).is_empty());

        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, transport) = fixture(&temp, Vec::new(), true);
        let tools = crate::mcp::mount_tools(&manager);
        TrustStore::load(&manager.inner.trust_path)
            .expect("trust store")
            .deny("docs", &manager.trust_fingerprint_for(&entry), "operator")
            .expect("revoke");
        assert!(crate::mcp::mount_tools(&manager).is_empty());
        assert!(
            runtime()
                .block_on(tools[0].execute(
                    "revoked",
                    json!({
                        "action":"read_resource", "uri":"db://secret"
                    }),
                    None
                ))
                .is_err()
        );
        assert!(McpManager::lock(&transport.requests).is_empty());
    }

    #[test]
    fn invalid_inputs_are_rejected_before_any_rpc() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, _, transport) = fixture(&temp, Vec::new(), true);
        let tools = crate::mcp::mount_tools(&manager);
        runtime().block_on(async {
            for input in [
                json!({"action":"tools/call"}),
                json!({"action":"read_resource"}),
                json!({"action":"read_resource","uri":""}),
                json!({"action":"read_resource","uri":"file://x\nsecret"}),
                json!({"action":"read_resource","uri":42}),
                json!({"action":"list_resources","cursor":42}),
                json!({"action":"list_resources","server":"other"}),
                json!({"action":"list_resources","uri":"file://ignored"}),
                json!({"action":"list_resources","cursor":"x".repeat(MAX_CURSOR_BYTES + 1)}),
            ] {
                assert!(tools[0].execute("bad", input, None).await.is_err());
            }
        });
        assert!(McpManager::lock(&transport.requests).is_empty());
    }

    #[test]
    fn revocation_during_response_prevents_context_publication() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, transport) = fixture(
            &temp,
            vec![Ok(json!({"contents":[
                {"uri":"db://secret","text":"must not escape"}
            ]}))],
            true,
        );
        let path = manager.inner.trust_path.clone();
        let fingerprint = manager.trust_fingerprint_for(&entry);
        *McpManager::lock(&transport.after_response) = Some(Arc::new(move || {
            TrustStore::load(&path)
                .expect("trust store")
                .deny("docs", &fingerprint, "operator")
                .expect("revoke");
        }));
        let error = runtime()
            .block_on(manager.read_resource("docs", "db://secret"))
            .expect_err("revoked");
        assert!(error.to_string().contains("MCP_TRUST_DENIED"));
        assert!(!error.to_string().contains("must not escape"));
        assert!(transport.closed.load(Ordering::Acquire));
    }

    #[test]
    fn transport_replacement_rejects_old_context_without_poisoning_replacement() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, old) = fixture(&temp, vec![Ok(json!({"resources":[]}))], true);
        let replacement = Arc::new(ContextTransport {
            replies: Mutex::new(VecDeque::new()),
            requests: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
            after_response: Mutex::new(None),
        });
        let erased: Arc<dyn McpTransport> = replacement.clone();
        let weak_entry = Arc::downgrade(&entry);
        *McpManager::lock(&old.after_response) = Some(Arc::new(move || {
            *McpManager::lock(&weak_entry.upgrade().expect("entry").transport) =
                Some(erased.clone());
        }));
        let error = runtime()
            .block_on(manager.list_resources("docs", None))
            .expect_err("superseded");
        assert!(error.to_string().contains("MCP_TRANSPORT_SUPERSEDED"));
        assert!(old.closed.load(Ordering::Acquire));
        assert!(!replacement.closed.load(Ordering::Acquire));
        assert_eq!(McpManager::lock(&entry.restarts).count, 0);
        assert!(McpManager::lock(&replacement.requests).is_empty());
    }

    #[test]
    fn dropped_context_request_retires_only_its_own_generation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, transport) = fixture(&temp, Vec::new(), true);
        let erased: Arc<dyn McpTransport> = transport.clone();
        drop(ContextRequestGuard {
            entry: entry.clone(),
            transport: erased,
            armed: true,
        });
        assert!(transport.closed.load(Ordering::Acquire));
        assert!(McpManager::lock(&entry.transport).is_none());
        assert_eq!(McpManager::lock(&entry.restarts).count, 1);
        assert_eq!(manager.context_server_names(None), vec!["docs"]);
    }

    #[test]
    fn server_errors_do_not_trigger_a_replay_or_poison_the_connection() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, transport) = fixture(
            &temp,
            vec![
                Err(tool_err(
                    "MCP_SERVER_ERROR",
                    "server error -32601: unsupported",
                )),
                Ok(json!({"contents":[{"uri":"db://known","text":"available"}]})),
            ],
            true,
        );
        runtime().block_on(async {
            assert!(manager.list_resources("docs", None).await.is_err());
            assert!(manager.read_resource("docs", "db://known").await.is_ok());
        });
        assert_eq!(McpManager::lock(&transport.requests).len(), 2);
        assert_eq!(McpManager::lock(&entry.restarts).count, 0);
        assert!(!transport.closed.load(Ordering::Acquire));
    }

    #[test]
    fn initial_missing_tools_method_keeps_resource_only_servers_usable() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, transport) = fixture(
            &temp,
            vec![
                Err(tool_err(
                    "MCP_SERVER_ERROR",
                    "server error -32601: no tools",
                )),
                Ok(json!({"resources":[]})),
            ],
            true,
        );
        runtime().block_on(async {
            assert!(
                manager
                    .list_and_cache_tools(&entry)
                    .await
                    .expect("no tools")
                    .is_empty()
            );
            assert!(manager.list_resources("docs", None).await.is_ok());
        });
        let methods: Vec<_> = McpManager::lock(&transport.requests)
            .iter()
            .map(|(method, _, _)| method.clone())
            .collect();
        assert_eq!(methods, vec!["tools/list", "resources/list"]);
        assert_eq!(McpManager::lock(&entry.restarts).count, 0);
        assert!(!transport.closed.load(Ordering::Acquire));
    }

    #[test]
    fn missing_tools_method_on_later_page_is_not_a_successful_partial_catalog() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, transport) = fixture(
            &temp,
            vec![
                Ok(json!({"tools":[],"nextCursor":"next"})),
                Err(tool_err("MCP_SERVER_ERROR", "server error -32601: gone")),
            ],
            true,
        );
        assert!(
            runtime()
                .block_on(manager.list_and_cache_tools(&entry))
                .is_err()
        );
        assert!(McpManager::lock(&entry.tools_cache).is_none());
        assert!(transport.closed.load(Ordering::Acquire));
    }

    #[test]
    fn malformed_or_oversized_responses_fail_closed() {
        for result in [
            json!(null),
            json!({"resources":[{"name":"missing-uri"}]}),
            json!({"resources":[],"nextCursor":null}),
            json!({"resources":[],"nextCursor":"x".repeat(MAX_CURSOR_BYTES + 1)}),
            json!({"resources":[{"name":"x","uri":"db://x","size":-1}]}),
            json!({"resources":[],"_meta":"x".repeat(MAX_CONTEXT_PAGE_BYTES)}),
            json!({"resources":vec![json!({"name":"x","uri":"db://x"});MAX_CONTEXT_ITEMS + 1]}),
        ] {
            let temp = tempfile::tempdir().expect("tempdir");
            let (manager, _, transport) = fixture(&temp, vec![Ok(result)], true);
            let error = runtime()
                .block_on(manager.list_resources("docs", None))
                .expect_err("invalid response");
            assert!(error.to_string().contains("MCP_PROTOCOL"));
            assert_eq!(McpManager::lock(&transport.requests).len(), 1);
            assert!(transport.closed.load(Ordering::Acquire));
        }
        for resource in [
            json!({"uri":"db://x"}),
            json!({"uri":"db://x","text":"a","blob":"Yg=="}),
            json!({"uri":"db://x","text":7}),
        ] {
            assert!(
                validate_response(
                    &json!({"contents":[resource]}),
                    ContextResult::ResourceContents
                )
                .is_err()
            );
        }
    }

    #[test]
    fn method_not_found_classification_uses_the_error_code_not_arbitrary_prose() {
        assert!(is_method_not_found(&tool_err(
            "MCP_SERVER_ERROR",
            "server error -32601: unsupported"
        )));
        for error in [
            tool_err("MCP_SERVER_ERROR", "server error -32600: mentions -32601"),
            tool_err("MCP_SERVER_ERROR", "server error -326010: different code"),
            tool_err("MCP_TRANSPORT_IO", "server error -32601: invalid taxonomy"),
            Error::tool(
                "other",
                "[MCP_SERVER_ERROR] server error -32601: wrong tool",
            ),
        ] {
            assert!(!is_method_not_found(&error));
        }
    }

    #[test]
    fn context_namespace_is_bounded_stable_and_disjoint_from_server_tools() {
        use crate::tools::Tool;
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, _, _) = fixture(&temp, Vec::new(), true);
        let mut names = std::collections::HashSet::new();
        for server in ["docs", "doc.s", "doc_s", "文档", &"s".repeat(200)] {
            let tool = crate::mcp::McpContextTool::new(server, manager.clone());
            assert!(tool.name().len() <= 64);
            assert!(!tool.name().starts_with("mcp__"));
            assert!(
                tool.name()
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            );
            assert!(names.insert(tool.name().to_string()));
            assert_eq!(
                tool.name(),
                crate::mcp::McpContextTool::new(server, manager.clone()).name()
            );
        }
    }

    #[test]
    fn shutdown_removes_context_mounts_and_rejects_stale_wrappers() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, _, transport) = fixture(&temp, Vec::new(), true);
        let tools = crate::mcp::mount_tools(&manager);
        runtime().block_on(async {
            manager.shutdown_all().await;
            assert!(crate::mcp::mount_tools(&manager).is_empty());
            assert!(
                tools[0]
                    .execute("old-session", json!({"action":"list_resources"}), None)
                    .await
                    .is_err()
            );
        });
        assert!(McpManager::lock(&transport.requests).is_empty());
    }

    #[test]
    fn prompt_catalog_exposes_arguments_and_cursor_but_not_private_metadata() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, _, transport) = fixture(
            &temp,
            vec![Ok(json!({
                "prompts": [{
                    "name": "review", "title": "Code review", "description": "Review code",
                    "arguments": [{
                        "name": "code", "description": "Source code", "required": true,
                        "_meta": {"secret": "argument-private"}
                    }],
                    "_meta": {"secret": "prompt-private"}
                }],
                "nextCursor": "", "_meta": {"secret": "catalog-private"}
            }))],
            true,
        );
        let tools = crate::mcp::mount_tools(&manager);
        let output = runtime()
            .block_on(tools[0].execute(
                "catalog",
                json!({"action":"list_prompts", "cursor":"opaque +/="}),
                None,
            ))
            .expect("prompt catalog");
        let public: Value = serde_json::from_str(&rendered(&output)).expect("public JSON");
        assert_eq!(public["nextCursor"], "");
        assert_eq!(public["prompts"][0]["arguments"][0]["required"], true);
        assert!(!rendered(&output).contains("private"));
        assert!(output.details.unwrap()["_meta"].is_object());
        let requests = McpManager::lock(&transport.requests);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].0, "prompts/list");
        assert_eq!(requests[0].1, json!({"cursor":"opaque +/="}));
    }

    #[test]
    fn selected_prompt_preserves_string_arguments_roles_and_native_content() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, _, transport) = fixture(
            &temp,
            vec![Ok(json!({
                "description": "Review reference",
                "messages": [
                    {"role":"user", "content":{"type":"text", "text":"Read this first"}},
                    {"role":"assistant", "content":{"type":"image", "mimeType":"image/png", "data":"AQID"}},
                    {"role":"user", "content":{"type":"resource", "resource":{
                        "uri":"repo://guide", "text":"Reference documentation"
                    }}}
                ],
                "_meta": {"secret":"not-model-context"}
            }))],
            true,
        );
        let tools = crate::mcp::mount_tools(&manager);
        let arguments = json!({"code":"fn main() {\n    println!(\"你好\");\n}", "mode":""});
        let output = runtime()
            .block_on(tools[0].execute(
                "prompt",
                json!({
                    "action":"get_prompt", "name":"review", "arguments":arguments
                }),
                None,
            ))
            .expect("selected prompt");
        assert!(!output.is_error);
        let text = rendered(&output);
        for expected in [
            "MCP prompt reference",
            "Prompt message 1 [user]",
            "Read this first",
            "Prompt message 2 [assistant]",
            "Prompt message 3 [user]",
            "Reference documentation",
        ] {
            assert!(text.contains(expected), "missing {expected}");
        }
        assert!(!text.contains("not-model-context"));
        assert_eq!(
            output.content.len(),
            3,
            "text must not move across native media"
        );
        assert!(
            matches!(&output.content[0], crate::model::ContentBlock::Text(text)
            if text.text.contains("Prompt message 2 [assistant]"))
        );
        assert!(
            matches!(&output.content[2], crate::model::ContentBlock::Text(text)
            if text.text.contains("Prompt message 3 [user]"))
        );
        assert!(output.content.iter().any(|block| matches!(block,
            crate::model::ContentBlock::Image(image) if image.data == "AQID"
        )));
        let requests = McpManager::lock(&transport.requests);
        assert_eq!(
            requests.len(),
            1,
            "prompt retrieval must not run its instructions"
        );
        assert_eq!(requests[0].0, "prompts/get");
        assert_eq!(
            requests[0].1,
            json!({"name":"review", "arguments":arguments})
        );
    }

    #[test]
    fn omitted_and_explicit_empty_prompt_arguments_remain_distinct() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, _, transport) = fixture(
            &temp,
            vec![Ok(json!({"messages":[]})), Ok(json!({"messages":[]}))],
            true,
        );
        runtime().block_on(async {
            manager
                .get_prompt("docs", "defaults", None)
                .await
                .expect("server defaults");
            manager
                .get_prompt("docs", "defaults", Some(&BTreeMap::new()))
                .await
                .expect("explicit empty arguments");
        });
        let requests = McpManager::lock(&transport.requests);
        assert_eq!(requests[0].1, json!({"name":"defaults"}));
        assert_eq!(requests[1].1, json!({"name":"defaults", "arguments":{}}));
    }

    #[test]
    fn prompt_inputs_are_bounded_and_typed_before_dispatch() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, _, transport) = fixture(&temp, Vec::new(), true);
        let tools = crate::mcp::mount_tools(&manager);
        runtime().block_on(async {
            for input in [
                json!({"action":"get_prompt"}),
                json!({"action":"get_prompt", "name":""}),
                json!({"action":"get_prompt", "name":"review", "arguments":{"code":123}}),
                json!({"action":"get_prompt", "name":"review", "arguments":["code"]}),
                json!({"action":"get_prompt", "name":"review", "arguments":{"":"bad name"}}),
                json!({"action":"get_prompt", "name":"review", "uri":"unrelated://field"}),
                json!({"action":"get_prompt", "name":"x".repeat(MAX_CONTEXT_NAME_BYTES + 1)}),
                json!({"action":"get_prompt", "name":"review", "arguments":{
                    "code":"x".repeat(MAX_PROMPT_ARGUMENT_BYTES)
                }}),
                // Count JSON escaping, not only the unescaped string length.
                json!({"action":"get_prompt", "name":"review", "arguments":{
                    "code":"\0".repeat(MAX_PROMPT_ARGUMENT_BYTES / 2)
                }}),
            ] {
                assert!(tools[0].execute("invalid", input, None).await.is_err());
            }
            let arguments = (0..=MAX_PROMPT_ARGUMENTS)
                .map(|index| (format!("arg-{index}"), String::new()))
                .collect::<BTreeMap<_, _>>();
            assert!(
                manager
                    .get_prompt("docs", "review", Some(&arguments))
                    .await
                    .is_err()
            );
        });
        assert!(McpManager::lock(&transport.requests).is_empty());
    }

    #[test]
    fn invalid_prompt_roles_or_shapes_are_not_partially_published() {
        for result in [
            json!({"messages":[{"role":"system", "content":{"type":"text", "text":"elevate"}}]}),
            json!({"messages":[{"role":"tool", "content":{"type":"text", "text":"elevate"}}]}),
            json!({"messages":[{"role":"user", "content":[]}]}),
            json!({"messages":[{"role":"assistant", "content":{"text":"missing type"}}]}),
            json!({"messages":[], "description":42}),
            json!({"messages":vec![json!({
                "role":"user", "content":{"type":"text", "text":"too many"}
            }); MAX_PROMPT_MESSAGES + 1]}),
        ] {
            let temp = tempfile::tempdir().expect("tempdir");
            let (manager, _, transport) = fixture(&temp, vec![Ok(result)], true);
            let error = runtime()
                .block_on(manager.get_prompt("docs", "review", None))
                .expect_err("invalid prompt");
            assert!(error.to_string().contains("MCP_PROTOCOL"));
            assert!(transport.closed.load(Ordering::Acquire));
            assert_eq!(McpManager::lock(&transport.requests).len(), 1);
        }
    }

    #[test]
    fn prompt_argument_declarations_are_validated_in_full() {
        for arguments in [
            json!(null),
            json!([{"name":"code", "required":"yes"}]),
            json!([{"name":"code", "description":false}]),
            json!([{"description":"missing name"}]),
            json!([{"name":"code"}, {"name":"code"}]),
        ] {
            assert!(
                validate_response(
                    &json!({"prompts":[{
                        "name":"review", "arguments":arguments
                    }]}),
                    ContextResult::Prompts
                )
                .is_err()
            );
        }
    }

    #[test]
    fn maximum_prompt_message_count_retains_the_last_message() {
        let temp = tempfile::tempdir().expect("tempdir");
        let messages: Vec<_> = (0..MAX_PROMPT_MESSAGES)
            .map(|index| {
                json!({
                    "role":"user", "content":{"type":"text", "text":format!("message-{index}")}
                })
            })
            .collect();
        let (manager, _, _) = fixture(
            &temp,
            vec![Ok(json!({
                "description":"At the message bound", "messages":messages
            }))],
            true,
        );
        let tools = crate::mcp::mount_tools(&manager);
        let output = runtime()
            .block_on(tools[0].execute("full", json!({"action":"get_prompt", "name":"full"}), None))
            .expect("whole prompt");
        assert!(!output.is_error);
        assert!(rendered(&output).contains(&format!("message-{}", MAX_PROMPT_MESSAGES - 1)));
        assert!(
            rendered(&output).contains(&format!("Prompt message {MAX_PROMPT_MESSAGES} [user]"))
        );
    }

    #[test]
    fn malformed_prompt_media_is_an_explicit_content_error() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, _, _) = fixture(
            &temp,
            vec![Ok(json!({"messages":[{
                "role":"user", "content":{"type":"image", "mimeType":"image/png", "data":"%%%"}
            }]}))],
            true,
        );
        let tools = crate::mcp::mount_tools(&manager);
        let output = runtime()
            .block_on(tools[0].execute(
                "bad-media",
                json!({"action":"get_prompt", "name":"media"}),
                None,
            ))
            .expect("explicit content error");
        assert!(output.is_error);
        assert!(rendered(&output).contains("MCP_CONTENT_INVALID"));
        assert!(!rendered(&output).contains("%%%"));
    }

    #[test]
    fn prompt_get_rechecks_trust_after_response_before_shaping() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, transport) = fixture(
            &temp,
            vec![Ok(json!({"messages":[{
                "role":"user", "content":{"type":"text", "text":"private prompt"}
            }]}))],
            true,
        );
        let tools = crate::mcp::mount_tools(&manager);
        let path = manager.inner.trust_path.clone();
        let fingerprint = manager.trust_fingerprint_for(&entry);
        *McpManager::lock(&transport.after_response) = Some(Arc::new(move || {
            TrustStore::load(&path)
                .expect("trust store")
                .deny("docs", &fingerprint, "operator")
                .expect("deny");
        }));
        let error = runtime()
            .block_on(tools[0].execute(
                "revoke",
                json!({"action":"get_prompt", "name":"private"}),
                None,
            ))
            .expect_err("revoked prompt");
        assert!(error.to_string().contains("MCP_TRUST_DENIED"));
        assert!(!error.to_string().contains("private prompt"));
        assert!(transport.closed.load(Ordering::Acquire));
    }

    #[test]
    fn cancelled_old_request_does_not_retire_a_replacement_connection() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, old) = fixture(&temp, Vec::new(), true);
        let old_erased: Arc<dyn McpTransport> = old.clone();
        let guard = ContextRequestGuard {
            entry: entry.clone(),
            transport: old_erased,
            armed: true,
        };
        let replacement = Arc::new(ContextTransport {
            replies: Mutex::new(vec![Ok(json!({"prompts":[]}))].into()),
            requests: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
            after_response: Mutex::new(None),
        });
        let erased: Arc<dyn McpTransport> = replacement.clone();
        *McpManager::lock(&entry.transport) = Some(erased.clone());
        drop(guard);
        assert!(old.closed.load(Ordering::Acquire));
        assert!(!replacement.closed.load(Ordering::Acquire));
        assert_eq!(McpManager::lock(&entry.restarts).count, 0);
        assert!(
            McpManager::lock(&entry.transport)
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &erased))
        );
        runtime()
            .block_on(manager.list_prompts("docs", None))
            .expect("replacement usable");
        assert!(McpManager::lock(&old.requests).is_empty());
        assert_eq!(McpManager::lock(&replacement.requests).len(), 1);
    }

    #[test]
    fn context_transport_failure_is_returned_once_without_replay() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, transport) = fixture(
            &temp,
            vec![
                Err(tool_err("MCP_TRANSPORT_CLOSED", "lost response")),
                Ok(json!({"contents":[{"uri":"db://x", "text":"must not retry"}]})),
            ],
            true,
        );
        let error = runtime()
            .block_on(manager.read_resource("docs", "db://x"))
            .expect_err("delivery failed");
        assert!(error.to_string().contains("MCP_TRANSPORT_CLOSED"));
        assert_eq!(McpManager::lock(&transport.requests).len(), 1);
        assert_eq!(McpManager::lock(&transport.replies).len(), 1);
        assert!(McpManager::lock(&entry.transport).is_none());
        assert!(transport.closed.load(Ordering::Acquire));
    }
}
