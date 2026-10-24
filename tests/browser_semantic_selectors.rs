//! Exercise semantic locators through the public browser tool, not a parallel
//! implementation. Live coverage requires explicit selection in the DSR lane.
#![forbid(unsafe_code)]
#![recursion_limit = "512"]

use pi::browser::{BrowserLaunchOptions, BrowserTool};
use pi::tools::Tool;
use serde_json::{Value, json};

fn event(case: &str, phase: &str) {
    eprintln!(
        "{}",
        json!({"suite":"browser_semantic_selectors","case":case,"phase":phase})
    );
}

#[test]
fn invalid_semantic_selectors_fail_before_connecting_or_launching() {
    event("public-preflight", "start");
    let dir = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(dir.path())
        .with_mock(false)
        .with_cdp_endpoint("invalid-endpoint");
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        for (action, selector) in [
            ("click", "role/button[name=unquoted]"),
            ("fill", "label/"),
            ("wait_for", "text/ \n "),
            ("evaluate", "text/ignored"),
            ("scroll", "role/button"),
            ("screenshot", "testid/ignored"),
        ] {
            let error = tool
                .execute(
                    "semantic-preflight",
                    json!({
                        "action":action,"selector":selector,"text":"PRIVATE-CANARY",
                    }),
                    None,
                )
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("BROWSER_SELECTOR_USAGE"), "{error}");
            assert!(!error.contains("PRIVATE-CANARY"));
            assert!(!error.contains("CDP endpoint"));
        }
    });
    event("public-preflight", "pass");
}

#[test]
fn mock_browser_cannot_claim_a_semantic_match() {
    event("mock-refusal", "start");
    let dir = tempfile::tempdir().unwrap();
    let tool = BrowserTool::new(dir.path()).with_mock(true);
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        for selector in ["text/Save", "label/Email", "testid/save", "role/button"] {
            assert!(
                tool.execute(
                    "semantic-mock",
                    json!({
                        "action":"click","selector":selector,
                    }),
                    None
                )
                .await
                .is_err()
            );
        }
    });
    event("mock-refusal", "pass");
}

async fn call(tool: &BrowserTool, case: &str, args: Value) -> Value {
    // Log routing metadata, never form values or uploaded file contents.
    eprintln!(
        "{}",
        json!({"suite":"browser_semantic_selectors","case":case,
        "phase":"dispatch","action":args["action"],"frameScoped":args.get("frame").is_some()})
    );
    let result = tool.execute(case, args, None).await.unwrap();
    assert!(!result.is_error);
    result.details.unwrap()
}

#[test]
#[ignore = "requires sandbox-capable Chromium; select explicitly in the DSR browser lane"]
fn live_semantic_locators_drive_native_input_forms_uploads_and_frame_scopes() {
    event("live-public-workflow", "start");
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("upload.txt"), "fixture contents").unwrap();
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
        call(&tool, "open", json!({"action":"open","tab":"semantic","url":"about:blank"})).await;
        call(&tool, "setup", json!({"action":"evaluate","tab":"semantic","script":r#"
            (()=>{
                document.body.innerHTML='<button id="save"><span>Save changes</span></button><button>Save draft</button><label for="email">Email address</label><input id="email" placeholder="Your email"><input id="agreement" type="checkbox" aria-label="Agree to terms"><select id="color" aria-label="Color"><option value="red">Red</option><option value="blue">Blue</option></select><input id="upload" type="file" data-testid="upload-control"><div id="shadow"></div>';
                window.clicks=[];save.addEventListener('click',e=>clicks.push(e.isTrusted));
                shadow.attachShadow({mode:'open'}).innerHTML='<button id="shadow-button" data-testid="shadow-save">Shadow action</button>';
                return true;
            })()
        "#})).await;
        call(&tool, "text-click", json!({"action":"click","tab":"semantic","selector":"text/Save changes"})).await;
        call(&tool, "role-click", json!({"action":"click","tab":"semantic","selector":r#"role/button[name="Save changes" exact]"#})).await;
        assert!(tool.execute("ambiguous", json!({"action":"click","tab":"semantic","selector":"text/Save"}), None).await.is_err());
        let observed = call(&tool, "trusted-clicks", json!({"action":"evaluate","tab":"semantic","script":"clicks"})).await;
        assert_eq!(observed["result"], json!([true,true]), "ambiguous lookup must not dispatch input");
        call(&tool, "label-fill", json!({"action":"fill","tab":"semantic","selector":"label/Email address","text":"first@example.invalid"})).await;
        call(&tool, "placeholder-fill", json!({"action":"fill","tab":"semantic","selector":"placeholder/Your email","text":"second@example.invalid"})).await;
        let observed = call(&tool, "filled-value", json!({"action":"evaluate","tab":"semantic","script":"email.value"})).await;
        assert_eq!(observed["result"], "second@example.invalid");
        call(&tool, "checked-label", json!({"action":"set_checked","tab":"semantic","selector":"label/Agree to terms","checked":true})).await;
        call(&tool, "selected-label", json!({"action":"select_option","tab":"semantic","selector":"label/Color","options":[{"value":"blue"}]})).await;
        call(&tool, "semantic-upload", json!({"action":"upload","tab":"semantic","selector":"testid/upload-control","files":["upload.txt"]})).await;
        let observed = call(&tool, "form-effects", json!({"action":"evaluate","tab":"semantic","script":"[agreement.checked,color.value,upload.files[0].name]"})).await;
        assert_eq!(observed["result"], json!([true,"blue","upload.txt"]));
        call(&tool, "shadow-testid", json!({"action":"click","tab":"semantic","selector":"testid/shadow-save"})).await;
        call(&tool, "frame-setup", json!({"action":"evaluate","tab":"semantic","script":r#"
            new Promise(resolve=>{
                const f=document.createElement('iframe');f.name='semantic-child';
                f.srcdoc='<input id="child" aria-label="Email address"><button>Save changes</button>';
                f.onload=()=>resolve(true);document.body.append(f);
            })
        "#})).await;
        let frames = call(&tool, "frame-list", json!({"action":"list_frames","tab":"semantic"})).await;
        let frame = frames["frames"].as_array().unwrap().iter()
            .find(|value| value["name"] == "semantic-child").unwrap()["frame_id"].clone();
        call(&tool, "frame-label", json!({"action":"fill","tab":"semantic","frame":frame,
            "selector":"label/Email address","text":"child-only"})).await;
        assert!(tool.execute("no-parent-fallback", json!({"action":"click","tab":"semantic","frame":frame,
            "selector":"testid/shadow-save"}), None).await.is_err());
        let child = call(&tool, "frame-value", json!({"action":"evaluate","tab":"semantic","frame":frame,
            "script":"document.querySelector('input').value"})).await;
        assert_eq!(child["result"], "child-only");
        let parent = call(&tool, "parent-value", json!({"action":"evaluate","tab":"semantic","script":"email.value"})).await;
        assert_eq!(parent["result"], "second@example.invalid");
        call(&tool, "delayed-target", json!({"action":"evaluate","tab":"semantic","script":
            "setTimeout(()=>{const b=document.createElement('button');b.textContent='Ready now';document.body.append(b)},100);true"
        })).await;
        call(&tool, "wait-semantic", json!({"action":"wait_for","tab":"semantic","selector":"text/Ready now","timeout_ms":5000})).await;
        call(&tool, "stop", json!({"action":"stop"})).await;
    });
    event("live-public-workflow", "pass");
}
