//! Public native-pointer coverage. Real Chromium tests are explicit DSR lanes,
//! never silently successful substitutes when a browser is unavailable.
#![forbid(unsafe_code)]
#![recursion_limit = "512"]

use pi::browser::{BrowserLaunchOptions, BrowserTool};
use pi::tools::Tool;
use serde_json::{Value, json};

fn event(case: &str, phase: &str) {
    eprintln!("{}", json!({"suite":"browser_pointer","case":case,"phase":phase}));
}

#[test]
fn invalid_pointer_options_fail_before_launch_or_network_access() {
    event("public-preflight", "start");
    let dir = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(dir.path()).with_mock(false).with_cdp_endpoint("invalid-endpoint");
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    runtime.block_on(async {
        for args in [
            json!({"action":"click","selector":"#x","button":"PRIVATE-CANARY"}),
            json!({"action":"click","selector":"#x","click_count":3}),
            json!({"action":"click","selector":"#x","button":"right","click_count":2}),
            json!({"action":"click","selector":"#x","modifiers":["Shift","Shift"]}),
            json!({"action":"click","selector":"#x","modifiers":null}),
            json!({"action":"press","key":"Enter","button":"left"}),
            json!({"action":"evaluate","script":"true","click_count":1}),
        ] {
            let error = tool.execute("pointer-preflight", args, None).await.unwrap_err().to_string();
            assert!(error.contains("BROWSER_POINTER_USAGE"), "{error}");
            assert!(!error.contains("PRIVATE-CANARY"));
            assert!(!error.contains("CDP endpoint"));
        }
    });
    event("public-preflight", "pass");
}

#[test]
fn explicit_pointer_sequences_are_described_and_never_simulated() {
    event("schema-and-mock", "start");
    let dir = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(dir.path()).with_mock(true);
    let schema = tool.parameters();
    assert_eq!(schema["properties"]["button"]["enum"], json!(["left","middle","right"]));
    assert_eq!(schema["properties"]["click_count"]["enum"], json!([1,2]));
    assert_eq!(schema["properties"]["modifiers"]["uniqueItems"], true);
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    runtime.block_on(async {
        for options in [json!({"button":"right"}),json!({"click_count":2}),json!({"modifiers":[]})] {
            let mut args = options;
            args["action"] = json!("click");
            args["selector"] = json!("#x");
            assert!(tool.execute("pointer-mock", args, None).await.is_err());
        }
    });
    event("schema-and-mock", "pass");
}

fn live_tool(cwd: &std::path::Path) -> BrowserTool {
    BrowserTool::new(cwd).with_mock(false).with_launch_options(BrowserLaunchOptions {
        executable_path: std::env::var_os("PI_BROWSER_EXECUTABLE").map(Into::into),
        ..Default::default()
    }).with_domain_allowlist(Some(Vec::new()))
}

async fn call(tool: &BrowserTool, args: Value) -> Value {
    eprintln!("{}", json!({"suite":"browser_pointer","phase":"dispatch",
        "action":args["action"],"frameScoped":args.get("frame").is_some()}));
    let result = tool.execute("pointer-live", args, None).await.unwrap();
    assert!(!result.is_error);
    result.details.unwrap()
}

#[test]
#[ignore = "requires sandbox-capable Chromium; select explicitly in the DSR browser lane"]
fn live_double_context_middle_and_modified_clicks_use_native_events() {
    event("native-buttons", "start");
    let dir = tempfile::tempdir().unwrap();
    let tool = live_tool(dir.path());
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    runtime.block_on(async {
        call(&tool, json!({"action":"open","tab":"pointer","url":"about:blank"})).await;
        call(&tool, json!({"action":"evaluate","tab":"pointer","script":r#"
            (()=>{
                document.body.innerHTML='<button id="target">Native target</button>';
                window.events=[];
                for(const type of ['mousedown','mouseup','click','dblclick','contextmenu','auxclick'])
                    target.addEventListener(type,e=>{
                        events.push({type:e.type,button:e.button,detail:e.detail,shift:e.shiftKey,trusted:e.isTrusted});
                        if(e.type==='contextmenu')e.preventDefault();
                    });
                return true;
            })()
        "#})).await;
        let result = call(&tool, json!({"action":"click","tab":"pointer","selector":"text/Native target",
            "click_count":2,"modifiers":["Shift"]})).await;
        assert_eq!(result["click_count"], 2);
        assert_eq!(result["modifier_mask"], 8);
        assert_eq!(result["dispatch_acknowledged"], true);
        assert_eq!(result["delivery_verified"], false);
        let result = call(&tool, json!({"action":"evaluate","tab":"pointer","script":"events"})).await;
        let events = result["result"].as_array().unwrap();
        let kinds: Vec<_> = events.iter().map(|event|event["type"].as_str().unwrap()).collect();
        assert_eq!(kinds, vec!["mousedown","mouseup","click","mousedown","mouseup","click","dblclick"]);
        assert!(events.iter().all(|event|event["trusted"] == true && event["shift"] == true && event["button"] == 0));
        assert_eq!(events.last().unwrap()["detail"], 2);
        call(&tool, json!({"action":"evaluate","tab":"pointer","script":"events=[];true"})).await;
        call(&tool, json!({"action":"click","tab":"pointer","selector":"#target","button":"right"})).await;
        let result = call(&tool, json!({"action":"evaluate","tab":"pointer","script":"events"})).await;
        assert_eq!(result["result"].as_array().unwrap().iter().filter(|event|event["type"] == "contextmenu" && event["trusted"] == true && event["button"] == 2).count(), 1);
        call(&tool, json!({"action":"evaluate","tab":"pointer","script":"events=[];true"})).await;
        call(&tool, json!({"action":"click","tab":"pointer","selector":"#target","button":"middle"})).await;
        let result = call(&tool, json!({"action":"evaluate","tab":"pointer","script":"events"})).await;
        assert_eq!(result["result"].as_array().unwrap().iter().filter(|event|event["type"] == "auxclick" && event["trusted"] == true && event["button"] == 1).count(), 1);
        call(&tool, json!({"action":"stop"})).await;
    });
    event("native-buttons", "pass");
}

#[test]
#[ignore = "requires sandbox-capable Chromium; select explicitly in the DSR browser lane"]
fn live_sequences_stop_on_replacement_and_respect_composed_tree_and_frame_scope() {
    event("identity-and-scope", "start");
    let dir = tempfile::tempdir().unwrap();
    let tool = live_tool(dir.path());
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    runtime.block_on(async {
        call(&tool, json!({"action":"open","tab":"pointer","url":"about:blank"})).await;
        call(&tool, json!({"action":"evaluate","tab":"pointer","script":r#"
            (()=>{
                document.body.innerHTML='<button id="replace">Replace</button><button disabled><span id="disabled">Disabled child</span></button><div id="host" inert></div>';
                window.first=0;window.replacement=0;
                replace.addEventListener('click',()=>{
                    first++;const next=document.createElement('button');next.id='replace';next.textContent='Replace';
                    next.onclick=()=>replacement++;replace.replaceWith(next);
                });
                host.attachShadow({mode:'open'}).innerHTML='<button data-testid="inert-control">Inert control</button>';
                return true;
            })()
        "#})).await;
        let error = tool.execute("interrupted-double", json!({"action":"click","tab":"pointer",
            "selector":"#replace","click_count":2}), None).await.unwrap_err().to_string();
        assert!(error.contains("BROWSER_POINTER_INTERRUPTED"), "{error}");
        let result = call(&tool, json!({"action":"evaluate","tab":"pointer","script":"[first,replacement]"})).await;
        assert_eq!(result["result"], json!([1,0]));
        for selector in ["#disabled","testid/inert-control"] {
            assert!(tool.execute("unavailable-control",json!({"action":"click","tab":"pointer","selector":selector}),None).await.is_err());
        }
        call(&tool, json!({"action":"evaluate","tab":"pointer","script":r#"
            new Promise(resolve=>{
                const frame=document.createElement('iframe');frame.name='pointer-child';
                frame.srcdoc='<button id="child" ondblclick="this.dataset.doubles=String(Number(this.dataset.doubles||0)+1)">Child</button>';
                frame.onload=()=>resolve(true);document.body.append(frame);
            })
        "#})).await;
        let result = call(&tool, json!({"action":"list_frames","tab":"pointer"})).await;
        let frame = result["frames"].as_array().unwrap().iter().find(|frame|frame["name"] == "pointer-child").unwrap()["frame_id"].clone();
        call(&tool, json!({"action":"click","tab":"pointer","frame":frame,"selector":"text/Child","click_count":2})).await;
        let result = call(&tool, json!({"action":"evaluate","tab":"pointer","frame":frame,
            "script":"document.querySelector('button').dataset.doubles"})).await;
        assert_eq!(result["result"], "1");
        call(&tool, json!({"action":"stop"})).await;
    });
    event("identity-and-scope", "pass");
}
