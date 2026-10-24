//! Admit workspace controls before discovery, snapshot I/O or child launch.
//!
//! Raw validation deliberately precedes Option deserialization: a null
//! isolation request must not turn into an unisolated child. The selected
//! policy is still executed by the existing ChildRunner and worktree engine.

use super::{Error, Result, Value};

const FIELDS: [&str; 3] = ["cwd", "isolation", "isoApply"];

pub(super) fn validate_request(input: &Value) -> Result<()> {
    let batch = input.get("tasks").is_some() || input.get("chain").is_some();
    if batch && FIELDS.iter().any(|field| input.get(*field).is_some()) {
        return Err(Error::tool(
            "subagent",
            "PI_SUBAGENT_WORKSPACE_SCOPE: cwd, isolation and isoApply belong on each tasks/chain entry, not the batch",
        ));
    }
    validate_fields(input)?;
    for field in ["tasks", "chain"] {
        if let Some(entries) = input.get(field).and_then(Value::as_array) {
            for entry in entries {
                validate_fields(entry)?;
            }
        }
    }
    Ok(())
}

fn invalid(message: &str) -> Error {
    Error::tool(
        "subagent",
        format!("PI_SUBAGENT_INVALID_WORKSPACE: {message}"),
    )
}

fn validate_fields(input: &Value) -> Result<()> {
    if let Some(value) = input.get("cwd") {
        let path = value
            .as_str()
            .ok_or_else(|| invalid("cwd must be a non-empty string"))?;
        if path.is_empty() || path.contains('\0') {
            return Err(invalid("cwd must be a non-empty path without NUL bytes"));
        }
    }
    if let Some(value) = input.get("isolation") {
        match value.as_str() {
            Some("none" | "worktree") => {}
            _ => {
                return Err(invalid(
                    "isolation must be none or worktree; omit it for the default",
                ));
            }
        }
    }
    if let Some(value) = input.get("isoApply") {
        match value.as_str() {
            Some("keep" | "apply" | "drop") => {}
            _ => return Err(invalid("isoApply must be keep, apply or drop")),
        }
        if input.get("isolation").and_then(Value::as_str) != Some("worktree") {
            return Err(invalid("isoApply requires isolation: worktree"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subagents::{RequestMode, SubagentRequest, SubagentTool};
    use crate::tools::Tool;
    use serde_json::json;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    fn single() -> Value {
        json!({"agent":"task","task":"implement the assigned change"})
    }

    #[test]
    fn omitted_workspace_fields_keep_the_original_defaults() {
        let value = single();
        validate_request(&value).expect("valid");
        let request: SubagentRequest = serde_json::from_value(value).expect("request");
        let RequestMode::Single(task) = request.mode().expect("single") else {
            panic!("single");
        };
        assert!(task.cwd.is_none());
        assert!(task.isolation.is_none());
        assert!(task.iso_apply.is_none());
    }

    #[test]
    fn single_workspace_options_reach_the_same_task_type() {
        for disposition in ["keep", "apply", "drop"] {
            let value = json!({"agent":"task","task":"implement","cwd":"packages/core","isolation":"worktree","isoApply":disposition});
            validate_request(&value).expect("valid workspace");
            let request: SubagentRequest = serde_json::from_value(value).expect("request");
            let RequestMode::Single(task) = request.mode().expect("single") else {
                panic!("single");
            };
            assert_eq!(task.cwd, Some(PathBuf::from("packages/core")));
            assert_eq!(task.isolation.as_deref(), Some("worktree"));
            assert_eq!(task.iso_apply.as_deref(), Some(disposition));
        }
    }

    #[test]
    fn explicit_invalid_fields_are_not_downgraded_in_any_workflow() {
        for (field, values) in [
            (
                "cwd",
                vec![
                    Value::Null,
                    json!(false),
                    json!(42),
                    json!([]),
                    json!(""),
                    json!("a\0b"),
                ],
            ),
            (
                "isolation",
                vec![
                    Value::Null,
                    json!(false),
                    json!(42),
                    json!({}),
                    json!(""),
                    json!("worktrees"),
                ],
            ),
            (
                "isoApply",
                vec![
                    Value::Null,
                    json!(false),
                    json!(42),
                    json!([]),
                    json!(""),
                    json!("force"),
                ],
            ),
        ] {
            for value in values {
                let mut entry = single();
                entry["isolation"] = json!("worktree");
                entry[field] = value;
                for request in [
                    entry.clone(),
                    json!({"tasks":[entry.clone()]}),
                    json!({"chain":[entry.clone()]}),
                ] {
                    let error = validate_request(&request).expect_err("must fail closed");
                    assert!(error.to_string().contains("PI_SUBAGENT_INVALID_WORKSPACE"));
                }
            }
        }
    }

    #[test]
    fn batch_level_workspace_options_never_silently_apply_or_disappear() {
        for mode in ["tasks", "chain"] {
            for field in FIELDS {
                for value in [Value::Null, json!("worktree"), json!("nested")] {
                    let mut request = json!({(mode):[single()]});
                    request[field] = value;
                    let error = validate_request(&request).expect_err("ambiguous batch option");
                    assert!(error.to_string().contains("PI_SUBAGENT_WORKSPACE_SCOPE"));
                }
            }
        }
        let typed: SubagentRequest =
            serde_json::from_value(json!({"tasks":[single()],"cwd":"nested"})).expect("typed");
        assert!(typed.mode().is_err());
    }

    #[test]
    fn dispositions_require_explicit_isolation() {
        for isolation in [None, Some("none")] {
            for disposition in ["keep", "apply", "drop"] {
                let mut request = single();
                request["isoApply"] = json!(disposition);
                if let Some(isolation) = isolation {
                    request["isolation"] = json!(isolation);
                }
                assert!(validate_request(&request).is_err());
            }
        }
    }

    #[test]
    fn parallel_and_chain_entries_retain_independent_workspaces() {
        for mode in ["tasks", "chain"] {
            let value = json!({(mode):[
                {"agent":"task","task":"first","cwd":"one","isolation":"worktree","isoApply":"keep"},
                {"agent":"scout","task":"second","cwd":"two","isolation":"none"}
            ]});
            validate_request(&value).expect("per-task options");
            let request: SubagentRequest = serde_json::from_value(value).expect("request");
            let tasks = match request.mode().expect("mode") {
                RequestMode::Parallel(tasks) | RequestMode::Chain(tasks) => tasks,
                RequestMode::Single(_) => panic!("batch"),
            };
            assert_eq!(tasks[0].cwd, Some(PathBuf::from("one")));
            assert_eq!(tasks[1].cwd, Some(PathBuf::from("two")));
            assert_eq!(tasks[0].iso_apply.as_deref(), Some("keep"));
            assert!(tasks[1].iso_apply.is_none());
        }
    }

    #[test]
    fn schema_advertises_single_and_per_task_workspace_controls() {
        let schema = SubagentTool::new(Path::new(".")).parameters();
        for field in FIELDS {
            assert!(schema["properties"].get(field).is_some());
            assert!(
                schema["definitions"]["task"]["properties"]
                    .get(field)
                    .is_some()
            );
        }
    }

    #[test]
    fn invalid_workspace_fails_before_authored_definition_discovery() {
        let temp = TempDir::new().expect("tempdir");
        let global = temp.path().join("global");
        std::fs::create_dir_all(global.join("agents")).expect("agents");
        std::fs::write(global.join("agents/broken.md"), "invalid definition").expect("definition");
        let tool = SubagentTool::with_paths(
            temp.path().to_path_buf(),
            global,
            PathBuf::from("nonexistent-child"),
        );
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let error = runtime
            .block_on(tool.execute(
                "invalid-workspace",
                json!({"agent":"task","task":"x","isolation":null}),
                None,
            ))
            .expect_err("validation precedes discovery");
        assert!(
            error.to_string().contains("PI_SUBAGENT_INVALID_WORKSPACE"),
            "{error}"
        );
    }

    #[cfg(unix)]
    fn child_fixture(temp: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let child = temp.join("child.sh");
        std::fs::write(&child, r#"#!/bin/sh
printf '%s\n' 'delegated change' > delegated.txt
printf '%s\n' '{"type":"agent_end","messages":[{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"done"}]}]}'
"#).expect("child");
        std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o700))
            .expect("executable");
        child
    }

    #[cfg(unix)]
    #[test]
    fn single_relative_cwd_controls_actual_child_execution() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path().join("workspace");
        std::fs::create_dir_all(root.join("nested")).expect("workspace");
        let tool = SubagentTool::with_paths(
            root.clone(),
            temp.path().join("global"),
            child_fixture(temp.path()),
        );
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let output = runtime
            .block_on(tool.execute(
                "single-cwd",
                json!({"agent":"task","task":"write marker","cwd":"nested"}),
                None,
            ))
            .expect("execute");
        assert!(!output.is_error, "{output:?}");
        assert!(root.join("nested/delegated.txt").exists());
        assert!(!root.join("delegated.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn single_isolation_refuses_non_git_without_launching_child() {
        let temp = TempDir::new().expect("tempdir");
        let tool = SubagentTool::with_paths(
            temp.path().to_path_buf(),
            temp.path().join("global"),
            child_fixture(temp.path()),
        );
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let output = runtime.block_on(tool.execute("single-non-git", json!({"agent":"task","task":"write marker","isolation":"worktree","isoApply":"keep"}), None)).expect("tool result");
        assert!(output.is_error, "{output:?}");
        assert!(!temp.path().join("delegated.txt").exists());
        assert!(
            output.details.expect("details")["results"][0]["error"]
                .as_str()
                .expect("error")
                .contains("PI_ISO_NOT_GIT")
        );
    }

    #[cfg(unix)]
    #[test]
    fn single_worktree_dispositions_use_existing_writeback_gates() {
        fn git(root: &Path, args: &[&str]) -> String {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args([
                    "-c",
                    "user.name=fixture",
                    "-c",
                    "user.email=fixture@example.test",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .output()
                .expect("git");
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).expect("UTF-8 git result")
        }
        for disposition in ["keep", "apply", "drop"] {
            let temp = TempDir::new().expect("tempdir");
            let root = temp.path().join("repo");
            std::fs::create_dir_all(root.join("nested")).expect("repo");
            git(&root, &["init", "-q"]);
            git(&root, &["config", "user.name", "fixture"]);
            git(&root, &["config", "user.email", "fixture@example.test"]);
            std::fs::write(root.join("nested/base.txt"), "committed\n").expect("base");
            git(&root, &["add", "."]);
            git(&root, &["commit", "-qm", "fixture baseline"]);
            std::fs::write(root.join("nested/base.txt"), "staged parent\n").expect("staged edit");
            git(&root, &["add", "nested/base.txt"]);
            std::fs::write(root.join("nested/base.txt"), "unstaged parent\n").expect("dirty edit");
            let head = git(&root, &["rev-parse", "HEAD"]);
            let index = git(&root, &["diff", "--cached"]);
            let tool = SubagentTool::with_paths(
                root.clone(),
                temp.path().join("global"),
                child_fixture(temp.path()),
            );
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .expect("runtime");
            let output = runtime.block_on(tool.execute("single-isolated", json!({"agent":"task","task":"write marker","cwd":"nested","isolation":"worktree","isoApply":disposition}), None)).expect("execute");
            assert!(!output.is_error, "{disposition}: {output:?}");
            let details = output.details.expect("details");
            let iso = &details["results"][0]["iso"];
            assert_eq!(iso["applyMode"], disposition);
            assert_eq!(iso["applied"], disposition == "apply");
            assert!(
                iso["patch"]
                    .as_str()
                    .expect("patch")
                    .contains("delegated.txt")
            );
            assert_eq!(
                root.join("nested/delegated.txt").exists(),
                disposition == "apply"
            );
            assert_eq!(
                std::fs::read_to_string(root.join("nested/base.txt")).expect("parent"),
                "unstaged parent\n"
            );
            assert_eq!(git(&root, &["rev-parse", "HEAD"]), head);
            assert_eq!(git(&root, &["diff", "--cached"]), index);
            let worktree = Path::new(iso["worktreePath"].as_str().expect("worktree"));
            if disposition == "keep" {
                assert!(worktree.join("nested/delegated.txt").exists());
                assert_eq!(
                    std::fs::read_to_string(worktree.join("nested/base.txt"))
                        .expect("captured dirty file"),
                    "unstaged parent\n"
                );
                git(
                    &root,
                    &[
                        "worktree",
                        "remove",
                        "--force",
                        worktree.to_str().expect("path"),
                    ],
                );
            } else {
                assert!(!worktree.exists());
            }
        }
    }
}
