//! Static imports through the public, conversation-owned retained eval tool.
#![recursion_limit = "256"]

use pi::eval::EvalTool;
use pi::model::ContentBlock;
use pi::tools::{Tool, ToolOutput, ToolRegistry};
use serde_json::json;
use std::sync::Arc;

fn run<F: std::future::Future>(future: F) -> F::Output {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .unwrap()
        .block_on(future)
}

fn text(output: &ToolOutput) -> String {
    output
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(value) => Some(value.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn static_data_imports_feed_native_displays_and_later_cells() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("rows.json"), "[{\"n\":17},{\"n\":25}]").unwrap();
    std::fs::write(dir.path().join("template.md"), "# Total\r\n{{total}}\n").unwrap();
    let tool = EvalTool::new(dir.path());
    run(async {
        let result = tool.execute("imports", json!({"kernel":"js","code":
            "import rows from './rows.json' with {type:'json'}; import template from './template.md' with {type:'text'}; const total=rows.reduce((sum,row)=>sum+row.n,0); display({total,report:template.replace('{{total}}',String(total))}); total"
        }), None).await.unwrap();
        assert!(!result.is_error, "{}", text(&result));
        assert_eq!(
            result.details.as_ref().unwrap()["jsonOutputs"],
            json!([{"total":42,"report":"# Total\r\n42\n"}])
        );
        let retained = tool
            .execute(
                "reuse",
                json!({"kernel":"js","code":"total + rows.length"}),
                None,
            )
            .await
            .unwrap();
        assert!(!retained.is_error, "{}", text(&retained));
        assert_eq!(text(&retained).trim(), "44");
    });
}

#[test]
fn imported_bindings_follow_conversation_state_and_explicit_reset() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("value.mjs"),
        "export let value=1; export function next(){return ++value}",
    )
    .unwrap();
    let registry = ToolRegistry::from_tools(vec![Box::new(EvalTool::new(dir.path()))]);
    let scope = registry.job_session_scope();
    let select = |name: &str| {
        let name = name.to_owned();
        scope.bind(Arc::new(move || {
            let name = name.clone();
            Box::pin(async move { Some(name) })
        }));
    };
    let tool = registry.get("eval").unwrap();
    run(async {
        select("import-owner-a");
        let a = tool.execute("a", json!({"kernel":"js","code":"import * as values from './value.mjs'; values.next()"}), None).await.unwrap();
        assert!(!a.is_error, "{}", text(&a));
        select("import-owner-b");
        let b = tool
            .execute(
                "b",
                json!({"kernel":"js","code":"import * as values from './value.mjs'; values.value"}),
                None,
            )
            .await
            .unwrap();
        assert!(!b.is_error, "{}", text(&b));
        // Owner b's first cell starts its own kernel.
        assert_eq!(text(&b).trim(), "(js kernel started)\n1");
        select("import-owner-a");
        let a = tool
            .execute(
                "a-again",
                json!({"kernel":"js","code":"values.value"}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(text(&a).trim(), "2");
        let reset = tool.execute("reset", json!({"kernel":"js","reset":true,"code":"import * as values from './value.mjs'; values.value"}), None).await.unwrap();
        assert!(!reset.is_error, "{}", text(&reset));
        assert_eq!(
            text(&reset).trim(),
            "EVAL_STATE_RESET: js state explicitly discarded before this cell.\n(js kernel started)\n1"
        );
    });
}

#[test]
fn percent_load_javascript_imports_keep_the_real_importer_directory() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("analysis")).unwrap();
    std::fs::write(
        dir.path().join("analysis/value.mjs"),
        "export const value=42;",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("analysis/setup.js"),
        "import {value} from './value.mjs'; globalThis.report=()=>value; report()",
    )
    .unwrap();
    let tool = EvalTool::new(dir.path());
    run(async {
        let loaded = tool
            .execute(
                "load",
                json!({"kernel":"js","code":"%load analysis/setup.js"}),
                None,
            )
            .await
            .unwrap();
        assert!(!loaded.is_error, "{}", text(&loaded));
        assert_eq!(text(&loaded).trim(), "(js kernel started)\n42");
        let reused = tool
            .execute("reuse", json!({"kernel":"js","code":"report()"}), None)
            .await
            .unwrap();
        assert_eq!(text(&reused).trim(), "42");
    });
}

#[test]
#[allow(clippy::literal_string_with_formatting_args)] // TypeScript source, not format args
fn typed_setup_files_compose_static_code_and_data_imports_in_the_retained_realm() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("analysis")).unwrap();
    std::fs::write(
        dir.path().join("analysis/rows.json"),
        "[{\"n\":17},{\"n\":25}]",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("analysis/template.md"),
        "# Result\n{{total}}\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("analysis/math.ts"),
        "globalThis.helperLoads=(globalThis.helperLoads||0)+1; export function sum(rows: {n:number}[]): number {return rows.reduce((n,row)=>n+row.n,0)}").unwrap();
    std::fs::write(dir.path().join("analysis/setup.ts"), r"
        import rows from './rows.json' with {type:'json'};
        import template from './template.md' with {type:'text'};
        import {sum} from './math.ts';
        import type {NotNeededAtRuntime} from './never-created.ts';
        interface Report { total: number; text: string }
        await sleep(1);
        const report: Report = {total:sum(rows),text:template.replace('{{total}}',String(sum(rows)))};
        display(report);
        report.total
    ").unwrap();
    let tool = EvalTool::new(dir.path());
    run(async {
        let loaded = tool
            .execute(
                "typed-load",
                json!({"kernel":"js","code":"%load analysis/setup.ts"}),
                None,
            )
            .await
            .unwrap();
        assert!(!loaded.is_error, "{}", text(&loaded));
        assert_eq!(
            loaded.details.as_ref().unwrap()["jsonOutputs"],
            json!([{"total":42,"text":"# Result\n42\n"}])
        );
        let reused = tool
            .execute(
                "typed-reuse",
                json!({"kernel":"js","code":"[report.total,sum(rows),helperLoads]"}),
                None,
            )
            .await
            .unwrap();
        assert!(!reused.is_error, "{}", text(&reused));
        assert_eq!(text(&reused).trim(), "[42,42,1]");
    });
}

#[test]
fn malformed_loaded_typescript_executes_neither_prefix_nor_imports() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("effect.ts"),
        "globalThis.dependencyRan=true;",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("bad.ts"),
        "globalThis.bodyRan=true; import './effect.ts'; const bad: = 42;",
    )
    .unwrap();
    let tool = EvalTool::new(dir.path());
    run(async {
        let rejected = tool
            .execute(
                "bad-load",
                json!({"kernel":"js","code":"%load bad.ts"}),
                None,
            )
            .await;
        assert!(rejected.as_ref().map_or(true, |output| output.is_error));
        let probe = tool
            .execute(
                "after-bad-load",
                json!({"kernel":"js","code":"[typeof bodyRan,typeof dependencyRan]"}),
                None,
            )
            .await
            .unwrap();
        assert!(!probe.is_error, "{}", text(&probe));
        // The refused load ran no cell, so the probe is the kernel's first.
        assert_eq!(
            text(&probe).trim(),
            "(js kernel started)\n[\"undefined\",\"undefined\"]"
        );
    });
}
