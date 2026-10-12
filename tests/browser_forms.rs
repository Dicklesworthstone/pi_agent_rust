//! Public form actions and an explicitly selected real-Chromium scenario.
//! The ignored scenario never silently passes when Chromium is unavailable.
#![forbid(unsafe_code)]
#![recursion_limit = "512"]

use pi::browser::{BrowserLaunchOptions, BrowserTool};
use pi::tools::Tool;
use serde_json::{Value, json};

#[test]
fn selection_schema_and_preflight_work_without_a_browser_connection() {
    let dir = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(dir.path()).with_mock(false)
        .with_cdp_endpoint("not-a-CDP-endpoint");
    let schema = tool.parameters();
    assert!(schema["properties"]["action"]["enum"].as_array().unwrap().contains(&json!("select_option")));
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    runtime.block_on(async {
        for args in [
            json!({"action":"select_option","selector":"#s"}),
            json!({"action":"select_option","selector":"#s","options":null}),
            json!({"action":"select_option","selector":"#s","options":[{"index":-1}]}),
            json!({"action":"select_option","selector":"#s","options":[],"text":"ignored"}),
            json!({"action":"select_option","selector":"#s","options":[],"dialog_response":{}}),
        ] {
            let error = tool.execute("form-preflight", args, None).await.unwrap_err().to_string();
            assert!(!error.contains("CDP endpoint"), "{error}");
        }
    });
}

#[test]
fn selection_never_reports_mock_success() {
    let dir = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(dir.path()).with_mock(true);
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    assert!(runtime.block_on(tool.execute("form-mock", json!({
        "action":"select_option","selector":"#s","options":[{"value":"a"}],
    }), None)).is_err());
}

async fn call(tool: &BrowserTool, args: Value) -> Value {
    let result = tool.execute("form-live", args, None).await.unwrap();
    assert!(!result.is_error);
    result.details.unwrap()
}

#[test]
#[ignore = "requires sandbox-capable Chromium; select explicitly in the DSR browser lane"]
fn live_selection_preserves_options_events_idempotency_and_frame_scope() {
    let dir = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(dir.path()).with_mock(false)
        .with_launch_options(BrowserLaunchOptions {
            executable_path: std::env::var_os("PI_BROWSER_EXECUTABLE").map(Into::into),
            ..Default::default()
        }).with_domain_allowlist(Some(Vec::new()));
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    runtime.block_on(async {
        call(&tool, json!({"action":"open","tab":"forms","url":"about:blank"})).await;
        call(&tool, json!({"action":"evaluate","tab":"forms","script":r#"
            (() => {
                document.body.innerHTML='<select id="choice" multiple><option value="a" selected>A</option><option value="b">B</option><option value="c">C</option><option value="x">Duplicate</option><option value="x">Duplicate</option><option value="d" disabled>D</option></select>';
                window.events=[];
                for (const kind of ['input','change']) choice.addEventListener(kind,e=>events.push(e.type));
                return true;
            })()
        "#})).await;
        let result = call(&tool, json!({"action":"select_option","tab":"forms","selector":"#choice",
            "options":[{"label":"C"},{"value":"b"}]})).await;
        assert_eq!(result["selected"][0]["index"], 1);
        assert_eq!(result["selected"][1]["index"], 2);
        assert_eq!(result["changed"], true);
        assert_eq!(result["input_method"], "dom-selection");
        let result = call(&tool, json!({"action":"select_option","tab":"forms","selector":"#choice",
            "options":[{"index":2},{"index":1}]})).await;
        assert_eq!(result["changed"], false);
        let result = call(&tool, json!({"action":"evaluate","tab":"forms","script":"events"})).await;
        assert_eq!(result["result"], json!(["input","change"]));
        for options in [json!([{"value":"a"},{"value":"missing"}]),
            json!([{"value":"x"}]), json!([{"value":"d"}]), json!([{"index":1},{"value":"b"}])] {
            assert!(tool.execute("invalid-selection", json!({"action":"select_option","tab":"forms",
                "selector":"#choice","options":options}), None).await.is_err());
            let value = call(&tool, json!({"action":"evaluate","tab":"forms",
                "script":"Array.from(choice.selectedOptions,o=>o.value)"})).await;
            assert_eq!(value["result"], json!(["b","c"]));
        }
        let result = call(&tool, json!({"action":"select_option","tab":"forms","selector":"#choice","options":[]})).await;
        assert_eq!(result["selected"], json!([]));

        call(&tool, json!({"action":"evaluate","tab":"forms","script":r#"
            new Promise(resolve=>{
                const f=document.createElement('iframe');f.name='form-child';
                f.srcdoc='<select id="choice"><option value="parent">Parent</option><option value="child">Child</option></select>';
                f.onload=()=>resolve(true);document.body.append(f);
            })
        "#})).await;
        let frames = call(&tool, json!({"action":"list_frames","tab":"forms"})).await;
        let frame = frames["frames"].as_array().unwrap().iter()
            .find(|frame| frame["name"] == "form-child").unwrap()["frame_id"].clone();
        let result = call(&tool, json!({"action":"select_option","tab":"forms","frame":frame,
            "selector":"#choice","options":[{"value":"child"}]})).await;
        assert_eq!(result["selected"][0]["value"], "child");
        let parent = call(&tool, json!({"action":"evaluate","tab":"forms","script":"choice.selectedOptions.length"})).await;
        assert_eq!(parent["result"], 0);

        call(&tool, json!({"action":"evaluate","tab":"forms","script":
            "choice.addEventListener('change',()=>{choice.selectedIndex=0});true"
        })).await;
        assert!(tool.execute("overridden-selection", json!({"action":"select_option","tab":"forms",
            "selector":"#choice","options":[{"value":"b"}]}), None).await.is_err());
        call(&tool, json!({"action":"stop"})).await;
    });
}
