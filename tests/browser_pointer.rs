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

#[test]
fn gestures_require_explicit_scopes_and_reject_invalid_options_before_connection() {
    event("gesture-public-preflight", "start");
    let dir = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(dir.path()).with_mock(false).with_cdp_endpoint("invalid-endpoint");
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    runtime.block_on(async {
        for args in [
            json!({"action":"hover","selector":"#x"}),
            json!({"action":"drag","tab":"page","selector":"#x"}),
            json!({"action":"drag","tab":"page","selector":"#x","target_selector":""}),
            json!({"action":"drag","tab":"page","selector":"#x","target_selector":"#y","drag_steps":0}),
            json!({"action":"drag","tab":"page","selector":"#x","target_selector":"#y","duration_ms":5001}),
            json!({"action":"hover","tab":"page","selector":"#x","button":"left"}),
            json!({"action":"hover","tab":"page","selector":"#x","frame":"PRIVATE-CANARY"}),
            json!({"action":"drag","tab":"page","selector":"#x","target_selector":"#y","frame":"PRIVATE-CANARY"}),
            json!({"action":"drag","tab":"page","selector":"#x","target_selector":"#y","dialog_response":{}}),
            json!({"action":"click","tab":"page","selector":"#x","target_selector":"#y"}),
        ] {
            let expected_code = if args["action"] == "hover" { "BROWSER_HOVER_USAGE" } else { "BROWSER_GESTURE_USAGE" };
            let error = tool.execute("gesture-preflight", args, None).await.unwrap_err().to_string();
            assert!(error.contains(expected_code), "{error}");
            assert!(!error.contains("PRIVATE-CANARY"));
            assert!(!error.contains("CDP endpoint"));
        }
        for action in ["hover", "drag"] {
            let mut args = json!({"action":action,"tab":"page","selector":"#x",
                "wait_after":{"navigation":{"url":"https://example.com/done"}}});
            if action == "drag" { args["target_selector"] = json!("#y"); }
            let error = tool.execute("gesture-navigation", args, None).await.unwrap_err().to_string();
            assert!(error.contains("BROWSER_NAVIGATION_USAGE"), "{error}");
        }
    });
    event("gesture-public-preflight", "pass");
}

#[test]
fn native_gestures_are_in_the_public_and_batch_schemas_but_never_the_mock_backend() {
    event("gesture-schema-and-mock", "start");
    let dir = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(dir.path()).with_mock(true);
    let schema = tool.parameters();
    for action in ["hover", "drag"] {
        assert!(schema["properties"]["action"]["enum"].as_array().unwrap().contains(&json!(action)));
        assert!(schema["properties"]["steps"]["items"]["properties"]["call"]["properties"]["action"]["enum"]
            .as_array().unwrap().contains(&json!(action)));
    }
    assert_eq!(schema["properties"]["drag_steps"]["maximum"], 64);
    assert_eq!(schema["properties"]["duration_ms"]["maximum"], 5000);
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    runtime.block_on(async {
        for action in ["hover", "drag"] {
            let mut args = json!({"action":action,"tab":"page","selector":"#x"});
            if action == "drag" { args["target_selector"] = json!("#y"); }
            assert!(tool.execute("gesture-mock", args, None).await.is_err());
        }
    });
    event("gesture-schema-and-mock", "pass");
}

#[test]
fn hover_schema_mock_refusal_and_preflight_are_connected_to_the_public_tool() {
    event("hover-public", "start");
    let dir = tempfile::tempdir().unwrap();
    let native = BrowserTool::new(dir.path()).with_mock(false).with_cdp_endpoint("invalid-endpoint");
    let mock = BrowserTool::new(dir.path()).with_mock(true);
    let schema = native.parameters();
    assert!(schema["properties"]["action"]["enum"].as_array().unwrap().contains(&json!("hover")));
    assert!(schema["properties"]["steps"]["items"]["properties"]["call"]["properties"]["action"]["enum"]
        .as_array().unwrap().contains(&json!("hover")));
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    runtime.block_on(async {
        for args in [json!({"action":"hover","selector":"#menu"}),
            json!({"action":"hover","tab":"app","selector":"#menu","button":"left"}),
            json!({"action":"hover","tab":"app","selector":"#menu","frame":"child"}),
            json!({"action":"hover","tab":"app","selector":"#menu","wait_after":{"navigation":{"url":"https://app.example/next"}}})] {
            let error = native.execute("hover-preflight", args, None).await.unwrap_err().to_string();
            assert!(!error.contains("CDP endpoint"), "{error}");
        }
        let error = mock.execute("hover-mock", json!({"action":"hover","tab":"app","selector":"#menu"}), None)
            .await.unwrap_err().to_string();
        assert!(error.contains("native backend"));
    });
    event("hover-public", "pass");
}

#[test]
#[ignore = "requires sandbox-capable Chromium; select explicitly in the DSR browser lane"]
fn live_hover_reveals_css_menu_with_trusted_events_without_clicking() {
    event("hover-live", "start");
    let dir = tempfile::tempdir().unwrap();
    let tool = live_tool(dir.path());
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    runtime.block_on(async {
        call(&tool, json!({"action":"open","tab":"hover","url":"about:blank"})).await;
        call(&tool, json!({"action":"evaluate","tab":"hover","script":r#"
            (()=>{
                document.body.innerHTML='<style>#menu{position:absolute;left:200px;top:150px;padding:25px}#panel{display:none}#menu:hover #panel{display:block}</style><div id="menu" data-testid="menu">Menu<div id="panel">Ready now</div></div>';
                window.moves=[];window.clicks=0;
                menu.addEventListener('pointerover',e=>moves.push({trusted:e.isTrusted,buttons:e.buttons,shift:e.shiftKey}));
                menu.addEventListener('click',()=>clicks++);return true;
            })()
        "#})).await;
        let result = call(&tool, json!({"action":"hover","tab":"hover","selector":"testid/menu",
            "modifiers":["Shift"],"wait_after":{"selector":"#panel","text":"Ready now","stable_ms":100}})).await;
        assert_eq!(result["action"], "hover");
        assert_eq!(result["dispatch_acknowledged"], true);
        assert_eq!(result["delivery_verified"], false);
        assert_eq!(result["wait_after"]["satisfied"], true);
        let state = call(&tool, json!({"action":"evaluate","tab":"hover","script":"({moves,clicks})"})).await;
        assert_eq!(state["result"]["clicks"], 0);
        let moves = state["result"]["moves"].as_array().unwrap();
        assert!(!moves.is_empty());
        assert!(moves.iter().all(|event| event["trusted"] == true && event["buttons"] == 0 && event["shift"] == true));
        call(&tool, json!({"action":"stop"})).await;
    });
    event("hover-live", "pass");
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

#[test]
#[ignore = "requires sandbox-capable Chromium; select explicitly in the DSR browser lane"]
fn live_hover_and_html_drag_use_native_events_then_verify_the_application_result() {
    event("native-gestures", "start");
    let dir = tempfile::tempdir().unwrap();
    let tool = live_tool(dir.path());
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    runtime.block_on(async {
        call(&tool, json!({"action":"open","tab":"board","url":"about:blank"})).await;
        call(&tool, json!({"action":"evaluate","tab":"board","script":r#"
            (()=>{
                document.body.innerHTML='<style>#source,#target{position:absolute;width:100px;height:100px;top:100px}#source{left:100px}#target{left:400px}#hover{position:absolute;top:300px}#menu{display:none;position:absolute;top:350px}#hover:hover+#menu{display:block}</style><div id="source" draggable="true">Move card</div><div id="target" data-testid="drop-zone">Drop here</div><button id="hover">Show menu</button><div id="menu">Menu ready</div><p id="status">Waiting</p>';
                window.gestureEvents=[];window.payload=null;
                for(const kind of ['dragstart','dragover','drop','dragend']) document.addEventListener(kind,e=>{
                    gestureEvents.push({kind:e.type,trusted:e.isTrusted});
                    if(e.type==='dragstart') e.dataTransfer.setData('text/plain','owned-card');
                    if(e.type==='dragover') e.preventDefault();
                    if(e.type==='drop'){e.preventDefault();window.payload=e.dataTransfer.getData('text/plain');document.querySelector('#status').textContent='Moved successfully';}
                });
                return true;
            })()
        "#})).await;
        let hovered = call(&tool, json!({"action":"hover","tab":"board","selector":"role/button[name=\"Show menu\" exact]",
            "wait_after":{"selector":"#menu","text":"Menu ready","text_match":"exact"}})).await;
        assert_eq!(hovered["dispatch_acknowledged"], true);
        assert_eq!(hovered["input_method"], "native-pointer");
        assert_eq!(hovered["wait_after"]["satisfied"], true);
        let dragged = call(&tool, json!({"action":"drag","tab":"board","selector":"text/Move card",
            "target_selector":"testid/drop-zone","drag_steps":12,"duration_ms":300,
            "wait_after":{"selector":"#status","text":"Moved successfully","text_match":"exact"}})).await;
        assert_eq!(dragged["gesture"]["press_acknowledged"], true);
        assert_eq!(dragged["gesture"]["release_acknowledged"], true);
        assert_eq!(dragged["wait_after"]["satisfied"], true);
        assert_eq!(dragged["delivery_verified"], false);
        let state = call(&tool,json!({"action":"evaluate","tab":"board","script":"({payload,gestureEvents})"})).await;
        assert_eq!(state["result"]["payload"], "owned-card");
        let events = state["result"]["gestureEvents"].as_array().unwrap();
        assert_eq!(events.iter().filter(|event|event["kind"] == "dragstart").count(), 1);
        assert_eq!(events.iter().filter(|event|event["kind"] == "drop").count(), 1);
        assert!(events.iter().all(|event|event["trusted"] == true));
        call(&tool,json!({"action":"stop"})).await;
    });
    event("native-gestures", "pass");
}

#[test]
#[ignore = "requires sandbox-capable Chromium; select explicitly in the DSR browser lane"]
fn live_replaced_drag_source_cancels_without_dropping_or_rebinding() {
    event("native-drag-replacement", "start");
    let dir = tempfile::tempdir().unwrap();
    let tool = live_tool(dir.path());
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    runtime.block_on(async {
        call(&tool,json!({"action":"open","tab":"board","url":"about:blank"})).await;
        call(&tool,json!({"action":"evaluate","tab":"board","script":r#"
            (()=>{
                document.body.innerHTML='<div id="source" draggable="true" style="position:absolute;left:100px;top:100px;width:100px;height:100px">Source</div><div id="target" style="position:absolute;left:400px;top:100px;width:100px;height:100px">Target</div>';
                window.presses=0;window.drops=0;
                document.addEventListener('pointerdown',()=>presses++);
                document.addEventListener('drop',()=>drops++);
                source.addEventListener('pointerdown',()=>source.replaceWith(source.cloneNode(true)));
                return true;
            })()
        "#})).await;
        let result = tool.execute("replace-drag",json!({"action":"drag","tab":"board","selector":"#source",
            "target_selector":"#target","duration_ms":0}),None).await.unwrap();
        assert!(result.is_error);
        let progress = result.details.unwrap();
        assert_eq!(progress["gesture"]["cancel_attempted"], true);
        assert_eq!(progress["gesture"]["release_attempted"], true);
        let state = call(&tool,json!({"action":"evaluate","tab":"board","script":"({presses,drops})"})).await;
        assert_eq!(state["result"],json!({"presses":1,"drops":0}));
        call(&tool,json!({"action":"stop"})).await;
    });
    event("native-drag-replacement", "pass");
}
