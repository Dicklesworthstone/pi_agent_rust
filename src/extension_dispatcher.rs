//! Hostcall dispatcher for JS extensions.
//!
//! This module introduces the core `ExtensionDispatcher` abstraction used to route
//! hostcall requests (tools, HTTP, session, UI, etc.) from the JS runtime to
//! Rust implementations.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use async_trait::async_trait;

use crate::connectors::http::HttpConnector;
use crate::error::Result;
use crate::extensions::{ExtensionSession, ExtensionUiRequest, ExtensionUiResponse};
use crate::extensions_js::{HostcallRequest, PiJsRuntime};
use crate::scheduler::{Clock as SchedulerClock, HostcallOutcome, WallClock};
use crate::tools::ToolRegistry;

/// Coordinates hostcall dispatch between the JS extension runtime and Rust handlers.
pub struct ExtensionDispatcher<C: SchedulerClock = WallClock> {
    /// The JavaScript runtime that generates hostcall requests.
    runtime: Rc<PiJsRuntime<C>>,
    /// Registry of available tools (built-in + extension-registered).
    tool_registry: Arc<ToolRegistry>,
    /// HTTP connector for pi.http() calls.
    http_connector: Arc<HttpConnector>,
    /// Session access for pi.session() calls.
    session: Arc<dyn ExtensionSession + Send + Sync>,
    /// UI handler for pi.ui() calls.
    ui_handler: Arc<dyn ExtensionUiHandler + Send + Sync>,
    /// Current working directory for relative path resolution.
    cwd: PathBuf,
}

impl<C: SchedulerClock + 'static> ExtensionDispatcher<C> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        runtime: Rc<PiJsRuntime<C>>,
        tool_registry: Arc<ToolRegistry>,
        http_connector: Arc<HttpConnector>,
        session: Arc<dyn ExtensionSession + Send + Sync>,
        ui_handler: Arc<dyn ExtensionUiHandler + Send + Sync>,
        cwd: PathBuf,
    ) -> Self {
        Self {
            runtime,
            tool_registry,
            http_connector,
            session,
            ui_handler,
            cwd,
        }
    }

    /// Drain pending hostcall requests from the JS runtime.
    #[must_use]
    pub fn drain_hostcall_requests(&self) -> VecDeque<HostcallRequest> {
        self.runtime.drain_hostcall_requests()
    }
}

/// Trait for handling individual hostcall types.
#[async_trait]
pub trait HostcallHandler: Send + Sync {
    /// Process a hostcall request and return the outcome.
    async fn handle(&self, params: serde_json::Value) -> HostcallOutcome;

    /// The capability name for policy checking (e.g., "read", "exec", "http").
    fn capability(&self) -> &'static str;
}

/// Trait for handling UI hostcalls (pi.ui()).
#[async_trait]
pub trait ExtensionUiHandler: Send + Sync {
    async fn request_ui(&self, request: ExtensionUiRequest) -> Result<Option<ExtensionUiResponse>>;
}

#[cfg(test)]
#[allow(clippy::arc_with_non_send_sync)]
mod tests {
    use super::*;

    use crate::scheduler::DeterministicClock;
    use crate::session::SessionMessage;
    use serde_json::Value;
    use std::path::Path;

    struct NullSession;

    #[async_trait]
    impl ExtensionSession for NullSession {
        async fn get_state(&self) -> Value {
            Value::Null
        }

        async fn get_messages(&self) -> Vec<SessionMessage> {
            Vec::new()
        }

        async fn get_entries(&self) -> Vec<Value> {
            Vec::new()
        }

        async fn get_branch(&self) -> Vec<Value> {
            Vec::new()
        }

        async fn set_name(&self, _name: String) -> Result<()> {
            Ok(())
        }

        async fn append_message(&self, _message: SessionMessage) -> Result<()> {
            Ok(())
        }

        async fn append_custom_entry(
            &self,
            _custom_type: String,
            _data: Option<Value>,
        ) -> Result<()> {
            Ok(())
        }
    }

    struct NullUiHandler;

    #[async_trait]
    impl ExtensionUiHandler for NullUiHandler {
        async fn request_ui(
            &self,
            _request: ExtensionUiRequest,
        ) -> Result<Option<ExtensionUiResponse>> {
            Ok(None)
        }
    }

    fn build_dispatcher(
        runtime: Rc<PiJsRuntime<DeterministicClock>>,
    ) -> ExtensionDispatcher<DeterministicClock> {
        ExtensionDispatcher::new(
            runtime,
            Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
            Arc::new(HttpConnector::with_defaults()),
            Arc::new(NullSession),
            Arc::new(NullUiHandler),
            PathBuf::from("."),
        )
    }

    #[test]
    fn dispatcher_constructs() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            assert!(Rc::ptr_eq(&dispatcher.runtime, &runtime));
            assert_eq!(dispatcher.cwd, PathBuf::from("."));
        });
    }

    #[test]
    fn dispatcher_drains_empty_queue() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            let drained = dispatcher.drain_hostcall_requests();
            assert!(drained.is_empty());
        });
    }

    #[test]
    fn dispatcher_drains_runtime_requests() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            runtime
                .eval(r#"pi.tool("read", { "path": "test.txt" });"#)
                .await
                .expect("eval");

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            let drained = dispatcher.drain_hostcall_requests();
            assert_eq!(drained.len(), 1);
        });
    }
}
