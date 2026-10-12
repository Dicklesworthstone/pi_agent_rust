//! Native cross-process iframe pointer input, with no external website.
//! This lane must actually observe a separate renderer and trusted DOM events.
//! Missing Chromium or missing process isolation is a failure, not a fake pass.
#![forbid(unsafe_code)]
#![recursion_limit = "512"]

use pi::browser::{BrowserLaunchOptions, BrowserTool};
use pi::tools::Tool;
use serde_json::{Value, json};

async fn call(tool: &BrowserTool, args: Value) -> Value {
    let result = tool
        .execute("remote-pointer-integration", args, None)
        .await
        .unwrap();
    assert!(!result.is_error);
    result.details.expect("browser details")
}

async fn evaluate(tool: &BrowserTool, frame: Option<&str>, script: &str) -> Value {
    let mut args = json!({"action":"evaluate","tab":"pointer","script":script});
    if let Some(frame) = frame {
        args["frame"] = json!(frame);
    }
    call(tool, args).await["result"].clone()
}

#[test]
#[ignore = "requires sandbox-capable Chromium with isolated sandboxed srcdoc frames; select in the DSR browser lane"]
#[allow(clippy::too_many_lines)]
fn live_remote_pointer_input_projects_transforms_scroll_and_rejects_parent_overlays() {
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
        call(&tool, json!({"action":"open","tab":"pointer","url":"about:blank"})).await;
        let child = r#"<!doctype html><title>Remote pointer fixture</title>
            <style>body{margin:0;height:2200px}button{position:absolute;left:40px;top:900px;width:130px;height:40px}</style>
            <button id="button" onclick="this.dataset.trusted=String(event.isTrusted);this.dataset.count=String(Number(this.dataset.count||0)+1)">Native target</button>"#;
        let setup = format!(r#"new Promise(resolve => {{
            document.body.style.cssText='margin:0;height:2200px';
            const frame=document.createElement('iframe');
            frame.id='pointer-frame';frame.name='pointer-frame';
            frame.setAttribute('sandbox','allow-scripts');
            frame.style.cssText='position:absolute;left:210px;top:120px;width:420px;height:300px;border:10px solid black;padding:7px';
            frame.onload=()=>resolve(true);frame.srcdoc={};document.body.append(frame);
        }})"#, serde_json::to_string(child).unwrap());
        evaluate(&tool, None, &setup).await;
        let inventory = call(&tool, json!({"action":"list_frames","tab":"pointer"})).await;
        let frame = inventory["frames"].as_array().unwrap().iter()
            .find(|frame| frame["name"] == "pointer-frame")
            .expect("sandboxed child frame");
        assert_eq!(frame["out_of_process"], true, "must exercise an actual remote renderer");
        let frame = frame["frame_id"].as_str().unwrap().to_owned();
        let snapshot = call(&tool, json!({"action":"snapshot","tab":"pointer","frame":frame})).await;
        let reference = snapshot["snapshot"]["elements"].as_array().unwrap().iter()
            .find(|element| element["role"] == "button" && element["text"] == "Native target")
            .expect("remote button reference")["ref_id"].as_str().unwrap().to_owned();
        evaluate(&tool, Some(&frame), r"(() => {
            document.body.dataset.wheels='0';
            addEventListener('wheel', event => {
                document.body.dataset.wheelTrusted=String(event.isTrusted);
                document.body.dataset.wheels=String(Number(document.body.dataset.wheels)+1);
            }, {passive:true});return true;
        })()").await;
        for (index, transform) in [
            "none", "scale(0.7)", "rotate(12deg)",
            "perspective(600px) rotateY(15deg)", "rotate(-9deg) scale(0.8)",
        ].into_iter().enumerate() {
            let script = format!(
                "document.querySelector('#pointer-frame').style.transform={}",
                serde_json::to_string(transform).unwrap(),
            );
            evaluate(&tool, None, &script).await;
            evaluate(&tool, Some(&frame), "scrollTo(0,800)").await;
            let selector = if index.is_multiple_of(2) { reference.as_str() } else { "#button" };
            call(&tool, json!({"action":"click","tab":"pointer","frame":frame,"selector":selector})).await;
            let state = evaluate(&tool, Some(&frame),
                "({trusted:document.querySelector('#button').dataset.trusted,count:Number(document.querySelector('#button').dataset.count)})").await;
            assert_eq!(state["trusted"], "true", "native click, not JavaScript dispatch");
            assert_eq!(state["count"].as_u64().unwrap(), u64::try_from(index + 1).unwrap());
            call(&tool, json!({"action":"scroll","tab":"pointer","frame":frame,"selector":selector,"delta_y":50})).await;
            // CDP acknowledges wheel dispatch before the compositor may deliver
            // it. Bound the wait in the page and still assert actual delivery.
            let wheel = evaluate(&tool, Some(&frame), r"new Promise(resolve => {
                const deadline=performance.now()+2000;
                const poll=()=>{
                    if(document.body.dataset.wheelTrusted==='true'||performance.now()>deadline)
                        resolve(document.body.dataset.wheelTrusted||'missing');
                    else setTimeout(poll,10);
                };poll();
            })").await;
            assert_eq!(wheel, "true");
            evaluate(&tool, Some(&frame), "document.body.dataset.wheelTrusted='pending'").await;
        }
        evaluate(&tool, None,
            "document.querySelector('#pointer-frame').style.transform='none';document.querySelector('#pointer-frame').style.top='920px';scrollTo(0,800)").await;
        evaluate(&tool, Some(&frame), "scrollTo(0,800)").await;
        call(&tool, json!({"action":"click","tab":"pointer","frame":frame,"selector":reference})).await;
        let before = evaluate(&tool, Some(&frame),
            "({count:document.querySelector('#button').dataset.count,wheels:document.body.dataset.wheels})").await;
        assert_eq!(before["count"], "6");
        evaluate(&tool, None, r"(() => {
            const overlay=document.createElement('div');overlay.id='cover';
            overlay.style.cssText='position:fixed;inset:0;z-index:2147483647;background:black';
            document.body.append(overlay);return true;
        })()").await;
        for action in ["click", "scroll"] {
            let result = tool.execute("covered-remote-frame", json!({
                "action":action,"tab":"pointer","frame":frame,"selector":reference,"delta_y":50,
            }), None).await;
            assert!(result.is_err(), "ancestor overlay must prevent {action}");
        }
        let after = evaluate(&tool, Some(&frame),
            "({count:document.querySelector('#button').dataset.count,wheels:document.body.dataset.wheels})").await;
        assert_eq!(after, before, "no remote pointer input through the parent overlay");
        call(&tool, json!({"action":"stop"})).await;
    });
}
