use super::*;
use crate::subagents::SubagentTool;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::time::Duration;

const SCRIPT: &str = r#"#!/bin/sh
set -eu
for arg do task="$arg"; done
command=${task%%"
"*}
printf '%s\n' "$command" >> launches
case "$command" in
  'Task: choose') printf '%s\n' @@CHOICE@@ ;;
  'Task: left') : > left-effect; printf '%s\n' @@LEFT@@ ;;
  'Task: right') : > right-effect; printf '%s\n' @@RIGHT@@ ;;
  'Task: fail') exit 7 ;;
  'Task: join') printf '%s' "$task" > received; printf '%s\n' @@DONE@@ ;;
  *) exit 31 ;;
esac
"#;

fn terminal(data: &Value) -> String {
    json!({"type":"agent_end","messages":[{"role":"assistant","stopReason":"stop",
        "content":[{"type":"text","text":data.to_string()}]}]})
    .to_string()
}

fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

struct Fixture {
    dir: tempfile::TempDir,
    tool: SubagentTool,
    runtime: asupersync::runtime::Runtime,
}

impl Fixture {
    fn new(choice: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("workspace");
        let global = dir.path().join("global");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(global.join("agents")).unwrap();
        std::fs::write(global.join("agents/worker.md"),
            "---\nname: worker\ndescription: selected input fixture\ntools: []\n---\nReturn the fixture output.\n").unwrap();
        let child = dir.path().join("child.sh");
        let script = SCRIPT
            .replace("@@CHOICE@@", &quoted(&terminal(&json!({"take":choice}))))
            .replace(
                "@@LEFT@@",
                &quoted(&terminal(&json!({"branch":"left","value":null}))),
            )
            .replace(
                "@@RIGHT@@",
                &quoted(&terminal(&json!({"branch":"right","value":false}))),
            )
            .replace("@@DONE@@", &quoted(&terminal(&json!({"done":true}))));
        std::fs::write(&child, script).unwrap();
        std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o700)).unwrap();
        let tool =
            SubagentTool::with_paths(cwd, global, child).with_timeout(Duration::from_secs(10));
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        Self { dir, tool, runtime }
    }

    fn root(&self) -> PathBuf {
        self.dir.path().join("workspace")
    }

    fn run(&self, nodes: Vec<Value>) -> crate::tools::ToolOutput {
        self.runtime
            .block_on(self.tool.execute(
                "selected-input-native",
                json!({"workflow":nodes,"scope":"user","concurrency":2}),
                None,
            ))
            .unwrap()
    }

    fn launches(&self) -> String {
        std::fs::read_to_string(self.root().join("launches")).unwrap_or_default()
    }
}

fn graph() -> Vec<Value> {
    let mut left = node("left");
    left["needs"] = json!(["choose"]);
    left["when"] = json!({"from":"choose","pointer":"/take","equals":"left"});
    let mut right = node("right");
    right["needs"] = json!(["choose"]);
    right["when"] = json!({"from":"choose","pointer":"/take","equals":"right"});
    vec![node("choose"), left, right, joined()]
}

#[test]
fn either_native_branch_can_supply_the_same_continuation_without_retyping_the_result() {
    event("native-selected-branch-handoff");
    for choice in ["left", "right"] {
        let fixture = Fixture::new(choice);
        let output = fixture.run(graph());
        assert!(!output.is_error, "{output:?}");
        let details = output.details.unwrap();
        assert_eq!(details["acceptedNodes"], 3);
        assert_eq!(details["notSelectedNodes"], 1);
        assert_eq!(details["nodes"][3]["state"], "accepted");
        let prompt = std::fs::read_to_string(fixture.root().join("received")).unwrap();
        let expected_value = if choice == "left" {
            Value::Null
        } else {
            json!(false)
        };
        assert_eq!(
            data_block(&prompt),
            json!({"answer":{"branch":choice,"value":expected_value}})
        );
        assert!(fixture.root().join(format!("{choice}-effect")).exists());
        let omitted = if choice == "left" { "right" } else { "left" };
        assert!(!fixture.root().join(format!("{omitted}-effect")).exists());
        assert_eq!(fixture.launches().lines().count(), 3);
    }
}

#[test]
fn ambiguous_successes_and_a_failed_alternative_never_launch_the_join_or_undo_effects() {
    event("native-ambiguity-and-failure");
    for fail in [false, true] {
        let fixture = Fixture::new("left");
        let mut graph = graph();
        for node in &mut graph[1..3] {
            node.as_object_mut().unwrap().remove("when");
        }
        if fail {
            graph[2]["call"]["task"] = json!("fail");
        }
        let output = fixture.run(graph);
        assert!(output.is_error);
        let details = output.details.unwrap();
        assert_eq!(details["nodes"][3]["state"], "skipped");
        assert_eq!(
            details["nodes"][3]["skipReason"],
            if fail {
                "dependency_failed"
            } else {
                "selected_input_ambiguous"
            }
        );
        assert!(!fixture.root().join("received").exists());
        assert!(fixture.root().join("left-effect").exists());
        if !fail {
            assert!(fixture.root().join("right-effect").exists());
        }
    }
}

#[test]
fn missing_selected_fields_are_errors_but_intentionally_unselected_joins_do_not_run() {
    event("native-missing-and-unselected");
    let fixture = Fixture::new("left");
    let mut missing = graph();
    missing[3]["inputs"]["answer"]["pointer"] = json!("/missing");
    let output = fixture.run(missing);
    assert!(output.is_error);
    assert_eq!(
        output.details.unwrap()["nodes"][3]["skipReason"],
        "dependency_input_missing"
    );
    assert!(!fixture.root().join("received").exists());

    let fixture = Fixture::new("neither");
    let output = fixture.run(graph());
    assert!(!output.is_error, "{output:?}");
    assert_eq!(output.details.unwrap()["nodes"][3]["state"], "not_selected");
    assert_eq!(fixture.launches(), "Task: choose\n");
}

#[test]
fn all_candidate_schemas_are_checked_before_even_the_decider_launches() {
    event("native-all-candidates-preflight");
    let fixture = Fixture::new("left");
    for invalid in [false, true] {
        let mut graph = graph();
        if invalid {
            graph[2]["call"]["outputSchema"] = json!({"type":"not-a-type"});
        } else {
            graph[2]["call"]
                .as_object_mut()
                .unwrap()
                .remove("outputSchema");
        }
        let error = fixture
            .runtime
            .block_on(fixture.tool.execute(
                "selected-preflight",
                json!({"workflow":graph,"scope":"user"}),
                None,
            ))
            .unwrap_err();
        assert!(error.to_string().contains("schema"));
        assert!(fixture.launches().is_empty());
    }
}
