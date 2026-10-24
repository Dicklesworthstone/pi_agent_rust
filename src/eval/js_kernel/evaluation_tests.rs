use super::{JsCellResponse, JsKernel};
use std::time::{Duration, Instant};

fn cell(kernel: &JsKernel, code: &str) -> JsCellResponse {
    kernel
        .submit(code.to_string(), Instant::now() + Duration::from_secs(5))
        .unwrap()
        .recv_timeout(Duration::from_secs(10))
        .expect("kernel must answer the cell")
}

fn succeeds(kernel: &JsKernel, code: &str) -> JsCellResponse {
    let response = cell(kernel, code);
    assert!(response.ok, "cell failed: {:?}", response.error);
    response
}

#[test]
fn runtime_syntax_errors_do_not_execute_a_cell_twice() {
    for failure in [
        "throw new SyntaxError('runtime marker')",
        "JSON.parse('{')",
        "new RegExp('[')",
        "eval('const =')",
    ] {
        let kernel = JsKernel::spawn().unwrap();
        succeeds(&kernel, "globalThis.executions = 0");
        let response = cell(
            &kernel,
            &format!("globalThis.executions++; console.log('once'); {failure}"),
        );
        assert!(!response.ok, "runtime failure must not become success");
        assert_eq!(response.console, "once\n");
        assert_eq!(succeeds(&kernel, "executions").result.as_deref(), Some("1"));
    }
}

#[test]
fn runtime_syntax_error_cannot_repeat_a_host_tool_call() {
    let kernel = JsKernel::spawn().unwrap();
    let response = kernel
        .submit(
            "tool.read('one.txt'); throw new SyntaxError('after bridge')".into(),
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
    let request = kernel.bridge.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(request.tool, "read");
    assert_eq!(request.input["path"], "one.txt");
    request.reply.send(Ok("content".into())).unwrap();
    let outcome = response.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(!outcome.ok);
    assert!(
        kernel.bridge.try_recv().is_err(),
        "bridge must be called once"
    );
}

#[test]
fn parse_errors_execute_no_prefix_statements() {
    let kernel = JsKernel::spawn().unwrap();
    succeeds(&kernel, "globalThis.parseCount = 0");
    for code in [
        "parseCount++; const = 1",
        "parseCount++; await Promise.resolve(1); const = 1",
    ] {
        assert!(!cell(&kernel, code).ok);
        assert_eq!(succeeds(&kernel, "parseCount").result.as_deref(), Some("0"));
    }
}

#[test]
fn script_lexical_bindings_keep_their_persistent_scope() {
    let kernel = JsKernel::spawn().unwrap();
    succeeds(
        &kernel,
        "let accumulated = 2; const factor = 3; function scale(x) { return x * factor; }",
    );
    assert_eq!(
        succeeds(&kernel, "accumulated += scale(4)")
            .result
            .as_deref(),
        Some("14")
    );
    assert!(!cell(&kernel, "let accumulated = 99").ok);
    assert_eq!(
        succeeds(&kernel, "accumulated").result.as_deref(),
        Some("14")
    );
}

#[test]
fn top_level_await_still_executes_once_and_retains_explicit_globals() {
    let kernel = JsKernel::spawn().unwrap();
    succeeds(&kernel, "globalThis.asyncCount = 0");
    succeeds(
        &kernel,
        "globalThis.asyncCount++; globalThis.answer = await Promise.resolve(42)",
    );
    assert_eq!(
        succeeds(&kernel, "[asyncCount, answer]").result.as_deref(),
        Some("[1,42]")
    );
    assert!(
        !cell(
            &kernel,
            "asyncCount++; await Promise.reject(new SyntaxError('async failure'))"
        )
        .ok
    );
    assert_eq!(succeeds(&kernel, "asyncCount").result.as_deref(), Some("2"));
}

#[test]
fn replacing_function_global_does_not_replace_the_parse_probe() {
    let kernel = JsKernel::spawn().unwrap();
    succeeds(
        &kernel,
        "globalThis.probes = 0; globalThis.Function = () => { probes++; throw new Error('not a parser'); }",
    );
    assert_eq!(succeeds(&kernel, "6 * 7").result.as_deref(), Some("42"));
    succeeds(&kernel, "await Promise.resolve(1)");
    assert_eq!(succeeds(&kernel, "probes").result.as_deref(), Some("0"));
}

#[test]
fn async_looking_literals_and_nested_functions_remain_scripts() {
    let kernel = JsKernel::spawn().unwrap();
    succeeds(
        &kernel,
        "const spelling = 'await tool.read(x)'; let tick = 0; async function next() { return ++tick; }",
    );
    succeeds(&kernel, "/* await */ next()");
    assert_eq!(
        succeeds(&kernel, "[spelling, tick]").result.as_deref(),
        Some("[\"await tool.read(x)\",1]")
    );
}

#[test]
fn ordinary_and_promise_objects_named_value_are_not_unwrapped() {
    let kernel = JsKernel::spawn().unwrap();
    for code in [
        "({value: 7, keep: true})",
        "Promise.resolve({value: 7, keep: true})",
    ] {
        let result = succeeds(&kernel, code).result.unwrap();
        let object: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(object, serde_json::json!({"value":7,"keep":true}));
    }
}
