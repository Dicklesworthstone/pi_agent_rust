//! Regression coverage for pi-freeflow's optional Undici dispatcher import.

mod common;

use pi::extensions::{
    ExtensionEventName, ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle,
};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::tools::ToolRegistry;
use std::sync::Arc;
use std::time::Duration;

fn load_extension(harness: &common::TestHarness, source: &str) -> Result<ExtensionManager, String> {
    let cwd = harness.temp_dir().to_path_buf();
    let entry = harness.create_file("extensions/undici_test.mjs", source.as_bytes());
    let spec = JsExtensionLoadSpec::from_entry_path(&entry).expect("load spec");
    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let mut config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        ..Default::default()
    };
    // A compatibility scan cannot substitute static registrations for a module
    // which failed to load: these tests must execute real module resolution.
    config
        .env
        .insert("PI_EXT_COMPAT_SCAN".to_string(), "0".to_string());
    common::run_async(async move {
        let runtime = JsExtensionRuntimeHandle::start(config, tools, manager.clone())
            .await
            .map_err(|error| error.to_string())?;
        manager.set_js_runtime(runtime);
        if let Err(error) = manager.load_js_extensions(vec![spec]).await {
            let _ = manager.shutdown(Duration::from_secs(2)).await;
            return Err(error.to_string());
        }
        Ok(manager)
    })
}

#[test]
fn undici_named_imports_preserve_freeflow_dispatcher_fallback() {
    let harness = common::TestHarness::new("undici_fallback");
    let manager = load_extension(
        &harness,
        r#"
import { Agent, fetch, Headers, Response } from 'undici';
import process from 'node:process';

export default function activate(pi) {
  pi.on('agent_start', () => {
    let agentError = null;
    try { new Agent({ connections: 50, keepAliveTimeout: 60000 }); }
    catch (error) { agentError = error.code; }
    return {
      fetchIsHostFetch: fetch === globalThis.fetch,
      headersIsHostHeaders: Headers === globalThis.Headers,
      responseIsHostResponse: Response === globalThis.Response,
      undiciVersionAbsent: process.versions.undici === undefined,
      canUseRelayDispatcher: Number((process.versions.undici ?? '0').split('.')[0]) >= 7,
      agentError,
    };
  });
}
"#,
    )
    .expect("real named undici imports must load");
    let result = common::run_async(async move {
        let result = manager
            .dispatch_event_with_response(ExtensionEventName::AgentStart, None, 10_000)
            .await
            .expect("dispatch probe")
            .expect("probe result");
        assert!(
            // ubs:ignore[rust.panic.assert-macros] -- This test requires the runtime to shut down after successful imports.
            manager.shutdown(Duration::from_secs(2)).await
        );
        result
    });
    assert_eq!(
        result,
        serde_json::json!({
            "fetchIsHostFetch": true,
            "headersIsHostHeaders": true,
            "responseIsHostResponse": true,
            "undiciVersionAbsent": true,
            "canUseRelayDispatcher": false,
            "agentError": "ERR_PIJS_UNSUPPORTED_DISPATCHER",
        })
    );
}

#[test]
fn undici_registration_does_not_enable_arbitrary_npm_packages() {
    let harness = common::TestHarness::new("undici_no_npm_authority");
    let result = load_extension(
        &harness,
        r#"
import { Agent } from 'undici';
import unrelated from 'pijs-arbitrary-package-regression-270';
export default function activate() { void Agent; void unrelated; }
"#,
    );
    let error = match result {
        Ok(_) => panic!(
            // ubs:ignore[rust.ownership.panic-macro] -- This test must fail if arbitrary npm resolution succeeds.
            "Registering undici must not enable arbitrary npm resolution"
        ),
        Err(error) => error,
    };
    assert!(
        // ubs:ignore[rust.panic.assert-macros] -- Failure must preserve the existing unsupported-package diagnostic.
        error.contains("Package module specifiers are not supported"),
        "unexpected resolution error: {error}"
    );
    assert!(
        // ubs:ignore[rust.panic.assert-macros] -- The diagnostic must identify the unrelated package that remained blocked.
        error.contains("pijs-arbitrary-package-regression-270")
    );
}
