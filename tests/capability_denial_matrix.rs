//! Capability denial matrix tests (bd-23do).
//!
//! Systematic regression suite proving capability policy enforcement for every
//! connector method across all three policy modes (Strict, Prompt, Permissive).
//! Verifies denied calls return deterministic error payloads with correct error
//! codes, messages, and preserved correlation IDs.
#![allow(clippy::needless_raw_string_hashes)]

use std::future::Future;

use asupersync::runtime::RuntimeBuilder;
use pi::connectors::http::HttpConnector;
use pi::extensions::{
    ExtensionPolicy, ExtensionPolicyMode, HostCallContext, HostCallErrorCode, HostCallPayload,
    PolicyDecision, dispatch_host_call_shared,
};
use pi::tools::ToolRegistry;
use serde_json::json;
use tempfile::tempdir;

/// Run an async future that may hold non-Send references (like `HostCallContext`).
fn run_async<T, Fut>(future: Fut) -> T
where
    Fut: Future<Output = T>,
{
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("build asupersync runtime");
    runtime.block_on(future)
}

// ─── Policy Factories ───────────────────────────────────────────────────────

fn strict_default() -> ExtensionPolicy {
    ExtensionPolicy {
        mode: ExtensionPolicyMode::Strict,
        ..Default::default()
    }
}

fn prompt_default() -> ExtensionPolicy {
    ExtensionPolicy {
        mode: ExtensionPolicyMode::Prompt,
        ..Default::default()
    }
}

fn permissive_default() -> ExtensionPolicy {
    ExtensionPolicy {
        mode: ExtensionPolicyMode::Permissive,
        ..Default::default()
    }
}

fn deny_all() -> ExtensionPolicy {
    ExtensionPolicy {
        mode: ExtensionPolicyMode::Strict,
        max_memory_mb: 256,
        default_caps: Vec::new(),
        deny_caps: vec![
            "read".into(),
            "write".into(),
            "exec".into(),
            "http".into(),
            "tool".into(),
            "session".into(),
            "ui".into(),
            "events".into(),
            "env".into(),
            "log".into(),
        ],
    }
}

fn allow_only(caps: &[&str]) -> ExtensionPolicy {
    ExtensionPolicy {
        mode: ExtensionPolicyMode::Strict,
        max_memory_mb: 256,
        default_caps: caps.iter().map(|s| (*s).to_string()).collect(),
        deny_caps: Vec::new(),
    }
}

// ─── Context Helper ─────────────────────────────────────────────────────────

fn make_ctx<'a>(
    tools: &'a ToolRegistry,
    http: &'a HttpConnector,
    policy: &'a ExtensionPolicy,
) -> HostCallContext<'a> {
    HostCallContext {
        runtime_name: "test",
        extension_id: Some("ext.matrix"),
        tools,
        http,
        manager: None,
        policy,
        js_runtime: None,
        interceptor: None,
    }
}

fn make_call(
    call_id: &str,
    method: &str,
    capability: &str,
    params: serde_json::Value,
) -> HostCallPayload {
    HostCallPayload {
        call_id: call_id.to_string(),
        capability: capability.to_string(),
        method: method.to_string(),
        params,
        timeout_ms: None,
        cancel_token: None,
        context: None,
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// 1. Default deny_caps enforcement (exec, env denied across ALL modes)
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn exec_denied_in_strict_mode() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = strict_default();
    let ctx = make_ctx(&tools, &http, &policy);
    let call = make_call(
        "exec-strict",
        "exec",
        "exec",
        json!({"cmd": "rm", "args": ["-rf", "/"]}),
    );

    run_async(async {
        let result = dispatch_host_call_shared(&ctx, call).await;
        assert!(result.is_error);
        let err = result.error.expect("error payload");
        assert_eq!(err.code, HostCallErrorCode::Denied);
        assert!(err.message.contains("exec"));
        assert!(err.message.contains("denied"));
        assert_eq!(result.call_id, "exec-strict");
    });
}

#[test]
fn exec_denied_in_prompt_mode() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = prompt_default();
    let ctx = make_ctx(&tools, &http, &policy);
    let call = make_call("exec-prompt", "exec", "exec", json!({"cmd": "ls"}));

    run_async(async {
        let result = dispatch_host_call_shared(&ctx, call).await;
        assert!(result.is_error);
        let err = result.error.expect("error payload");
        assert_eq!(err.code, HostCallErrorCode::Denied);
        assert_eq!(result.call_id, "exec-prompt");
    });
}

#[test]
fn exec_denied_in_permissive_mode() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = permissive_default();
    let ctx = make_ctx(&tools, &http, &policy);
    let call = make_call("exec-perm", "exec", "exec", json!({"cmd": "ls"}));

    run_async(async {
        let result = dispatch_host_call_shared(&ctx, call).await;
        assert!(result.is_error);
        let err = result.error.expect("error payload");
        assert_eq!(err.code, HostCallErrorCode::Denied);
        assert_eq!(result.call_id, "exec-perm");
    });
}

#[test]
fn env_denied_in_strict_mode() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = strict_default();
    let ctx = make_ctx(&tools, &http, &policy);
    let call = make_call(
        "env-strict",
        "env",
        "env",
        json!({"op": "get", "key": "SECRET"}),
    );

    run_async(async {
        let result = dispatch_host_call_shared(&ctx, call).await;
        assert!(result.is_error);
        let err = result.error.expect("error payload");
        assert_eq!(err.code, HostCallErrorCode::Denied);
        assert!(err.message.contains("env"));
        assert_eq!(result.call_id, "env-strict");
    });
}

#[test]
fn env_denied_in_permissive_mode() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = permissive_default();
    let ctx = make_ctx(&tools, &http, &policy);
    let call = make_call(
        "env-perm",
        "env",
        "env",
        json!({"op": "get", "key": "HOME"}),
    );

    run_async(async {
        let result = dispatch_host_call_shared(&ctx, call).await;
        assert!(result.is_error);
        let err = result.error.expect("error payload");
        assert_eq!(err.code, HostCallErrorCode::Denied);
        assert_eq!(result.call_id, "env-perm");
    });
}

// ═══════════════════════════════════════════════════════════════════════════
// 2. Default allowed caps pass policy (read, write, http, events, session)
//    These proceed past policy but may fail at the actual handler level.
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn read_allowed_in_strict_mode_default_caps() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = strict_default();
    let ctx = make_ctx(&tools, &http, &policy);
    let call = make_call(
        "read-strict",
        "tool",
        "read",
        json!({"name": "read", "input": {"path": "/nonexistent"}}),
    );

    run_async(async {
        let result = dispatch_host_call_shared(&ctx, call).await;
        // Should NOT be denied by policy - may fail for other reasons (file not found)
        if result.is_error {
            let err = result.error.as_ref().expect("error payload");
            assert_ne!(
                err.code,
                HostCallErrorCode::Denied,
                "read should not be denied in strict mode with default caps"
            );
        }
        assert_eq!(result.call_id, "read-strict");
    });
}

#[test]
fn http_allowed_in_strict_mode_default_caps() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = strict_default();
    let ctx = make_ctx(&tools, &http, &policy);
    let call = make_call(
        "http-strict",
        "http",
        "http",
        json!({"url": "https://example.com"}),
    );

    run_async(async {
        let result = dispatch_host_call_shared(&ctx, call).await;
        if result.is_error {
            let err = result.error.as_ref().expect("error payload");
            assert_ne!(
                err.code,
                HostCallErrorCode::Denied,
                "http should not be denied in strict mode with default caps"
            );
        }
        assert_eq!(result.call_id, "http-strict");
    });
}

#[test]
fn session_allowed_in_strict_mode_default_caps() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = strict_default();
    let ctx = make_ctx(&tools, &http, &policy);
    let call = make_call(
        "session-strict",
        "session",
        "session",
        json!({"op": "getSessionName"}),
    );

    run_async(async {
        let result = dispatch_host_call_shared(&ctx, call).await;
        // May fail at handler level (no manager) but should NOT fail with
        // "denied by policy" message — policy should allow session in default_caps.
        if result.is_error {
            let err = result.error.as_ref().expect("error payload");
            assert!(
                !err.message.contains("denied by policy"),
                "session should pass policy check in strict mode with default caps, msg: {}",
                err.message
            );
        }
        assert_eq!(result.call_id, "session-strict");
    });
}

#[test]
fn events_passes_policy_in_strict_mode_default_caps() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = strict_default();
    let ctx = make_ctx(&tools, &http, &policy);
    let call = make_call("events-strict", "events", "events", json!({"op": "list"}));

    run_async(async {
        let result = dispatch_host_call_shared(&ctx, call).await;
        // May fail at handler level (no manager) but should NOT fail with
        // "denied by policy" message
        if result.is_error {
            let err = result.error.as_ref().expect("error payload");
            assert!(
                !err.message.contains("denied by policy"),
                "events should pass policy check in strict mode with default caps, msg: {}",
                err.message
            );
        }
        assert_eq!(result.call_id, "events-strict");
    });
}

// ═══════════════════════════════════════════════════════════════════════════
// 3. Strict mode denies unknown capabilities
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn ui_denied_in_strict_mode_not_in_default_caps() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    // Default caps: read, write, http, events, session. UI is NOT included.
    let policy = strict_default();
    let ctx = make_ctx(&tools, &http, &policy);
    let call = make_call(
        "ui-strict",
        "ui",
        "ui",
        json!({"op": "notify", "message": "hello"}),
    );

    run_async(async {
        let result = dispatch_host_call_shared(&ctx, call).await;
        assert!(
            result.is_error,
            "ui should be denied in strict mode (not in default_caps)"
        );
        let err = result.error.expect("error payload");
        assert_eq!(err.code, HostCallErrorCode::Denied);
        assert!(err.message.contains("ui"));
        assert_eq!(result.call_id, "ui-strict");
    });
}

#[test]
fn log_denied_in_strict_mode_not_in_default_caps() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = strict_default();
    let ctx = make_ctx(&tools, &http, &policy);
    let call = make_call(
        "log-strict",
        "log",
        "log",
        json!({"level": "info", "msg": "test"}),
    );

    run_async(async {
        let result = dispatch_host_call_shared(&ctx, call).await;
        assert!(
            result.is_error,
            "log should be denied in strict mode (not in default_caps)"
        );
        let err = result.error.expect("error payload");
        assert_eq!(err.code, HostCallErrorCode::Denied);
        assert_eq!(result.call_id, "log-strict");
    });
}

// ═══════════════════════════════════════════════════════════════════════════
// 4. Permissive mode allows unknown capabilities (unless deny_caps)
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn ui_allowed_in_permissive_mode() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = permissive_default();
    let ctx = make_ctx(&tools, &http, &policy);
    let call = make_call(
        "ui-perm",
        "ui",
        "ui",
        json!({"op": "notify", "message": "hello"}),
    );

    run_async(async {
        let result = dispatch_host_call_shared(&ctx, call).await;
        // May fail at handler level (no manager) but should NOT fail with
        // "denied by policy" — permissive mode allows all capabilities.
        if result.is_error {
            let err = result.error.as_ref().expect("error payload");
            assert!(
                !err.message.contains("denied by policy"),
                "ui should pass policy check in permissive mode, msg: {}",
                err.message
            );
        }
        assert_eq!(result.call_id, "ui-perm");
    });
}

#[test]
fn log_allowed_in_permissive_mode() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = permissive_default();
    let ctx = make_ctx(&tools, &http, &policy);
    let call = make_call(
        "log-perm",
        "log",
        "log",
        json!({"level": "info", "msg": "test"}),
    );

    run_async(async {
        let result = dispatch_host_call_shared(&ctx, call).await;
        if result.is_error {
            let err = result.error.as_ref().expect("error payload");
            assert_ne!(
                err.code,
                HostCallErrorCode::Denied,
                "log should not be denied in permissive mode"
            );
        }
        assert_eq!(result.call_id, "log-perm");
    });
}

// ═══════════════════════════════════════════════════════════════════════════
// 5. deny_all policy denies EVERY capability through dispatch
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn deny_all_matrix_through_dispatch() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = deny_all();
    let ctx = make_ctx(&tools, &http, &policy);

    let test_cases: Vec<(&str, &str, &str, serde_json::Value)> = vec![
        (
            "deny-read",
            "tool",
            "read",
            json!({"name": "read", "input": {"path": "/etc/passwd"}}),
        ),
        (
            "deny-write",
            "tool",
            "write",
            json!({"name": "write", "input": {"path": "/tmp/x", "content": "y"}}),
        ),
        ("deny-exec", "exec", "exec", json!({"cmd": "ls"})),
        (
            "deny-http",
            "http",
            "http",
            json!({"url": "https://evil.com"}),
        ),
        (
            "deny-session",
            "session",
            "session",
            json!({"op": "getSessionName"}),
        ),
        ("deny-ui", "ui", "ui", json!({"op": "notify"})),
        ("deny-events", "events", "events", json!({"op": "emit"})),
        (
            "deny-env",
            "env",
            "env",
            json!({"op": "get", "key": "SECRET"}),
        ),
        ("deny-log", "log", "log", json!({"level": "info"})),
    ];

    run_async(async {
        for (call_id, method, capability, params) in test_cases {
            let call = make_call(call_id, method, capability, params);
            let result = dispatch_host_call_shared(&ctx, call).await;

            assert!(result.is_error, "deny_all: {call_id} should be denied");
            let err = result
                .error
                .unwrap_or_else(|| panic!("deny_all: {call_id} error payload"));
            assert_eq!(
                err.code,
                HostCallErrorCode::Denied,
                "deny_all: {call_id} should have Denied error code, got {:?}",
                err.code
            );
            assert!(
                err.message.contains("denied"),
                "deny_all: {call_id} message should contain 'denied', got: {}",
                err.message
            );
            // Correlation ID preserved
            assert_eq!(
                result.call_id, call_id,
                "deny_all: call_id must be preserved"
            );
        }
    });
}

// ═══════════════════════════════════════════════════════════════════════════
// 6. allow_only policy: selective allowlist
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn allow_only_read_denies_write_and_exec() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = allow_only(&["read"]);
    let ctx = make_ctx(&tools, &http, &policy);

    run_async(async {
        // read should NOT be denied (it's in default_caps)
        let read_call = make_call(
            "allow-read",
            "tool",
            "read",
            json!({"name": "read", "input": {"path": "/tmp/x"}}),
        );
        let result = dispatch_host_call_shared(&ctx, read_call).await;
        if result.is_error {
            let err = result.error.as_ref().unwrap();
            assert_ne!(
                err.code,
                HostCallErrorCode::Denied,
                "read should be allowed"
            );
        }

        // write should be denied (not in default_caps, strict mode)
        let write_call = make_call(
            "deny-write",
            "tool",
            "write",
            json!({"name": "write", "input": {"path": "/tmp/x", "content": "y"}}),
        );
        let result = dispatch_host_call_shared(&ctx, write_call).await;
        assert!(result.is_error, "write should be denied");
        let err = result.error.expect("write error");
        assert_eq!(err.code, HostCallErrorCode::Denied);

        // exec should be denied
        let exec_call = make_call("deny-exec", "exec", "exec", json!({"cmd": "ls"}));
        let result = dispatch_host_call_shared(&ctx, exec_call).await;
        assert!(result.is_error, "exec should be denied");
        let err = result.error.expect("exec error");
        assert_eq!(err.code, HostCallErrorCode::Denied);
    });
}

#[test]
fn allow_only_http_denies_everything_else() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = allow_only(&["http"]);
    let ctx = make_ctx(&tools, &http, &policy);

    run_async(async {
        // http should be allowed
        let http_call = make_call(
            "allow-http",
            "http",
            "http",
            json!({"url": "https://api.example.com"}),
        );
        let result = dispatch_host_call_shared(&ctx, http_call).await;
        if result.is_error {
            let err = result.error.as_ref().unwrap();
            assert_ne!(
                err.code,
                HostCallErrorCode::Denied,
                "http should be allowed"
            );
        }

        // session should be denied
        let session_call = make_call(
            "deny-session",
            "session",
            "session",
            json!({"op": "getSessionName"}),
        );
        let result = dispatch_host_call_shared(&ctx, session_call).await;
        assert!(result.is_error);
        let err = result.error.expect("session error");
        assert_eq!(err.code, HostCallErrorCode::Denied);
    });
}

// ═══════════════════════════════════════════════════════════════════════════
// 7. Error response format validation
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn denied_error_format_has_required_fields() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = deny_all();
    let ctx = make_ctx(&tools, &http, &policy);
    let call = make_call("format-check", "exec", "exec", json!({"cmd": "test"}));

    run_async(async {
        let result = dispatch_host_call_shared(&ctx, call).await;

        // is_error must be true
        assert!(result.is_error);

        // call_id must be preserved
        assert_eq!(result.call_id, "format-check");

        // output must be an object (per HostResultPayload spec)
        assert!(result.output.is_object(), "output must be {{}} on error");

        // error must be present with correct code
        let err = result.error.expect("error payload must exist");
        assert_eq!(err.code, HostCallErrorCode::Denied);

        // error message must contain capability and reason
        assert!(
            err.message.contains("exec"),
            "message should contain capability name, got: {}",
            err.message
        );
        assert!(
            err.message.contains("deny_caps"),
            "message should contain reason, got: {}",
            err.message
        );

        // chunk must be None for non-streaming
        assert!(result.chunk.is_none());
    });
}

#[test]
fn denied_error_preserves_unique_call_ids() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = deny_all();
    let ctx = make_ctx(&tools, &http, &policy);

    let ids = ["id-001", "id-002", "id-003", "correlation-xyz", "req-12345"];

    run_async(async {
        for id in ids {
            let call = make_call(id, "exec", "exec", json!({"cmd": "test"}));
            let result = dispatch_host_call_shared(&ctx, call).await;
            assert_eq!(result.call_id, id, "correlation ID must be preserved");
        }
    });
}

// ═══════════════════════════════════════════════════════════════════════════
// 8. Case insensitivity bypass prevention
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn case_variants_of_denied_caps_blocked() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = ExtensionPolicy::default(); // deny_caps: [exec, env]
    let ctx = make_ctx(&tools, &http, &policy);

    let exec_variants = ["EXEC", "Exec", "eXeC", "ExEc"];

    run_async(async {
        for variant in exec_variants {
            let call = make_call(
                &format!("case-{variant}"),
                variant,
                variant,
                json!({"cmd": "test"}),
            );
            let result = dispatch_host_call_shared(&ctx, call).await;
            assert!(result.is_error, "case variant {variant} should be denied");
            let err = result.error.unwrap_or_else(|| panic!("{variant} error"));
            assert_eq!(
                err.code,
                HostCallErrorCode::Denied,
                "{variant}: expected Denied, got {:?}",
                err.code
            );
        }
    });
}

// ═══════════════════════════════════════════════════════════════════════════
// 9. Empty capability denied at dispatch level
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn empty_capability_denied_through_dispatch() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = permissive_default();
    let ctx = make_ctx(&tools, &http, &policy);

    // Empty method triggers invalid_request from validate_host_call before policy
    let call = make_call("empty-method", "", "", json!({}));

    run_async(async {
        let result = dispatch_host_call_shared(&ctx, call).await;
        assert!(result.is_error, "empty method should fail");
        let err = result.error.expect("error payload");
        // Empty method returns invalid_request, not denied
        assert_eq!(err.code, HostCallErrorCode::InvalidRequest);
        assert_eq!(result.call_id, "empty-method");
    });
}

// ═══════════════════════════════════════════════════════════════════════════
// 10. deny_caps override default_caps through dispatch
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn deny_caps_override_default_caps_through_dispatch() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    // Put "read" in both default_caps AND deny_caps — deny wins
    let policy = ExtensionPolicy {
        mode: ExtensionPolicyMode::Prompt,
        max_memory_mb: 256,
        default_caps: vec!["read".into(), "write".into(), "http".into()],
        deny_caps: vec!["read".into()],
    };
    let ctx = make_ctx(&tools, &http, &policy);

    run_async(async {
        // read is in both → denied
        let call = make_call(
            "override-read",
            "tool",
            "read",
            json!({"name": "read", "input": {"path": "/tmp/x"}}),
        );
        let result = dispatch_host_call_shared(&ctx, call).await;
        assert!(
            result.is_error,
            "read in deny_caps should be denied even if in default_caps"
        );
        let err = result.error.expect("error");
        assert_eq!(err.code, HostCallErrorCode::Denied);
        assert!(err.message.contains("deny_caps"));

        // write is only in default_caps → allowed
        let call = make_call(
            "allow-write",
            "tool",
            "write",
            json!({"name": "write", "input": {"path": "/tmp/x", "content": "y"}}),
        );
        let result = dispatch_host_call_shared(&ctx, call).await;
        if result.is_error {
            let err = result.error.as_ref().unwrap();
            assert_ne!(
                err.code,
                HostCallErrorCode::Denied,
                "write should not be denied"
            );
        }
    });
}

// ═══════════════════════════════════════════════════════════════════════════
// 11. deny_caps override permissive mode through dispatch
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn deny_caps_override_permissive_mode_through_dispatch() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = ExtensionPolicy {
        mode: ExtensionPolicyMode::Permissive,
        max_memory_mb: 256,
        default_caps: Vec::new(),
        deny_caps: vec!["http".into(), "session".into()],
    };
    let ctx = make_ctx(&tools, &http, &policy);

    run_async(async {
        // http in deny_caps → denied even in permissive
        let call = make_call(
            "perm-deny-http",
            "http",
            "http",
            json!({"url": "https://evil.com"}),
        );
        let result = dispatch_host_call_shared(&ctx, call).await;
        assert!(result.is_error);
        let err = result.error.expect("http error");
        assert_eq!(err.code, HostCallErrorCode::Denied);

        // session in deny_caps → denied even in permissive
        let call = make_call(
            "perm-deny-session",
            "session",
            "session",
            json!({"op": "getSessionName"}),
        );
        let result = dispatch_host_call_shared(&ctx, call).await;
        assert!(result.is_error);
        let err = result.error.expect("session error");
        assert_eq!(err.code, HostCallErrorCode::Denied);

        // events NOT in deny_caps → allowed in permissive (may fail at handler
        // level with no manager, but must NOT be "denied by policy")
        let call = make_call(
            "perm-allow-events",
            "events",
            "events",
            json!({"op": "list"}),
        );
        let result = dispatch_host_call_shared(&ctx, call).await;
        if result.is_error {
            let err = result.error.as_ref().unwrap();
            assert!(
                !err.message.contains("denied by policy"),
                "events should pass policy check in permissive (not in deny_caps), msg: {}",
                err.message
            );
        }
    });
}

// ═══════════════════════════════════════════════════════════════════════════
// 12. Policy evaluate() comprehensive matrix
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn full_policy_evaluate_matrix() {
    // Test every capability × mode combination through policy.evaluate()
    let all_caps = [
        "read", "write", "http", "events", "session", "exec", "env", "ui", "log", "tool",
    ];
    let default_caps_set = ["read", "write", "http", "events", "session"];
    let deny_caps_set = ["exec", "env"];

    let modes = [
        ("strict", ExtensionPolicyMode::Strict),
        ("prompt", ExtensionPolicyMode::Prompt),
        ("permissive", ExtensionPolicyMode::Permissive),
    ];

    for (mode_name, mode) in &modes {
        let policy = ExtensionPolicy {
            mode: *mode,
            ..Default::default()
        };

        for cap in &all_caps {
            let check = policy.evaluate(cap);
            let is_default = default_caps_set.contains(cap);
            let is_denied = deny_caps_set.contains(cap);

            if is_denied {
                assert_eq!(
                    check.decision,
                    PolicyDecision::Deny,
                    "{mode_name}/{cap}: deny_caps should always deny"
                );
                assert_eq!(check.reason, "deny_caps");
            } else if is_default {
                assert_eq!(
                    check.decision,
                    PolicyDecision::Allow,
                    "{mode_name}/{cap}: default_caps should always allow"
                );
                if matches!(mode, ExtensionPolicyMode::Permissive) {
                    assert_eq!(check.reason, "permissive");
                } else {
                    assert_eq!(check.reason, "default_caps");
                }
            } else {
                // Not in default or deny
                match mode {
                    ExtensionPolicyMode::Strict => {
                        assert_eq!(
                            check.decision,
                            PolicyDecision::Deny,
                            "{mode_name}/{cap}: strict denies unknown"
                        );
                        assert_eq!(check.reason, "not_in_default_caps");
                    }
                    ExtensionPolicyMode::Prompt => {
                        assert_eq!(
                            check.decision,
                            PolicyDecision::Prompt,
                            "{mode_name}/{cap}: prompt mode prompts for unknown"
                        );
                        assert_eq!(check.reason, "prompt_required");
                    }
                    ExtensionPolicyMode::Permissive => {
                        assert_eq!(
                            check.decision,
                            PolicyDecision::Allow,
                            "{mode_name}/{cap}: permissive allows unknown"
                        );
                        assert_eq!(check.reason, "permissive");
                    }
                }
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// 13. Prompt mode with no manager (fallback to deny)
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn prompt_mode_without_manager_denies_unknown_cap() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = prompt_default();
    // ctx.manager is None, so prompt resolution falls back to deny
    let ctx = make_ctx(&tools, &http, &policy);
    let call = make_call("prompt-no-mgr", "ui", "ui", json!({"op": "notify"}));

    run_async(async {
        let result = dispatch_host_call_shared(&ctx, call).await;
        // ui is not in default_caps → Prompt → no manager → denied with "shutdown" reason
        assert!(result.is_error, "prompt without manager should deny");
        let err = result.error.expect("error payload");
        assert_eq!(err.code, HostCallErrorCode::Denied);
        assert_eq!(result.call_id, "prompt-no-mgr");
    });
}

// ═══════════════════════════════════════════════════════════════════════════
// 14. Tool-specific capability mapping through dispatch
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn tool_bash_maps_to_exec_and_denied_by_default() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = ExtensionPolicy::default();
    let ctx = make_ctx(&tools, &http, &policy);

    let call = make_call(
        "tool-bash",
        "tool",
        "exec",
        json!({"name": "bash", "input": {"command": "rm -rf /"}}),
    );

    run_async(async {
        let result = dispatch_host_call_shared(&ctx, call).await;
        assert!(result.is_error, "bash tool should be denied (maps to exec)");
        let err = result.error.expect("error");
        assert_eq!(err.code, HostCallErrorCode::Denied);
    });
}

#[test]
fn tool_read_maps_to_read_and_allowed_by_default() {
    let dir = tempdir().expect("tempdir");
    let tools = ToolRegistry::new(&[], dir.path(), None);
    let http = HttpConnector::with_defaults();
    let policy = ExtensionPolicy::default();
    let ctx = make_ctx(&tools, &http, &policy);

    let call = make_call(
        "tool-read",
        "tool",
        "read",
        json!({"name": "read", "input": {"path": "/tmp/nonexistent"}}),
    );

    run_async(async {
        let result = dispatch_host_call_shared(&ctx, call).await;
        // Should NOT be denied by policy
        if result.is_error {
            let err = result.error.as_ref().unwrap();
            assert_ne!(
                err.code,
                HostCallErrorCode::Denied,
                "read tool should not be denied by default policy"
            );
        }
    });
}
