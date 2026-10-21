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
        self.calls.lock().expect("fixture calls").push(call.arguments.clone());
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
    output.content.iter().filter_map(|block| match block {
        ContentBlock::Text(block) => Some(block.text.as_str()),
        _ => None,
    }).collect()
}

async fn cell(tool: &EvalTool, host: &Host, code: &str) -> ToolOutput {
    let protect = |parts: &[&str]| Ok(parts.iter().map(|part| (*part).to_owned()).collect());
    tool.execute_with_host(
        "browser-script",
        json!({"kernel":"js", "code":code}),
        None,
        &protect,
        host,
    ).await.expect("native eval outcome")
}

#[test]
fn browser_helpers_are_installed_and_use_the_native_invocation_host() {
    let root = tempfile::tempdir().expect("tempdir");
    let tool = tool(root.path());
    let host = host();
    let output = runtime().block_on(cell(&tool, &host, concat!(
        "globalThis.page = await browser.open({name:'app',url:'https://example.test'});",
        "await page.getByLabel('Email').fill('person@example.test');",
        "const result = await page.snapshot(); JSON.stringify(result.details.request)",
    )));
    assert!(!output.is_error, "{}", text(&output));
    let calls = host.calls.lock().expect("calls");
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0]["action"], "open");
    assert_eq!(calls[1]["selector"], "label/Email");
    assert_eq!(calls[2], json!({"action":"snapshot", "tab":"app"}));
    let details = output.details.expect("native details");
    assert_eq!(details["toolCalls"]["calls"].as_array().expect("receipts").len(), 3);
}

#[test]
fn retained_tab_handles_cannot_bypass_a_later_host_denial() {
    let root = tempfile::tempdir().expect("tempdir");
    let tool = tool(root.path());
    let host = host();
    runtime().block_on(async {
        let first = cell(&tool, &host,
            "globalThis.page = browser.tab('app'); await page.snapshot()").await;
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
    let output = runtime().block_on(cell(&tool, &host, concat!(
        "globalThis.evidence = null; try {",
        "await browser.tab('app').batch([{call:{action:'click',selector:'#save'}}]);",
        "await browser.tab('app').reload();",
        "} catch (error) { evidence = {name:error.name, result:error.result}; }",
        "JSON.stringify(evidence)",
    )));
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
    let output = runtime().block_on(tool.execute(
        "standalone-browser",
        json!({"kernel":"js", "code":"await browser.tab('app').snapshot()"}),
        None,
    )).expect("native eval outcome");
    assert!(output.is_error);
    assert!(text(&output).contains("EVAL_BRIDGE_DENIED"));
}
