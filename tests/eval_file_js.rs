//! File-backed JavaScript through the public `EvalTool`, not a separate evaluator.
#![recursion_limit = "512"]

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
        .filter_map(|part| match part {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn scripts_reuse_state_and_reload_current_source() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("setup data.js");
    std::fs::write(
        &script,
        "globalThis.answer = 41; function inc(x) { return x + 1; } answer",
    )
    .unwrap();
    let tool = EvalTool::new(dir.path());
    run(async {
        let first = tool
            .execute(
                "load",
                json!({"kernel":"js", "code":"%load 'setup data.js'"}),
                None,
            )
            .await
            .unwrap();
        assert!(!first.is_error, "{}", text(&first));
        assert!(text(&first).trim().ends_with("41"));
        assert_eq!(
            first.details.as_ref().unwrap()["sourceCommand"],
            "%load 'setup data.js'"
        );
        let next = tool
            .execute("next", json!({"kernel":"js", "code":"inc(answer)"}), None)
            .await
            .unwrap();
        assert_eq!(text(&next).trim(), "42");
        std::fs::write(&script, "globalThis.answer = 99; answer").unwrap();
        let loaded = tool
            .execute(
                "reload",
                json!({"kernel":"js", "code":"%load 'setup data.js'"}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(text(&loaded).trim(), "99");
        assert_eq!(loaded.details.as_ref().unwrap()["cell"], 3);
    });
}

#[test]
fn scripts_can_await_the_existing_tool_bridge() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("data.txt"), "file-backed bridge").unwrap();
    std::fs::write(dir.path().join("async.js"), "await sleep(1); globalThis.payload = await tool.call('read', {path:'data.txt'}); console.log(payload)").unwrap();
    let tool = EvalTool::new(dir.path());
    let result = run(tool.execute(
        "load",
        json!({"kernel":"js", "code":"%load async.js"}),
        None,
    ))
    .unwrap();
    assert!(!result.is_error, "{}", text(&result));
    assert!(text(&result).contains("file-backed bridge"));
    let next = run(tool.execute(
        "next",
        json!({"kernel":"js", "code":"payload.includes('file-backed bridge')"}),
        None,
    ))
    .unwrap();
    assert_eq!(text(&next).trim(), "true");
}

#[test]
fn load_admission_failure_does_not_apply_requested_reset() {
    let dir = tempfile::tempdir().unwrap();
    let tool = EvalTool::new(dir.path());
    run(tool.execute(
        "seed",
        json!({"kernel":"js", "code":"globalThis.keep = 7"}),
        None,
    ))
    .unwrap();
    let error = run(tool.execute(
        "bad",
        json!({"kernel":"js", "code":"%load missing.js", "reset":true}),
        None,
    ))
    .unwrap_err();
    assert!(error.to_string().contains("EVAL_LOAD_PATH"));
    let kept = run(tool.execute("keep", json!({"kernel":"js", "code":"keep"}), None)).unwrap();
    assert_eq!(text(&kept).trim(), "7");
    assert_eq!(kept.details.as_ref().unwrap()["cell"], 2);
}

#[test]
fn syntax_and_runtime_failures_keep_filename_and_do_not_retry() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("broken.js");
    let tool = EvalTool::new(dir.path());
    run(tool.execute(
        "seed",
        json!({"kernel":"js", "code":"globalThis.count = 0"}),
        None,
    ))
    .unwrap();
    std::fs::write(&path, "globalThis.count += 1; if (").unwrap();
    let syntax = run(tool.execute(
        "syntax",
        json!({"kernel":"js", "code":"%load broken.js"}),
        None,
    ))
    .unwrap();
    assert!(syntax.is_error);
    assert!(text(&syntax).contains("broken.js"));
    let count = run(tool.execute("read", json!({"kernel":"js", "code":"count"}), None)).unwrap();
    assert_eq!(text(&count).trim(), "0");
    std::fs::write(&path, "globalThis.count += 1; JSON.parse('{')").unwrap();
    let runtime = run(tool.execute(
        "runtime",
        json!({"kernel":"js", "code":"%load broken.js"}),
        None,
    ))
    .unwrap();
    assert!(runtime.is_error);
    let count = run(tool.execute("read", json!({"kernel":"js", "code":"count"}), None)).unwrap();
    assert_eq!(text(&count).trim(), "1");
}

#[test]
fn typed_script_bindings_and_runtime_constructs_persist_in_the_same_realm() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("setup.ts"),
        r"
        interface Observation { value: number }
        type Box<T> = { value: T };
        enum Score { Base = 40, Next }
        class ObservationBox { constructor(public value: number) {} }
        function identity<T>(value: T): T { return value; }
        const observation: Box<number> = new ObservationBox(identity(Score.Next + 1));
        observation.value
    ",
    )
    .unwrap();
    let tool = EvalTool::new(dir.path());
    run(async {
        let loaded = tool
            .execute(
                "typed",
                json!({"kernel":"js", "code":"%load setup.ts"}),
                None,
            )
            .await
            .unwrap();
        assert!(!loaded.is_error, "{}", text(&loaded));
        assert!(text(&loaded).trim().ends_with("42"));
        assert_eq!(
            loaded.details.as_ref().unwrap()["sourceCommand"],
            "%load setup.ts"
        );
        assert!(
            loaded.details.as_ref().unwrap()["sourcePath"]
                .as_str()
                .unwrap()
                .ends_with("setup.ts")
        );
        let next = tool
            .execute(
                "next",
                json!({"kernel":"js", "code":"identity(observation.value) + Score.Base"}),
                None,
            )
            .await
            .unwrap();
        assert!(!next.is_error, "{}", text(&next));
        assert_eq!(text(&next).trim(), "82");
        assert_eq!(next.details.as_ref().unwrap()["cell"], 2);
        assert_eq!(next.details.as_ref().unwrap()["restarted"], false);
    });
}

#[test]
fn typed_async_cells_share_native_bridge_streaming_and_rich_output() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("data.txt"), "typed-bridge-payload").unwrap();
    std::fs::write(
        dir.path().join("async.ts"),
        r"
        interface Result { matched: boolean }
        const contents: string = await tool.call('read', {path: 'data.txt'});
        await sleep(1);
        (globalThis as any).typedPayload = contents;
        const result = { matched: contents.includes('typed-bridge-payload') } satisfies Result;
        console.log('typed-live-output');
        display(result);
    ",
    )
    .unwrap();
    let tool = EvalTool::new(dir.path());
    let seen = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let capture = std::sync::Arc::clone(&seen);
    let loaded = run(tool.execute(
        "typed",
        json!({"kernel":"js", "code":"%load async.ts"}),
        Some(Box::new(move |update| {
            if update.content.iter().any(|block|
                matches!(block, ContentBlock::Text(text) if text.text.contains("typed-live-output"))
            ) { capture.store(true, std::sync::atomic::Ordering::SeqCst); }
        })),
    ))
    .unwrap();
    assert!(!loaded.is_error, "{}", text(&loaded));
    assert!(seen.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(
        loaded.details.as_ref().unwrap()["jsonOutputs"],
        json!([{"matched":true}])
    );
    let next = run(tool.execute(
        "next",
        json!({"kernel":"js", "code":"typedPayload.includes('typed-bridge-payload')"}),
        None,
    ))
    .unwrap();
    assert!(!next.is_error, "{}", text(&next));
    assert_eq!(text(&next).trim(), "true");
}

#[test]
fn typed_preparation_refusals_preserve_state_and_cell_sequence_even_with_reset() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bad.ts");
    let tool = EvalTool::new(dir.path());
    run(async {
        tool.execute(
            "seed",
            json!({"kernel":"js", "code":"globalThis.keep = 7"}),
            None,
        )
        .await
        .unwrap();
        // Static imports are not here: b2d02c5a8 made them valid in typed loads.
        for rejected in [
            "globalThis.keep = 99; const x: number = ;",
            "globalThis.keep = 99; export const answer: number = 42;",
            "globalThis.keep = 99; const origin = import.meta.url;",
        ] {
            std::fs::write(&path, rejected).unwrap();
            let error = tool
                .execute(
                    "rejected",
                    json!({
                        "kernel":"js", "code":"%load bad.ts", "reset":true
                    }),
                    None,
                )
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("EVAL_LOAD_TYPESCRIPT"),
                "{error}"
            );
            assert!(error.to_string().contains("bad.ts"), "{error}");
        }
        let kept = tool
            .execute("kept", json!({"kernel":"js", "code":"keep"}), None)
            .await
            .unwrap();
        assert!(!kept.is_error);
        assert_eq!(text(&kept).trim(), "7");
        assert_eq!(kept.details.as_ref().unwrap()["cell"], 2);
    });
}

#[test]
fn typed_runtime_syntax_error_does_not_reexecute_prefix_mutations() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("failure.ts"),
        "const next: number = globalThis.count + 1; globalThis.count = next; JSON.parse('{');",
    )
    .unwrap();
    let tool = EvalTool::new(dir.path());
    run(async {
        tool.execute(
            "seed",
            json!({"kernel":"js", "code":"globalThis.count = 0"}),
            None,
        )
        .await
        .unwrap();
        let failed = tool
            .execute(
                "failed",
                json!({"kernel":"js", "code":"%load failure.ts"}),
                None,
            )
            .await
            .unwrap();
        assert!(failed.is_error);
        assert!(text(&failed).contains("failure.ts"));
        let count = tool
            .execute("count", json!({"kernel":"js", "code":"count"}), None)
            .await
            .unwrap();
        assert!(!count.is_error);
        assert_eq!(text(&count).trim(), "1");
    });
}

#[test]
fn typed_reloads_compile_current_bytes_and_explicit_reset_clears_old_bindings() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reload.ts");
    let tool = EvalTool::new(dir.path());
    run(async {
        std::fs::write(&path, "const answer: number = 41; answer").unwrap();
        let first = tool
            .execute(
                "first",
                json!({"kernel":"js", "code":"%load reload.ts"}),
                None,
            )
            .await
            .unwrap();
        assert!(!first.is_error);
        assert!(text(&first).trim().ends_with("41"));
        std::fs::write(&path, "const answer: number = 42; answer").unwrap();
        let unchanged = tool
            .execute("unchanged", json!({"kernel":"js", "code":"answer"}), None)
            .await
            .unwrap();
        assert_eq!(
            text(&unchanged).trim(),
            "41",
            "editing alone must not reload"
        );
        let fresh = tool
            .execute(
                "reset",
                json!({"kernel":"js", "code":"%load reload.ts", "reset":true}),
                None,
            )
            .await
            .unwrap();
        assert!(!fresh.is_error, "{}", text(&fresh));
        assert!(text(&fresh).trim().ends_with("42"));
        assert_eq!(fresh.details.as_ref().unwrap()["reset"], true);
        assert_eq!(fresh.details.as_ref().unwrap()["cell"], 1);
    });
}

#[test]
fn typed_unicode_and_bom_hashbang_files_preserve_values_and_last_expression() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("unicode.ts"),
        concat!(
            "\u{feff}#!/usr/bin/env node\n",
            "const greeting: string = '界🌍';\n",
            "const object: {answer: number} = { answer: 42 };\n",
            "greeting + object.answer\n",
        ),
    )
    .unwrap();
    let tool = EvalTool::new(dir.path());
    let loaded = run(tool.execute(
        "unicode",
        json!({"kernel":"js", "code":"%load unicode.ts"}),
        None,
    ))
    .unwrap();
    assert!(!loaded.is_error, "{}", text(&loaded));
    assert!(text(&loaded).trim().ends_with("\"界🌍42\""));
}

#[test]
fn typed_modules_link_mixed_graphs_preserve_side_effects_live_bindings_and_cache() {
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join("graph");
    std::fs::create_dir(&graph).unwrap();
    std::fs::write(graph.join("constants.js"), "export const base = 40;").unwrap();
    std::fs::write(
        graph.join("effect.ts"),
        r"
        (globalThis as any).typedLoads = ((globalThis as any).typedLoads || 0) + 1;
        export const unused: number = 1;
    ",
    )
    .unwrap();
    std::fs::write(
        graph.join("values.ts"),
        r"
        import {base} from './constants.js';
        import {unused} from './effect.ts';
        import type {Absent} from './not-installed.ts';
        export let answer: number = base + 2;
        export function bump(): void { answer += 1; }
        export const origin: string = import.meta.url;
    ",
    )
    .unwrap();
    std::fs::write(
        graph.join("entry.mts"),
        "export {answer, bump, origin} from './values.ts'; await sleep(1);",
    )
    .unwrap();
    let tool = EvalTool::new(dir.path());
    run(async {
        let loaded = tool.execute("import", json!({"kernel":"js", "code":
            "globalThis.graph = await import('./graph/entry.mts'); display({answer:graph.answer, loads:typedLoads});"
        }), None).await.unwrap();
        assert!(!loaded.is_error, "{}", text(&loaded));
        assert_eq!(
            loaded.details.as_ref().unwrap()["jsonOutputs"],
            json!([{"answer":42,"loads":1}])
        );
        let changed = tool
            .execute(
                "bump",
                json!({"kernel":"js", "code":"graph.bump(); graph.answer"}),
                None,
            )
            .await
            .unwrap();
        assert!(!changed.is_error);
        assert_eq!(text(&changed).trim(), "43");
        std::fs::write(graph.join("constants.js"), "export const base = 96;").unwrap();
        let cached = tool.execute("cached", json!({"kernel":"js", "code":
            "globalThis.sameGraph = await import('./graph/entry.mts'); display({same:sameGraph === graph, answer:sameGraph.answer, loads:typedLoads});"
        }), None).await.unwrap();
        assert!(!cached.is_error, "{}", text(&cached));
        assert_eq!(
            cached.details.as_ref().unwrap()["jsonOutputs"],
            json!([{"same":true,"answer":43,"loads":1}])
        );
        let origin = tool
            .execute(
                "origin",
                json!({"kernel":"js", "code":"graph.origin"}),
                None,
            )
            .await
            .unwrap();
        let expected =
            url::Url::from_file_path(std::fs::canonicalize(graph.join("values.ts")).unwrap())
                .unwrap();
        assert_eq!(
            serde_json::from_str::<String>(text(&origin).trim()).unwrap(),
            expected.as_str()
        );
        let fresh = tool.execute("reset", json!({"kernel":"js", "reset":true, "code":
            "globalThis.graph = await import('./graph/entry.mts'); display({answer:graph.answer, loads:typedLoads});"
        }), None).await.unwrap();
        assert!(!fresh.is_error, "{}", text(&fresh));
        assert_eq!(
            fresh.details.as_ref().unwrap()["jsonOutputs"],
            json!([{"answer":98,"loads":1}])
        );
    });
}

#[test]
fn compiled_script_functions_retain_their_relative_typed_import_origin() {
    let dir = tempfile::tempdir().unwrap();
    let analysis = dir.path().join("analysis");
    std::fs::create_dir(&analysis).unwrap();
    std::fs::write(
        analysis.join("sibling.ts"),
        "export const value: number = 42;",
    )
    .unwrap();
    std::fs::write(
        analysis.join("setup.ts"),
        r"
        (globalThis as any).loadSibling = async function(): Promise<number> {
            const module = await import('./sibling.ts');
            return module.value;
        };
    ",
    )
    .unwrap();
    let tool = EvalTool::new(dir.path());
    let loaded = run(tool.execute(
        "load",
        json!({"kernel":"js", "code":"%load analysis/setup.ts"}),
        None,
    ))
    .unwrap();
    assert!(!loaded.is_error, "{}", text(&loaded));
    let later = run(tool.execute(
        "later",
        json!({"kernel":"js", "code":"await loadSibling()"}),
        None,
    ))
    .unwrap();
    assert!(!later.is_error, "{}", text(&later));
    assert_eq!(text(&later).trim(), "42");
}

#[test]
fn typed_import_failures_do_not_execute_module_prefixes_or_escape_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(
        dir.path().join("outside.ts"),
        "globalThis.escaped = true; export const value: number = 1;",
    )
    .unwrap();
    std::fs::write(
        project.join("bad.ts"),
        "globalThis.executed = true; export const value: number = ;",
    )
    .unwrap();
    let tool = EvalTool::new(&project);
    run(async {
        tool.execute(
            "seed",
            json!({"kernel":"js", "code":"globalThis.keep = 42"}),
            None,
        )
        .await
        .unwrap();
        for specifier in [
            "./bad.ts",
            "../outside.ts",
            "node:fs",
            "https://host/remote.ts",
        ] {
            let code = format!(
                "await import({})",
                serde_json::to_string(specifier).unwrap()
            );
            let output = tool
                .execute("rejected", json!({"kernel":"js", "code":code}), None)
                .await
                .unwrap();
            assert!(output.is_error, "accepted {specifier}");
            assert!(
                text(&output).contains("EVAL_MODULE") || text(&output).contains("EVAL_TYPESCRIPT"),
                "{}",
                text(&output)
            );
        }
        let kept = tool
            .execute(
                "kept",
                json!({"kernel":"js", "code":"[keep, typeof executed, typeof escaped]"}),
                None,
            )
            .await
            .unwrap();
        assert!(!kept.is_error, "{}", text(&kept));
        assert_eq!(text(&kept).trim(), "[42,\"undefined\",\"undefined\"]");
    });
}
