use super::*;
use crate::eval::js_kernel::{JsCellResponse, JsKernel};
use std::time::{Duration, Instant};

fn cell(kernel: &JsKernel, code: &str) -> JsCellResponse {
    kernel
        .submit(code.into(), Instant::now() + Duration::from_secs(5))
        .unwrap()
        .recv_timeout(Duration::from_secs(10))
        .expect("import cell terminates")
}

fn succeeds(kernel: &JsKernel, code: &str) -> JsCellResponse {
    let result = cell(kernel, code);
    assert!(result.ok, "{code}: {:?}", result.error);
    result
}

#[test]
fn ordinary_code_dynamic_imports_comments_and_unparseable_cells_are_untouched() {
    for code in [
        "const text = 'import thing from x'; text",
        "// import x from 'y'\n42",
        "await import('./value.mjs')",
        "const re = /import\\s+/; re.test('import ')",
        "globalThis.prefix = true; import value from './value.mjs'; const = ;",
    ] {
        assert!(prepare(code).unwrap().is_none(), "{code}");
    }
}

#[test]
fn hoisting_keeps_directives_shebangs_body_bytes_and_newline_locations() {
    for newline in ["\n", "\r\n", "\r"] {
        let code = format!(
            "#! /usr/bin/env pi{newline}'use strict';{newline}globalThis.marker = value;{newline}import {{value}} from './value.mjs';{newline}marker"
        );
        let prepared = prepare(&code).unwrap().unwrap();
        assert!(prepared.starts_with(&format!("#! /usr/bin/env pi{newline}'use strict';;const")));
        assert!(
            prepared.find("await import(").unwrap() < prepared.find("globalThis.marker").unwrap()
        );
        assert!(prepared.contains("globalThis.marker = value;"));
        assert_eq!(
            prepared.bytes().filter(|b| *b == b'\n').count(),
            code.bytes().filter(|b| *b == b'\n').count()
        );
        assert_eq!(
            prepared.bytes().filter(|b| *b == b'\r').count(),
            code.bytes().filter(|b| *b == b'\r').count()
        );
        assert!(prepared.ends_with("marker"));
    }
}

#[test]
fn all_import_forms_lower_with_typed_attributes_and_no_global_helper_bindings() {
    let prepared = prepare(
        r#"
        import defaultValue, * as all from './one.mjs';
        import {value as renamed, "hyphen-name" as special} from './two.mjs';
        import rows from './rows.json' with {type:'json'};
        import template from './template.md' with {'type':'text'};
        import './side.mjs';
        [defaultValue, all.value, renamed, special, rows, template]
    "#,
    )
    .unwrap()
    .unwrap();
    assert!(prepared.contains("[\"defaultValue\"]:m0[\"default\"]"));
    assert!(prepared.contains("[\"all\"]:m0"));
    assert!(prepared.contains("m1[\"hyphen-name\"]"));
    assert!(prepared.contains("import(\"./rows.json\",{with:{type:\"json\"}})"));
    assert!(prepared.contains("import(\"./template.md\",{with:{type:\"text\"}})"));
    assert!(prepared.contains("const m4=await import(\"./side.mjs\")"));
    assert!(prepared.contains("await(async()=>{const m0="));
}

#[test]
fn invalid_module_only_syntax_and_attributes_fail_before_any_evaluation() {
    for code in [
        "import x from './one.mjs'; export const y=1;",
        "import './one.mjs'; function f(){return import.meta.url}",
        "import x from './rows.json' with {type:'json', extra:'value'};",
        "import x from './rows.json' with {type:'PRIVATE-CANARY'};",
    ] {
        let error = prepare(code).unwrap_err();
        assert!(error.starts_with("EVAL_IMPORT"));
        assert!(!error.contains("PRIVATE-CANARY"));
    }
}

#[test]
fn input_import_and_binding_limits_are_bounded() {
    assert!(prepare(&"x".repeat(MAX_SOURCE_BYTES + 1)).is_err());
    let code = "import './one.mjs';".repeat(MAX_IMPORTS + 1);
    assert_eq!(prepare(&code).unwrap_err(), LIMIT);
    let bindings = (0..=MAX_BINDINGS)
        .map(|n| format!("value as v{n}"))
        .collect::<Vec<_>>()
        .join(",");
    assert_eq!(
        prepare(&format!("import {{{bindings}}} from './one.mjs';")).unwrap_err(),
        LIMIT
    );
    let mut output = "x".repeat(MAX_OUTPUT_BYTES - 1);
    append(&mut output, "y").unwrap();
    assert_eq!(append(&mut output, "z").unwrap_err(), LIMIT);
    assert_eq!(output.len(), MAX_OUTPUT_BYTES);
}

#[test]
fn native_kernel_retains_bindings_with_snapshot_and_namespace_semantics() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("counter.mjs"),
        "globalThis.loads=(globalThis.loads||0)+1; export let count=40; export function bump(){return ++count}; export default 7;").unwrap();
    let kernel = JsKernel::spawn_in(dir.path()).unwrap();
    let result = succeeds(
        &kernel,
        "import initial, {count as snapshot, bump} from './counter.mjs'; import * as counter from './counter.mjs'; bump(); [initial,snapshot,counter.count,loads]",
    );
    assert_eq!(result.result.as_deref(), Some("[7,40,41,1]"));
    let result = succeeds(&kernel, "bump(); [snapshot,counter.count,typeof m0,loads]");
    assert_eq!(result.result.as_deref(), Some("[40,42,\"undefined\",1]"));
    assert!(!cell(&kernel, "snapshot=99").ok);
    assert_eq!(succeeds(&kernel, "snapshot").result.as_deref(), Some("40"));
}

#[test]
fn dependencies_are_hoisted_and_awaited_before_body_statements() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("first.mjs"),
        "globalThis.order='first'; await sleep(1); export const answer=42;",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("second.mjs"),
        "globalThis.order+=' second';",
    )
    .unwrap();
    let kernel = JsKernel::spawn_in(dir.path()).unwrap();
    let result = succeeds(
        &kernel,
        "globalThis.seen=answer; import {answer} from './first.mjs'; import './second.mjs'; [seen,order]",
    );
    assert_eq!(result.result.as_deref(), Some("[42,\"first second\"]"));
}

#[test]
fn runtime_failures_are_never_retried_and_missing_exports_never_become_undefined() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("value.mjs"),
        "globalThis.loads=(globalThis.loads||0)+1; export const answer=42;",
    )
    .unwrap();
    let kernel = JsKernel::spawn_in(dir.path()).unwrap();
    let failed = cell(
        &kernel,
        "import {missing} from './value.mjs'; globalThis.bodyRan=true",
    );
    assert!(!failed.ok);
    assert!(failed.error.unwrap().contains("EVAL_IMPORT_EXPORT"));
    assert_eq!(
        succeeds(&kernel, "[loads,typeof bodyRan]")
            .result
            .as_deref(),
        Some("[1,\"undefined\"]")
    );
    let failed = cell(
        &kernel,
        "import {answer} from './value.mjs'; globalThis.executions=(globalThis.executions||0)+1; throw new SyntaxError('runtime only')",
    );
    assert!(!failed.ok);
    assert_eq!(
        succeeds(&kernel, "[loads,executions]").result.as_deref(),
        Some("[1,1]")
    );
}

#[test]
fn rejected_syntax_cannot_execute_a_prefix_or_a_dependency() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("effect.mjs"),
        "globalThis.importRan=true; export default 1;",
    )
    .unwrap();
    let kernel = JsKernel::spawn_in(dir.path()).unwrap();
    for code in [
        "globalThis.bodyRan=true; import x from './effect.mjs'; export const y=1;",
        "globalThis.bodyRan=true; import x from './effect.mjs'; const = ;",
        "import x from './effect.mjs'; let x; globalThis.bodyRan=true;",
    ] {
        assert!(!cell(&kernel, code).ok);
    }
    assert_eq!(
        succeeds(&kernel, "[typeof importRan,typeof bodyRan]")
            .result
            .as_deref(),
        Some("[\"undefined\",\"undefined\"]")
    );
}

#[test]
fn native_loader_still_refuses_remote_and_out_of_workspace_imports() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(
        dir.path().join("outside.mjs"),
        "globalThis.escaped=true; export default 42;",
    )
    .unwrap();
    let kernel = JsKernel::spawn_in(&project).unwrap();
    for (index, path) in [
        "../outside.mjs",
        "https://example.test/code.mjs",
        "node:fs",
        "a-package",
    ]
    .iter()
    .enumerate()
    {
        let code = format!(
            "import denied{index} from {};",
            serde_json::to_string(path).unwrap()
        );
        assert!(!cell(&kernel, &code).ok);
    }
    assert_eq!(
        succeeds(&kernel, "typeof escaped").result.as_deref(),
        Some("\"undefined\"")
    );
}

#[test]
fn imported_then_alias_does_not_intercept_internal_promise_fulfillment() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("functions.mjs"),
        "export function callback(){globalThis.calls=(globalThis.calls||0)+1; return 42;}",
    )
    .unwrap();
    let kernel = JsKernel::spawn_in(dir.path()).unwrap();
    let result = succeeds(
        &kernel,
        "import {callback as then} from './functions.mjs'; [typeof calls,then()]",
    );
    assert_eq!(result.result.as_deref(), Some("[\"undefined\",42]"));
    assert_eq!(succeeds(&kernel, "calls").result.as_deref(), Some("1"));
}

#[test]
fn guest_prototype_hooks_do_not_run_during_binding_transfer() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("value.mjs"), "export const value=42;").unwrap();
    let kernel = JsKernel::spawn_in(dir.path()).unwrap();
    succeeds(
        &kernel,
        "Object.prototype.then=function(){globalThis.hookRan=true; throw 'unexpected thenable'}; 1",
    );
    let imported = cell(&kernel, "import {value} from './value.mjs'; value");
    // Restore before reporting any failure so a prototype hook cannot mask it.
    succeeds(&kernel, "delete Object.prototype.then; 1");
    assert!(imported.ok, "{:?}", imported.error);
    assert_eq!(imported.result.as_deref(), Some("42"));
    assert_eq!(
        succeeds(&kernel, "typeof hookRan").result.as_deref(),
        Some("\"undefined\"")
    );
}
