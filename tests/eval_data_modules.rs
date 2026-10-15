//! Workspace data imports through the real EvalTool and conversation scope.
#![recursion_limit = "256"]

use pi::eval::EvalTool;
use pi::model::ContentBlock;
use pi::tools::{Tool, ToolOutput, ToolRegistry};
use serde_json::json;
use std::sync::Arc;

fn run<F: std::future::Future>(future: F) -> F::Output {
    asupersync::runtime::RuntimeBuilder::current_thread().blocking_threads(1, 8)
        .build().unwrap().block_on(future)
}

fn text(output: &ToolOutput) -> String {
    output.content.iter().filter_map(|block| match block {
        ContentBlock::Text(value) => Some(value.text.as_str()), _ => None,
    }).collect::<Vec<_>>().join("\n")
}

#[test]
fn retained_json_modules_follow_conversation_ownership_and_explicit_reset() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("data.json"), "{\"answer\":1}").unwrap();
    let registry = ToolRegistry::from_tools(vec![Box::new(EvalTool::new(dir.path()))]);
    let scope = registry.job_session_scope();
    let select = |name: &str| {
        let name = name.to_string();
        scope.bind(Arc::new(move || {
            let name = name.clone();
            Box::pin(async move { Some(name) })
        }));
    };
    let tool = registry.get("eval").unwrap();
    run(async {
        select("data-owner-a");
        let a = tool.execute("a", json!({"kernel":"js","code":
            "globalThis.data = (await import('./data.json', {with:{type:'json'}})).default; data.answer = 42"
        }), None).await.unwrap();
        assert!(!a.is_error, "{}", text(&a));
        select("data-owner-b");
        let b = tool.execute("b", json!({"kernel":"js","code":
            "(await import('./data.json', {with:{type:'json'}})).default.answer"
        }), None).await.unwrap();
        assert!(!b.is_error, "{}", text(&b));
        assert_eq!(text(&b).trim(), "1");
        select("data-owner-a");
        let restored = tool.execute("restore", json!({"kernel":"js","code":"data.answer"}), None).await.unwrap();
        assert!(!restored.is_error);
        assert_eq!(text(&restored).trim(), "42");
        std::fs::write(dir.path().join("data.json"), "{\"answer\":99}").unwrap();
        let reset = tool.execute("reset", json!({"kernel":"js","reset":true,"code":
            "(await import('./data.json', {with:{type:'json'}})).default.answer"
        }), None).await.unwrap();
        assert!(!reset.is_error, "{}", text(&reset));
        assert_eq!(text(&reset).trim(), "99");
    });
}

#[test]
fn imported_json_can_be_analyzed_and_streamed_as_native_structured_display() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("rows.json"), "[{\"n\":17},{\"n\":25}]").unwrap();
    let tool = EvalTool::new(dir.path());
    let output = run(tool.execute("analyze", json!({"kernel":"js","code":
        "const rows = (await import('./rows.json', {with:{type:'json'}})).default; display({sum: rows.reduce((sum, row) => sum + row.n, 0)}); rows.length"
    }), None)).unwrap();
    assert!(!output.is_error, "{}", text(&output));
    assert_eq!(output.details.as_ref().unwrap()["jsonOutputs"], json!([{"sum":42}]));
    assert!(text(&output).contains('2'));
}

#[test]
fn typed_analysis_composes_json_and_text_assets_without_another_tool_call() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("analysis")).unwrap();
    std::fs::write(dir.path().join("rows.json"), "[{\"n\":17},{\"n\":25}]").unwrap();
    std::fs::write(dir.path().join("analysis/template.md"), "# Total\r\n{{total}}\n").unwrap();
    std::fs::write(dir.path().join("analysis/report.mts"), r#"
        import rows from '../rows.json' with {type:'json'};
        import template from './template.md' with {type:'text'};
        export const total: number = rows.reduce((sum: number, row: {n: number}) => sum + row.n, 0);
        export const report: string = template.replace('{{total}}', String(total));
    "#).unwrap();
    let tool = EvalTool::new(dir.path());
    let out = run(tool.execute("report", json!({"kernel":"js","code":
        "const report = await import('./analysis/report.mts'); display({total:report.total, report:report.report}); report.total"
    }), None)).unwrap();
    assert!(!out.is_error, "{}", text(&out));
    assert_eq!(out.details.as_ref().unwrap()["jsonOutputs"], json!([{"total":42,"report":"# Total\r\n42\n"}]));
}
