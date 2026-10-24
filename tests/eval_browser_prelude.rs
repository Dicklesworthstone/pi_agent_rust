//! Actual QuickJS -> native Eval bridge integration with a controlled host.
//! These tests do not stand in for Chromium/CDP acceptance.

use pi::error::{Error, Result};
use pi::eval::EvalTool;
use pi::model::{ContentBlock, TextContent, ToolCall};
use pi::tools::{Tool, ToolInvocationHost, ToolOutput};
use serde_json::{Value, json};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

struct Host {
    calls: Mutex<Vec<Value>>,
    enabled: AtomicBool,
    fail: AtomicBool,
}

#[async_trait::async_trait]
impl ToolInvocationHost for Host {
    async fn call_tool(&self, call: ToolCall, expected_session: &str) -> Result<ToolOutput> {
        assert_eq!(expected_session, "browser-prelude-test");
        assert_eq!(call.name, "browser");
        self.calls
            .lock()
            .expect("fixture calls")
            .push(call.arguments.clone());
        if !self.enabled.load(Ordering::SeqCst) {
            return Err(Error::tool("browser", "current browser policy denied"));
        }
        let failed = self.fail.load(Ordering::SeqCst);
        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(if failed {
                "postcondition failed after input"
            } else {
                "native browser result"
            }))],
            details: Some(json!({"completedSteps": 1, "request": call.arguments})),
            is_error: failed,
        })
    }
}

fn host() -> Host {
    Host {
        calls: Mutex::new(Vec::new()),
        enabled: AtomicBool::new(true),
        fail: AtomicBool::new(false),
    }
}

fn runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .expect("native fixture runtime")
}

fn tool(root: &std::path::Path) -> EvalTool {
    let mut tool = EvalTool::new(root);
    tool.bind_job_session_scope(pi::jobs::JobSessionScope::fixed("browser-prelude-test"));
    tool
}

fn text(output: &ToolOutput) -> String {
    output
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(block) => Some(block.text.as_str()),
            _ => None,
        })
        .collect()
}

async fn cell(tool: &EvalTool, host: &Host, code: &str) -> ToolOutput {
    let protect = |parts: &[&str]| Ok(parts.iter().map(|part| (*part).to_owned()).collect());
    tool.execute_with_host(
        "browser-script",
        json!({"kernel":"js", "code":code}),
        None,
        &protect,
        host,
    )
    .await
    .expect("native eval outcome")
}

#[test]
fn browser_helpers_are_installed_and_use_the_native_invocation_host() {
    let root = tempfile::tempdir().expect("tempdir");
    let tool = tool(root.path());
    let host = host();
    let output = runtime().block_on(cell(
        &tool,
        &host,
        concat!(
            "globalThis.page = await browser.open({name:'app',url:'https://example.test'});",
            "await page.getByLabel('Email').fill('person@example.test');",
            "const result = await page.snapshot(); JSON.stringify(result.details.request)",
        ),
    ));
    assert!(!output.is_error, "{}", text(&output));
    let calls = host.calls.lock().expect("calls");
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0]["action"], "open");
    assert_eq!(calls[1]["selector"], "label/Email");
    assert_eq!(calls[2], json!({"action":"snapshot", "tab":"app"}));
    let details = output.details.expect("native details");
    assert_eq!(
        details["toolCalls"]["calls"]
            .as_array()
            .expect("receipts")
            .len(),
        3
    );
}

#[test]
fn retained_tab_handles_cannot_bypass_a_later_host_denial() {
    let root = tempfile::tempdir().expect("tempdir");
    let tool = tool(root.path());
    let host = host();
    runtime().block_on(async {
        let first = cell(
            &tool,
            &host,
            "globalThis.page = browser.tab('app'); await page.snapshot()",
        )
        .await;
        assert!(!first.is_error, "{}", text(&first));
        host.enabled.store(false, Ordering::SeqCst);
        let second = cell(&tool, &host, "await page.locator('#save').click()").await;
        assert!(second.is_error);
        assert!(text(&second).contains("current browser policy denied"));
        assert_eq!(host.calls.lock().expect("calls").len(), 2);
    });
}

#[test]
fn caught_browser_failure_keeps_partial_evidence_and_never_replays_input() {
    let root = tempfile::tempdir().expect("tempdir");
    let tool = tool(root.path());
    let host = host();
    host.fail.store(true, Ordering::SeqCst);
    let output = runtime().block_on(cell(
        &tool,
        &host,
        concat!(
            "globalThis.evidence = null; try {",
            "await browser.tab('app').batch([{call:{action:'click',selector:'#save'}}]);",
            "await browser.tab('app').reload();",
            "} catch (error) { evidence = {name:error.name, result:error.result}; }",
            "JSON.stringify(evidence)",
        ),
    ));
    assert!(!output.is_error, "{}", text(&output));
    assert!(text(&output).contains("BrowserActionError"));
    assert!(text(&output).contains("completedSteps"));
    assert_eq!(host.calls.lock().expect("calls").len(), 1);
    let details = output.details.expect("native details");
    assert_eq!(details["toolCalls"]["calls"][0]["status"], "failed");
}

#[test]
fn standalone_eval_does_not_gain_browser_authority_from_the_prelude() {
    let root = tempfile::tempdir().expect("tempdir");
    let tool = tool(root.path());
    let output = runtime()
        .block_on(tool.execute(
            "standalone-browser",
            json!({"kernel":"js", "code":"await browser.tab('app').snapshot()"}),
            None,
        ))
        .expect("native eval outcome");
    assert!(output.is_error);
    assert!(text(&output).contains("EVAL_BRIDGE_DENIED"));
}

fn python_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

async fn python_cell(tool: &EvalTool, host: &Host, code: &str, reset: bool) -> ToolOutput {
    let protect = |parts: &[&str]| Ok(parts.iter().map(|part| (*part).to_owned()).collect());
    tool.execute_with_host(
        "python-browser-script",
        json!({"kernel":"python", "code":code, "reset":reset}),
        None,
        &protect,
        host,
    )
    .await
    .expect("native Python eval outcome")
}

#[test]
fn python_browser_startup_preserves_workspace_imports_namespace_and_retained_handles() {
    if !python_available() {
        return;
    }
    let root = tempfile::tempdir().expect("tempdir");
    std::fs::write(root.path().join("workspace_module.py"), "value = 42\n")
        .expect("workspace import fixture");
    let tool = tool(root.path());
    let host = host();
    runtime().block_on(async {
        let first = python_cell(
            &tool,
            &host,
            concat!(
                "import sys, workspace_module\n",
                "assert __name__ == '__main__'\n",
                "assert workspace_module.value == 42\n",
                "assert sys.path[0] == ''\n",
                "assert len(sys.argv) == 2 and sys.argv[1].endswith('kernel.py')\n",
                "page = await browser.open(name='app', url='https://example.test')\n",
                "field = page.get_by_label('Email')\n",
                "await field.fill('person@example.test')\n",
                "assert page.opened['details']['request']['action'] == 'open'\n",
            ),
            false,
        )
        .await;
        assert!(!first.is_error, "{}", text(&first));
        let later = python_cell(&tool, &host, "await field.press('Enter')", false).await;
        assert!(!later.is_error, "{}", text(&later));
        let calls = host.calls.lock().expect("calls");
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[1]["selector"], "label/Email");
        assert_eq!(
            calls[2],
            json!({
                "action":"press", "tab":"app", "selector":"label/Email", "key":"Enter",
            })
        );
        assert_eq!(
            later.details.expect("details")["toolCalls"]["calls"][0]["status"],
            "completed"
        );
    });
}

#[test]
fn python_script_consumes_native_results_and_submits_one_scoped_batch() {
    if !python_available() {
        return;
    }
    let root = tempfile::tempdir().expect("tempdir");
    let tool = tool(root.path());
    let host = host();
    let output = runtime().block_on(python_cell(&tool, &host, concat!(
        "page = browser.tab('app').frame('native-frame')\n",
        "observed = await page.snapshot()\n",
        "if observed['details']['completedSteps'] == 1:\n",
        "    await page.batch([\n",
        "        {'call': {'action': 'snapshot'}, 'expect': {'pointer': '/ready', 'equals': True}},\n",
        "        {'call': {'action': 'click'}, 'bind': {'selector': {'step': 0, 'pointer': '/ref'}}},\n",
        "    ], timeout_ms=10000)\n",
    ), false));
    assert!(!output.is_error, "{}", text(&output));
    let calls = host.calls.lock().expect("calls");
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["frame"], "native-frame");
    assert_eq!(calls[1]["action"], "batch");
    assert_eq!(calls[1]["steps"][1]["call"]["tab"], "app");
    assert_eq!(calls[1]["steps"][1]["call"]["frame"], "native-frame");
    assert_eq!(calls[1]["steps"][1]["bind"]["selector"]["step"], 0);
}

#[test]
fn python_browser_failure_retains_partial_evidence_and_a_failed_host_receipt() {
    if !python_available() {
        return;
    }
    let root = tempfile::tempdir().expect("tempdir");
    let tool = tool(root.path());
    let host = host();
    host.fail.store(true, Ordering::SeqCst);
    let output = runtime().block_on(python_cell(
        &tool,
        &host,
        concat!(
            "evidence = None\n",
            "try:\n",
            "    await browser.tab('app').locator('#save').click()\n",
            "    await browser.tab('app').reload()\n",
            "except browser.ActionError as error:\n",
            "    evidence = error.result\n",
            "    assert error.action == 'click'\n",
            "assert evidence['details']['completedSteps'] == 1\n",
            "assert evidence['isError'] is True\n",
        ),
        false,
    ));
    assert!(!output.is_error, "{}", text(&output));
    assert_eq!(host.calls.lock().expect("calls").len(), 1);
    assert_eq!(
        output.details.expect("details")["toolCalls"]["calls"][0]["status"],
        "failed"
    );
}

#[test]
fn python_browser_handle_reuses_the_current_host_and_standalone_stays_denied() {
    if !python_available() {
        return;
    }
    let root = tempfile::tempdir().expect("tempdir");
    let tool = tool(root.path());
    let host = host();
    runtime().block_on(async {
        let first = python_cell(
            &tool,
            &host,
            "page = browser.tab('app')\nawait page.snapshot()",
            false,
        )
        .await;
        assert!(!first.is_error, "{}", text(&first));
        host.enabled.store(false, Ordering::SeqCst);
        let denied = python_cell(&tool, &host, "await page.snapshot()", false).await;
        assert!(denied.is_error);
        assert!(text(&denied).contains("current browser policy denied"));
        let standalone = tool
            .execute(
                "standalone-python-browser",
                json!({
                    "kernel":"python", "code":"await page.snapshot()",
                }),
                None,
            )
            .await
            .expect("standalone cell outcome");
        assert!(standalone.is_error);
        assert!(text(&standalone).contains("EVAL_BRIDGE_DENIED"));
        assert_eq!(host.calls.lock().expect("calls").len(), 2);
    });
}

#[test]
fn python_reset_reinstalls_browser_without_preserving_old_user_handles() {
    if !python_available() {
        return;
    }
    let root = tempfile::tempdir().expect("tempdir");
    let tool = tool(root.path());
    let host = host();
    runtime().block_on(async {
        let first = python_cell(&tool, &host, "old = browser.tab('app')\nvalue = 42", false).await;
        assert!(!first.is_error, "{}", text(&first));
        let reset = python_cell(
            &tool,
            &host,
            concat!(
                "assert 'old' not in globals() and 'value' not in globals()\n",
                "await browser.tab('fresh').snapshot()\n",
            ),
            true,
        )
        .await;
        assert!(!reset.is_error, "{}", text(&reset));
        assert_eq!(reset.details.expect("details")["reset"], true);
        assert_eq!(
            host.calls.lock().expect("calls").as_slice(),
            &[json!({"action":"snapshot", "tab":"fresh"})]
        );
    });
}

#[test]
fn python_browser_batch_preflight_refuses_a_later_scope_change_before_dispatch() {
    if !python_available() {
        return;
    }
    let root = tempfile::tempdir().expect("tempdir");
    let tool = tool(root.path());
    let host = host();
    let output = runtime().block_on(python_cell(
        &tool,
        &host,
        concat!(
            "await browser.tab('app').batch([\n",
            "    {'call': {'action': 'click', 'selector': '#save'}},\n",
            "    {'call': {'action': 'snapshot', 'tab': 'different'}},\n",
            "])\n",
        ),
        false,
    ));
    assert!(output.is_error);
    assert!(text(&output).contains("EVAL_BROWSER_INPUT"));
    assert!(host.calls.lock().expect("calls").is_empty());
}
