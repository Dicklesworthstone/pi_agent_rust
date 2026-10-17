//! Public browser-frame preflight and a real Chromium integration scenario.
//! The live test is explicitly ignored unless selected by the DSR browser lane;
//! it never substitutes a simulated browser or silently passes without Chromium.
#![forbid(unsafe_code)]
#![recursion_limit = "512"]

use pi::browser::{BrowserLaunchOptions, BrowserTool};
use pi::tools::Tool;
use serde_json::{Value, json};

async fn call(tool: &BrowserTool, args: Value) -> Value {
    let result = tool.execute("frame-integration", args, None).await.unwrap();
    assert!(!result.is_error);
    result.details.expect("browser result details")
}

fn child_id(details: &Value) -> String {
    details["frames"]
        .as_array()
        .unwrap()
        .iter()
        .find(|frame| frame["name"] == "form-frame")
        .expect("fixture frame is discoverable")["frame_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn public_frame_schema_and_preflight_reject_ambiguous_input_without_connecting() {
    let dir = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(dir.path())
        .with_mock(false)
        .with_cdp_endpoint("http://not-a-loopback-host.invalid:9222");
    let schema = tool.parameters();
    assert!(
        schema["properties"]["action"]["enum"]
            .as_array()
            .unwrap()
            .contains(&json!("list_frames"))
    );
    assert_eq!(schema["properties"]["frame"]["type"], "string");
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        for args in [
            json!({"action":"snapshot","frame":"child"}),
            json!({"action":"press","tab":"work","frame":"child","key":"Enter"}),
            json!({"action":"scroll","tab":"work","frame":"child"}),
            json!({"action":"upload","tab":"work","frame":"child","files":[]}),
        ] {
            let error = tool
                .execute("invalid-frame", args, None)
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("frame"), "{error}");
            assert!(
                !error.contains("CDP endpoint"),
                "frame validation must precede connection configuration: {error}"
            );
        }
    });
}

#[test]
#[ignore = "requires an installed sandbox-capable Chromium; select explicitly in the DSR browser lane"]
#[allow(clippy::too_many_lines)]
#[allow(clippy::literal_string_with_formatting_args)] // inline CSS braces, not format arguments
fn live_chromium_frame_forms_use_native_input_and_cannot_hit_ancestor_overlays() {
    let dir = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(dir.path())
        .with_mock(false)
        .with_launch_options(BrowserLaunchOptions {
            executable_path: std::env::var_os("PI_BROWSER_EXECUTABLE").map(Into::into),
            ..Default::default()
        })
        .with_domain_allowlist(Some(Vec::new()));
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        call(&tool, json!({"action":"open","tab":"frames","url":"about:blank"})).await;
        let html = r#"<!doctype html><title>Child form</title>
            <style>body{margin:12px}input,button{display:block;margin:12px;width:180px;height:32px}</style>
            <label for="field">Frame field</label><input id="field">
            <button id="go" onclick="document.body.dataset.clicked='yes'"><span>Frame button</span></button>"#;
        let setup = format!(r#"new Promise(resolve => {{
            document.title = 'Parent document';
            document.body.innerHTML = '<input id="field" value="PARENT-CANARY">';
            const frame = document.createElement('iframe');
            frame.id = 'fixture-frame'; frame.name = 'form-frame';
            frame.style.cssText = 'position:absolute;left:260px;top:140px;width:320px;height:250px;border:0';
            frame.onload = () => resolve(true);
            frame.srcdoc = {};
            document.body.append(frame);
        }})"#, serde_json::to_string(html).unwrap());
        call(&tool, json!({"action":"evaluate","tab":"frames","script":setup})).await;
        let frame = child_id(&call(&tool, json!({"action":"list_frames","tab":"frames"})).await);
        let snapshot = call(&tool, json!({"action":"snapshot","tab":"frames","frame":frame})).await;
        assert_eq!(snapshot["snapshot"]["title"], "Child form");
        assert_eq!(snapshot["reference_scope"], "selected-frame document");
        let button_ref = snapshot["snapshot"]["elements"].as_array().unwrap().iter()
            .find(|element| element["role"] == "button" && element["text"] == "Frame button")
            .expect("frame button in accessibility snapshot")["ref_id"].as_str().unwrap().to_owned();
        // Snapshotting the parent must not replace the child's reference bucket.
        call(&tool, json!({"action":"snapshot","tab":"frames"})).await;
        assert!(tool.execute("wrong-document", json!({
            "action":"click","tab":"frames","selector":button_ref,
        }), None).await.is_err());

        call(&tool, json!({"action":"fill","tab":"frames","frame":frame,"selector":"#field","text":"literal \"🦀\""})).await;
        call(&tool, json!({"action":"press","tab":"frames","frame":frame,"selector":"#field","key":"End"})).await;
        call(&tool, json!({"action":"type","tab":"frames","frame":frame,"selector":"#field","text":"!"})).await;
        let value = call(&tool, json!({
            "action":"evaluate","tab":"frames","frame":frame,
            "script":"document.querySelector('#field').value",
        })).await;
        assert_eq!(value["result"], "literal \"🦀\"!");
        let parent = call(&tool, json!({
            "action":"evaluate","tab":"frames","script":"document.querySelector('#field').value",
        })).await;
        assert_eq!(parent["result"], "PARENT-CANARY");
        call(&tool, json!({"action":"wait_for","tab":"frames","frame":frame,"selector":"#go"})).await;

        // A parent overlay covers the iframe without changing its local DOM.
        // The local helper alone would approve this click; page-level hit testing must not.
        call(&tool, json!({"action":"evaluate","tab":"frames","script":
            "(() => { const cover=document.createElement('div'); cover.id='cover'; cover.style.cssText='position:fixed;inset:0;z-index:2147483647;background:black'; document.body.append(cover); return true; })()"
        })).await;
        assert!(tool.execute("covered-frame", json!({
            "action":"click","tab":"frames","frame":frame,"selector":button_ref,
        }), None).await.is_err());
        let unchanged = call(&tool, json!({
            "action":"evaluate","tab":"frames","frame":frame,
            "script":"document.body.dataset.clicked || 'no'",
        })).await;
        assert_eq!(unchanged["result"], "no");
        call(&tool, json!({"action":"evaluate","tab":"frames","script":"document.querySelector('#cover').remove()"})).await;
        call(&tool, json!({"action":"click","tab":"frames","frame":frame,"selector":button_ref})).await;
        let clicked = call(&tool, json!({
            "action":"evaluate","tab":"frames","frame":frame,"script":"document.body.dataset.clicked",
        })).await;
        assert_eq!(clicked["result"], "yes");

        // A fresh document can reuse DOM IDs, never the prior snapshot's authority.
        call(&tool, json!({"action":"evaluate","tab":"frames","script":
            "new Promise(resolve => { const frame=document.querySelector('#fixture-frame'); frame.onload=()=>resolve(true); frame.srcdoc='<button id=go>Replacement</button>'; })"
        })).await;
        assert!(tool.execute("stale-frame-reference", json!({
            "action":"click","tab":"frames","frame":frame,"selector":button_ref,
        }), None).await.is_err());
        call(&tool, json!({"action":"evaluate","tab":"frames","script":"document.querySelector('#fixture-frame').remove()"})).await;
        assert!(tool.execute("detached-frame", json!({
            "action":"evaluate","tab":"frames","frame":frame,"script":"document.title",
        }), None).await.is_err());
        call(&tool, json!({"action":"stop"})).await;
    });
}
