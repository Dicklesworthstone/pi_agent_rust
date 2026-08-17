//! dialect_bench (bd-cv653.7.8): the offline oracle for tool-call dialect
//! repair. Runs a fixture corpus of weak-model emissions through the dialect
//! extractor and reports first-attempt apply rate + false-positive rate as
//! JSON. `--live` is a documented gate for future provider-backed scoring
//! (requires real keys; not part of CI).

use pi::dialects::{Dialect, dialect_for_model, extract_text_tool_calls};
use serde_json::json;
use std::fmt::Write as _;

/// One corpus case: text a weak model might emit + whether a valid tool call
/// should be extracted from it.
struct Case {
    name: &'static str,
    dialect: Dialect,
    text: &'static str,
    expect_extraction: bool,
    expected_tool: Option<&'static str>,
}

const KNOWN: &[&str] = &["read", "write", "edit", "bash", "grep", "find", "ls"];

fn is_known(name: &str) -> bool {
    KNOWN.contains(&name)
}

fn corpus() -> Vec<Case> {
    vec![
        Case {
            name: "xmlish_tool_call_tag",
            dialect: Dialect::Xmlish,
            text: r#"Let me check. <tool_call>{"name": "read", "arguments": {"path": "src/lib.rs"}}</tool_call>"#,
            expect_extraction: true,
            expected_tool: Some("read"),
        },
        Case {
            name: "fenced_json_block",
            dialect: Dialect::Xmlish,
            text: "I'll read it:\n\n```json\n{\"name\": \"read\", \"arguments\": {\"path\": \"Cargo.toml\"}}\n```\n",
            expect_extraction: true,
            expected_tool: Some("read"),
        },
        Case {
            name: "bare_json_object",
            dialect: Dialect::Xmlish,
            text: r#"{"name": "bash", "arguments": {"command": "cargo test"}}"#,
            expect_extraction: true,
            expected_tool: Some("bash"),
        },
        Case {
            name: "tool_name_tag",
            dialect: Dialect::Xmlish,
            text: r#"<tool name="grep">{"pattern": "fn main"}</tool>"#,
            expect_extraction: true,
            expected_tool: Some("grep"),
        },
        Case {
            name: "prose_around_object_no_extract",
            dialect: Dialect::Xmlish,
            text: "I would suggest running:\n{\"name\": \"bash\", \"arguments\": {\"command\": \"rm -rf /\"}}\nbut let me confirm first.",
            expect_extraction: false,
            expected_tool: None,
        },
        Case {
            name: "rust_fence_example_no_extract",
            dialect: Dialect::Xmlish,
            text: "For example:\n\n```rust\n{\"name\": \"read\", \"arguments\": {}}\n```\n",
            expect_extraction: false,
            expected_tool: None,
        },
        Case {
            name: "unknown_tool_no_extract",
            dialect: Dialect::Xmlish,
            text: r#"<tool_call>{"name": "fly_to_moon", "arguments": {}}</tool_call>"#,
            expect_extraction: false,
            expected_tool: None,
        },
        Case {
            name: "non_object_args_no_extract",
            dialect: Dialect::Xmlish,
            text: r#"{"name": "bash", "arguments": "ls"}"#,
            expect_extraction: false,
            expected_tool: None,
        },
        Case {
            name: "plain_prose_no_extract",
            dialect: Dialect::Xmlish,
            text: "The function reads the file and returns the parsed config.",
            expect_extraction: false,
            expected_tool: None,
        },
        Case {
            name: "native_dialect_untouched",
            dialect: Dialect::Native,
            text: r#"{"name": "read", "arguments": {"path": "x"}}"#,
            expect_extraction: false, // never extracted for Native models
            expected_tool: None,
        },
    ]
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--live") {
        eprintln!(
            "dialect_bench --live: provider-backed scoring requires real API keys and is \
             a manual lane (not CI). Offline corpus results are the gate."
        );
        std::process::exit(2);
    }

    let mut rows = Vec::new();
    let mut tp = 0usize; // extracted when expected
    let mut fp = 0usize; // extracted when NOT expected (or wrong tool)
    let mut miss = 0usize; // not extracted when expected
    let mut correct_negative = 0usize;

    for case in corpus() {
        // Dialect gating is part of the contract: extraction only runs for
        // non-native dialects.
        let extracted = if case.dialect == Dialect::Native {
            Vec::new()
        } else {
            extract_text_tool_calls(case.text, &is_known)
        };
        let hit = extracted.first();
        let (ok, kind) = match (case.expect_extraction, hit) {
            (true, Some(candidate)) => {
                if Some(candidate.name.as_str()) == case.expected_tool {
                    tp += 1;
                    (true, "tp")
                } else {
                    fp += 1;
                    (false, "fp(wrong-tool)")
                }
            }
            (true, None) => {
                miss += 1;
                (false, "miss")
            }
            (false, Some(_)) => {
                fp += 1;
                (false, "fp")
            }
            (false, None) => {
                correct_negative += 1;
                (true, "tn")
            }
        };
        rows.push(json!({
            "case": case.name,
            "dialect": case.dialect.as_str(),
            "verdict": kind,
            "ok": ok,
        }));
    }

    let positives = tp + miss;
    let negatives = fp + correct_negative;
    let apply_rate = if positives == 0 {
        1.0
    } else {
        tp as f64 / positives as f64
    };
    let fp_rate = if negatives == 0 {
        0.0
    } else {
        fp as f64 / negatives as f64
    };

    let report = json!({
        "schema": "pi.dialect_bench.v1",
        "bead": "bd-cv653.7.8",
        "totals": {
            "cases": rows.len(),
            "true_positives": tp,
            "misses": miss,
            "false_positives": fp,
            "true_negatives": correct_negative,
            "first_attempt_apply_rate": apply_rate,
            "false_positive_rate": fp_rate,
        },
        "rows": rows,
    });
    let mut out = String::new();
    let _ = write!(out, "{}", serde_json::to_string_pretty(&report).expect("render"));
    println!("{out}");

    // Gate: 100% apply rate on the positive corpus, 0% false positives.
    if (apply_rate - 1.0).abs() > f64::EPSILON || fp > 0 {
        eprintln!("ORACLE GATE FAILED: apply_rate={apply_rate} false_positives={fp}");
        std::process::exit(1);
    }
    eprintln!("oracle gate: apply_rate=100%, false_positives=0 ({} cases)", rows.len());
}
