#![recursion_limit = "256"]

//! Imported Axios must reach the capability-gated HTTP host, never echo a
//! fabricated success. These fixtures load actual extensions and serve HTTP.

mod common;

use common::harness::MockHttpResponse;
use pi::extensions::{
    ExtensionManager, ExtensionPolicy, ExtensionPolicyMode, JsExtensionLoadSpec,
    JsExtensionRuntimeHandle,
};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::tools::ToolRegistry;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

async fn load(harness: &common::TestHarness, source: &str, allow_http: bool) -> ExtensionManager {
    let entry = harness.create_file("axios-extension.mjs", source.as_bytes());
    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], harness.temp_dir(), None));
    let mut policy = ExtensionPolicy {
        mode: ExtensionPolicyMode::Strict,
        ..ExtensionPolicy::default()
    };
    if allow_http {
        policy.deny_caps.retain(|capability| capability != "http");
        policy.default_caps.push("http".to_string());
    } else {
        policy.deny_caps.push("http".to_string());
    }
    let runtime = JsExtensionRuntimeHandle::start_with_policy(
        PiJsRuntimeConfig {
            cwd: harness.temp_dir().display().to_string(),
            disk_cache_dir: None,
            ..Default::default()
        },
        tools,
        manager.clone(),
        policy,
    )
    .await
    .expect("native extension runtime");
    manager.set_js_runtime(runtime);
    manager
        .load_js_extensions(vec![
            JsExtensionLoadSpec::from_entry_path(entry).expect("extension spec"),
        ])
        .await
        .expect("load Axios extension");
    manager
}

fn header<'a>(request: &'a common::harness::MockHttpRequest, name: &str) -> &'a str {
    request
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map_or("", |(_, value)| value.as_str())
}

#[test]
fn axios_esm_and_require_send_real_json_form_and_binary_requests() {
    let harness = common::TestHarness::new("axios_real_http");
    let server = harness.start_mock_http_server();
    server.add_route(
        "POST",
        "/api/items",
        MockHttpResponse::json(201, &json!({"saved": 17})),
    );
    server.add_route("POST", "/api/form", MockHttpResponse::text(200, "accepted"));
    server.add_route("GET", "/api/query", MockHttpResponse::text(200, "queried"));
    server.add_route(
        "GET",
        "/api/binary",
        MockHttpResponse {
            status: 200,
            headers: vec![(
                "Content-Type".to_string(),
                "application/octet-stream".to_string(),
            )],
            body: vec![0, 255, 1, 128],
        },
    );
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    let value = runtime.block_on(async {
        let manager = load(&harness, r"
            import axios, { isAxiosError } from 'axios';
            import { createRequire } from 'node:module';
            export default function(pi) {
              pi.registerCommand('axios-probe', {handler: async base => {
                const required = createRequire(import.meta.url)('axios');
                const order = [];
                const client = axios.create({baseURL: base + '/api',
                  headers: {common: {'X-Default': 'kept'}}});
                client.interceptors.request.use(config => {
                  order.push('first'); config.headers.set('X-Order', order.join(',')); return config;
                });
                client.interceptors.request.use(config => { order.push('second'); return config; });
                const responseId = client.interceptors.response.use(response => {
                  order.push('response'); return response;
                });
                const result = await client.post('/items', {name:'λ', count:2},
                  {headers:{'X-Request':'kept'}, params:{q:'two words', tags:['a','b']}});
                client.interceptors.response.eject(responseId);
                const form = await required.post(base + '/api/form',
                  {grant_type:'token exchange', scopes:['read','write']},
                  {headers:{'Content-Type':'application/x-www-form-urlencoded'}});
                const binary = await required.get(base + '/api/binary', {responseType:'arraybuffer'});
                const query = await axios.create({baseURL:base + '/api', params:{lang:'en'}})
                  .get('/query', {params:new URLSearchParams('q=rust')});
                return {same: required === axios, status:result.status, data:result.data,
                  defaultHeader:result.config.headers.get('x-default'), order,
                  form:form.data, bytes:Array.from(new Uint8Array(binary.data)),
                  ordinaryIsError:isAxiosError({}), query:query.data};
              }});
            }
        ", true).await;
        let value = manager.execute_command("axios-probe", &server.base_url(), 10_000)
            .await.expect("real Axios requests");
        assert!(manager.shutdown(Duration::from_secs(3)).await);
        value
    });
    assert_eq!(value["same"], true);
    assert_eq!(value["status"], 201);
    assert_eq!(value["data"], json!({"saved": 17}));
    assert_eq!(value["defaultHeader"], "kept");
    assert_eq!(value["order"], json!(["second", "first", "response"]));
    assert_eq!(value["form"], "accepted");
    assert_eq!(value["bytes"], json!([0, 255, 1, 128]));
    assert_eq!(value["ordinaryIsError"], false);
    assert_eq!(value["query"], "queried");
    let requests = server.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[3].path, "/api/query?q=rust");
    assert_eq!(requests[0].method, "POST");
    let url = url::Url::parse(&format!("{}{}", server.base_url(), requests[0].path)).unwrap();
    assert_eq!(url.path(), "/api/items");
    assert_eq!(
        url.query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect::<Vec<_>>(),
        vec![
            ("q".to_string(), "two words".to_string()),
            ("tags[]".to_string(), "a".to_string()),
            ("tags[]".to_string(), "b".to_string())
        ]
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&requests[0].body).unwrap(),
        json!({"name":"λ","count":2})
    );
    assert_eq!(header(&requests[0], "content-type"), "application/json");
    assert_eq!(header(&requests[0], "x-default"), "kept");
    assert_eq!(header(&requests[0], "x-request"), "kept");
    assert_eq!(header(&requests[0], "x-order"), "second,first");
    let form = url::form_urlencoded::parse(&requests[1].body)
        .into_owned()
        .collect::<Vec<_>>();
    assert_eq!(
        form,
        vec![
            ("grant_type".to_string(), "token exchange".to_string()),
            ("scopes[]".to_string(), "read".to_string()),
            ("scopes[]".to_string(), "write".to_string())
        ]
    );
}

#[test]
fn axios_errors_limits_and_pre_cancel_never_fabricate_success() {
    let harness = common::TestHarness::new("axios_errors");
    let server = harness.start_mock_http_server();
    server.add_route(
        "GET",
        "/missing",
        MockHttpResponse::json(404, &json!({"missing":true})),
    );
    server.add_route("GET", "/long", MockHttpResponse::text(200, "too long"));
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    let result = runtime.block_on(async {
        #[allow(clippy::literal_string_with_formatting_args)] // JS object literal, not a format string
        let manager = load(&harness, r"
            import axios from 'axios';
            export default function(pi) {
              pi.registerCommand('axios-errors', {handler: async base => {
                const errors = [];
                const record = async work => {
                  try { await work(); errors.push({unexpectedSuccess:true}); }
                  catch (error) { errors.push({axios:axios.isAxiosError(error), code:error.code,
                    status:error.response?.status, data:error.response?.data, cancel:axios.isCancel(error)}); }
                };
                await record(() => axios.get(base + '/missing'));
                const accepted = await axios.get(base + '/missing', {validateStatus:() => true});
                await record(() => axios.get(base + '/long', {maxContentLength:3}));
                const controller = new AbortController(); controller.abort();
                await record(() => axios.get(base + '/must-not-send', {signal:controller.signal}));
                const source = axios.CancelToken.source(); source.cancel('stopped');
                await record(() => axios.get(base + '/must-not-send', {cancelToken:source.token}));
                await record(() => axios.post(base + '/must-not-send', 'oversized', {maxBodyLength:2}));
                await record(() => axios.get(base + '/must-not-send', {httpAgent:{}}));
                return {errors, accepted:accepted.status};
              }});
            }
        ", true).await;
        let result = manager.execute_command("axios-errors", &server.base_url(), 10_000)
            .await.expect("Axios error handling");
        assert!(manager.shutdown(Duration::from_secs(3)).await);
        result
    });
    assert_eq!(result["accepted"], 404);
    let errors = result["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 6);
    assert!(errors.iter().all(|error| error["axios"] == true));
    assert_eq!(errors[0]["code"], "ERR_BAD_REQUEST");
    assert_eq!(errors[0]["status"], 404);
    assert_eq!(errors[0]["data"], json!({"missing":true}));
    assert_eq!(errors[1]["code"], "ERR_BAD_RESPONSE");
    for error in &errors[2..4] {
        assert_eq!(error["code"], "ERR_CANCELED");
        assert_eq!(error["cancel"], true);
    }
    assert_eq!(errors[4]["code"], "ERR_BAD_REQUEST");
    assert_eq!(errors[5]["code"], "ERR_NOT_SUPPORT");
    assert_eq!(
        server.requests().len(),
        3,
        "cancelled and rejected inputs perform no HTTP"
    );
}

#[test]
fn axios_obeys_native_http_capability_denial() {
    let harness = common::TestHarness::new("axios_denied_http");
    let server = harness.start_mock_http_server();
    server.add_route(
        "GET",
        "/",
        MockHttpResponse::text(200, "must not reach server"),
    );
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    let result = runtime.block_on(async {
        #[allow(clippy::literal_string_with_formatting_args)] // JS object literal, not a format string
        let manager = load(&harness, r"
            import axios from 'axios';
            export default function(pi) {
              pi.registerCommand('axios-denied', {handler: async url => {
                try { await axios.get(url); return {unexpectedSuccess:true}; }
                catch (error) { return {axios:axios.isAxiosError(error), error:String(error.message)}; }
              }});
            }
        ", false).await;
        let result = manager.execute_command("axios-denied", &server.base_url(), 10_000)
            .await.expect("capability denial is a request error");
        assert!(manager.shutdown(Duration::from_secs(3)).await);
        result
    });
    assert_eq!(result["axios"], true);
    assert!(
        result["error"]
            .as_str()
            .is_some_and(|message| message.to_ascii_lowercase().contains("denied"))
    );
    assert!(
        server.requests().is_empty(),
        "Axios must reuse host HTTP policy"
    );
}
