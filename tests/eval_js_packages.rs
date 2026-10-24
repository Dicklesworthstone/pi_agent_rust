//! Installed ESM dependencies through the public retained eval tool.
use pi::eval::EvalTool;
use pi::model::ContentBlock;
use pi::tools::{Tool, ToolOutput};
use serde_json::json;

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
fn package_analysis_uses_existing_displays_and_explicit_reset() {
    let dir = tempfile::tempdir().unwrap();
    let package = dir.path().join("node_modules/analysis-kit");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(
        package.join("package.json"),
        r#"{"type":"module","exports":"./index.js"}"#,
    )
    .unwrap();
    std::fs::write(package.join("index.js"), "export const answer = 42;").unwrap();
    let tool = EvalTool::new(dir.path());
    let out = run(tool.execute(
        "import",
        json!({"kernel":"js","code":
            "import * as analysis from 'analysis-kit'; display({answer:analysis.answer})"
        }),
        None,
    ))
    .unwrap();
    assert!(!out.is_error, "{}", text(&out));
    assert_eq!(
        out.details.as_ref().unwrap()["jsonOutputs"],
        json!([{"answer":42}])
    );
    std::fs::write(package.join("index.js"), "export const answer = 99;").unwrap();
    let retained = run(tool.execute(
        "retained",
        json!({"kernel":"js","code":"analysis.answer"}),
        None,
    ))
    .unwrap();
    assert!(!retained.is_error, "{}", text(&retained));
    assert!(text(&retained).contains("42"));
    let reset = run(tool.execute(
        "reset",
        json!({"kernel":"js","reset":true,
            "code":"import * as analysis from 'analysis-kit'; display({answer:analysis.answer})"
        }),
        None,
    ))
    .unwrap();
    assert!(!reset.is_error, "{}", text(&reset));
    assert_eq!(
        reset.details.as_ref().unwrap()["jsonOutputs"],
        json!([{"answer":99}])
    );
}

#[test]
fn unexported_and_commonjs_entries_never_execute_package_source() {
    let dir = tempfile::tempdir().unwrap();
    let package = dir.path().join("node_modules/sealed");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(
        package.join("package.json"),
        r#"{"exports":null,"main":"./index.mjs"}"#,
    )
    .unwrap();
    std::fs::write(
        package.join("index.mjs"),
        "globalThis.packageEffect = true; export default 1;",
    )
    .unwrap();
    let tool = EvalTool::new(dir.path());
    let rejected = run(tool.execute(
        "sealed",
        json!({"kernel":"js","code":"await import('sealed')"}),
        None,
    ))
    .unwrap();
    assert!(rejected.is_error);
    assert!(text(&rejected).contains("EVAL_MODULE"));
    assert!(text(&rejected).contains("EVAL_PACKAGE_NOT_EXPORTED"));
    let legacy = dir.path().join("node_modules/legacy");
    std::fs::create_dir_all(&legacy).unwrap();
    std::fs::write(
        legacy.join("package.json"),
        r#"{"type":"commonjs","main":"index.js"}"#,
    )
    .unwrap();
    std::fs::write(
        legacy.join("index.js"),
        "globalThis.legacyEffect = true; module.exports = 1;",
    )
    .unwrap();
    let rejected = run(tool.execute(
        "legacy",
        json!({"kernel":"js","code":"await import('legacy')"}),
        None,
    ))
    .unwrap();
    assert!(rejected.is_error);
    assert!(text(&rejected).contains("EVAL_MODULE"));
    assert!(text(&rejected).contains("EVAL_PACKAGE_COMMONJS"));
    let check = run(tool.execute(
        "check",
        json!({"kernel":"js","code":"typeof packageEffect"}),
        None,
    ))
    .unwrap();
    assert!(!check.is_error);
    assert!(text(&check).contains("undefined"));
    let check = run(tool.execute(
        "legacy-check",
        json!({"kernel":"js","code":"typeof legacyEffect"}),
        None,
    ))
    .unwrap();
    assert!(!check.is_error);
    assert!(text(&check).contains("undefined"));
}
