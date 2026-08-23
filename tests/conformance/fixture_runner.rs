//! Fixture-based conformance test runner.
//!
//! This module provides the infrastructure to run tests defined in JSON fixture files.

use crate::conformance::{
    FixtureFile, SetupStep, TestCase, TestResult, validate_expected_with_goldens,
};
use clap::error::ErrorKind;
use pi::cli::{Cli, Commands, ExtensionCliFlag, parse_with_extension_flags};
use pi::model::ContentBlock;
use pi::tools::Tool;
use serde_json::{Value, json};
use std::path::{Component, Path, PathBuf};
use tempfile::TempDir;

/// Run all test cases from a fixture file.
pub async fn run_fixture_tests(fixture: &FixtureFile) -> Vec<TestResult> {
    let mut results = Vec::new();

    for case in &fixture.cases {
        let result = run_test_case(&fixture.tool, case).await;
        results.push(result);
    }

    results
}

/// Run a single test case.
async fn run_test_case(tool_name: &str, case: &TestCase) -> TestResult {
    let case_name = case.display_name();

    if tool_name == "cli_flags" {
        return run_cli_test_case(case, &case_name);
    }

    // Create a temporary directory for the test
    let temp_dir = match TempDir::new() {
        Ok(dir) => dir,
        Err(e) => {
            return TestResult::fail(&case_name, format!("Failed to create temp dir: {e}"));
        }
    };

    // Run setup steps
    if let Err(e) = run_setup_steps(&case.setup, temp_dir.path()) {
        return TestResult::fail(&case_name, format!("Setup failed: {e}"));
    }

    // Create the tool
    let tool: Box<dyn Tool> = match tool_name {
        "read" => Box::new(pi::tools::ReadTool::new(temp_dir.path())),
        "bash" => Box::new(pi::tools::BashTool::new(temp_dir.path())),
        "edit" => Box::new(pi::tools::EditTool::new(temp_dir.path())),
        "write" => Box::new(pi::tools::WriteTool::new(temp_dir.path())),
        "grep" => Box::new(pi::tools::GrepTool::new(temp_dir.path())),
        "find" => Box::new(pi::tools::FindTool::new(temp_dir.path())),
        "ls" => Box::new(pi::tools::LsTool::new(temp_dir.path())),
        "hashline_edit" => Box::new(pi::tools::HashlineEditTool::new(temp_dir.path())),
        "ast_grep" => Box::new(pi::ast_tools::AstGrepTool::new(temp_dir.path())),
        "ast_edit" => Box::new(pi::ast_tools::AstEditTool::new(temp_dir.path())),
        "web_search" => Box::new(pi::web_search::WebSearchTool::new()),
        "xdev" => {
            // The dispatcher's snapshot is built from the real discoverable
            // tools so fixtures exercise the genuine contract (bd-cv653.1.6).
            let ast_grep = pi::ast_tools::AstGrepTool::new(temp_dir.path());
            let ast_edit = pi::ast_tools::AstEditTool::new(temp_dir.path());
            let snapshot = vec![
                pi::xdev::DiscoverableToolInfo {
                    name: ast_grep.name().to_string(),
                    one_liner: pi::xdev::one_liner(ast_grep.description()),
                    description: ast_grep.description().to_string(),
                    parameters: ast_grep.parameters(),
                },
                pi::xdev::DiscoverableToolInfo {
                    name: ast_edit.name().to_string(),
                    one_liner: pi::xdev::one_liner(ast_edit.description()),
                    description: ast_edit.description().to_string(),
                    parameters: ast_edit.parameters(),
                },
            ];
            Box::new(pi::xdev::XdevTool::new(temp_dir.path(), snapshot))
        }
        _ => {
            return TestResult::fail(&case_name, format!("Unknown tool: {tool_name}"));
        }
    };

    // Execute the tool
    let result = tool.execute("test-id", case.input.clone(), None).await;

    // Handle expected errors
    if case.expect_error {
        match result {
            Err(e) => {
                let error_msg = e.to_string();
                if let Some(expected_substr) = &case.error_contains {
                    if error_msg
                        .to_lowercase()
                        .contains(&expected_substr.to_lowercase())
                    {
                        return TestResult::pass(&case_name);
                    }
                    return TestResult::fail(
                        &case_name,
                        format!(
                            "Error message '{error_msg}' does not contain expected '{expected_substr}'"
                        ),
                    );
                }
                return TestResult::pass(&case_name);
            }
            Ok(_) => {
                return TestResult::fail(&case_name, "Expected error but tool succeeded");
            }
        }
    }

    // Check for unexpected errors
    let output = match result {
        Ok(o) => o,
        Err(e) => {
            return TestResult::fail(&case_name, format!("Unexpected error: {e}"));
        }
    };

    // Extract text content
    let content = extract_text_content(&output.content);

    // Validate expected results
    match validate_expected_with_goldens(&case.expected, &content, output.details.as_ref()) {
        Ok(()) => {
            let mut result = TestResult::pass(&case_name);
            result.actual_content = Some(content);
            result.actual_details = output.details;
            result
        }
        Err(msg) => {
            let mut result = TestResult::fail(&case_name, msg);
            result.actual_content = Some(content);
            result.actual_details = output.details;
            result
        }
    }
}

fn run_cli_test_case(case: &TestCase, case_name: &str) -> TestResult {
    let args = case
        .input
        .get("args")
        .and_then(|value| value.as_array())
        .map(|values| {
            values
                .iter()
                .filter_map(|item| item.as_str().map(ToString::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let mut cli_args = Vec::with_capacity(args.len() + 1);
    cli_args.push("pi".to_string());
    cli_args.extend(args);

    let mut content = String::new();
    let mut details: Option<Value> = None;
    let mut parse_error: Option<String> = None;

    match parse_with_extension_flags(cli_args) {
        Ok(parsed) => {
            // Handle custom --version flag (since clap's is disabled)
            if parsed.cli.version {
                content = format!("pi {}", env!("CARGO_PKG_VERSION"));
            }
            details = Some(cli_details(&parsed.cli, &parsed.extension_flags));
        }
        Err(err) => match err.kind() {
            ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
                content = err.to_string();
            }
            _ => {
                parse_error = Some(err.to_string());
            }
        },
    }

    if case.expect_error {
        match parse_error {
            Some(error_msg) => {
                if let Some(expected_substr) = &case.error_contains {
                    if error_msg
                        .to_lowercase()
                        .contains(&expected_substr.to_lowercase())
                    {
                        return TestResult::pass(case_name);
                    }
                    return TestResult::fail(
                        case_name,
                        format!(
                            "Error message '{error_msg}' does not contain expected '{expected_substr}'"
                        ),
                    );
                }
                return TestResult::pass(case_name);
            }
            None => {
                return TestResult::fail(case_name, "Expected error but CLI parsed successfully");
            }
        }
    }

    if let Some(error_msg) = parse_error {
        return TestResult::fail(case_name, format!("Unexpected CLI error: {error_msg}"));
    }

    match validate_expected_with_goldens(&case.expected, &content, details.as_ref()) {
        Ok(()) => {
            let mut result = TestResult::pass(case_name);
            result.actual_content = Some(content);
            result.actual_details = details;
            result
        }
        Err(msg) => {
            let mut result = TestResult::fail(case_name, msg);
            result.actual_content = Some(content);
            result.actual_details = details;
            result
        }
    }
}

fn cli_details(cli: &Cli, extension_flags: &[ExtensionCliFlag]) -> Value {
    json!({
        "version": cli.version,
        "provider": cli.provider.clone(),
        "model": cli.model.clone(),
        "api_key": cli.api_key.clone(),
        "models": cli.models.clone(),
        "thinking": cli.thinking.clone(),
        "system_prompt": cli.system_prompt.clone(),
        "append_system_prompt": cli.append_system_prompt.clone(),
        "continue": cli.r#continue,
        "resume": cli.resume,
        "session": cli.session.clone(),
        "session_dir": cli.session_dir.clone(),
        "no_session": cli.no_session,
        "session_durability": cli.session_durability.clone(),
        "no_migrations": cli.no_migrations,
        "no_mouse_capture": cli.no_mouse_capture,
        "mode": cli.mode.clone(),
        "print": cli.print,
        "rpc": cli.rpc,
        "acp": cli.acp,
        "verbose": cli.verbose,
        "no_tools": cli.no_tools,
        "tools": cli.tools.clone(),
        "extension": cli.extension.clone(),
        "extension_flags": extension_flags_value(extension_flags),
        "no_extensions": cli.no_extensions,
        "extension_policy": cli.extension_policy.clone(),
        "explain_extension_policy": cli.explain_extension_policy,
        "repair_policy": cli.repair_policy.clone(),
        "explain_repair_policy": cli.explain_repair_policy,
        "skill": cli.skill.clone(),
        "no_skills": cli.no_skills,
        "prompt_template": cli.prompt_template.clone(),
        "no_prompt_templates": cli.no_prompt_templates,
        "theme": cli.theme.clone(),
        "theme_path": cli.theme_path.clone(),
        "no_themes": cli.no_themes,
        "hide_cwd_in_prompt": cli.hide_cwd_in_prompt,
        "max_tool_iterations": cli.max_tool_iterations,
        "export": cli.export.clone(),
        "list_models": list_models_value(cli.list_models.as_ref()),
        "list_providers": cli.list_providers,
        "command": command_value(cli.command.as_ref()),
        "file_args": cli
            .file_args()
            .into_iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        "message_args": cli
            .message_args()
            .into_iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
    })
}

fn extension_flags_value(extension_flags: &[ExtensionCliFlag]) -> Value {
    Value::Array(
        extension_flags
            .iter()
            .map(|flag| {
                json!({
                    "name": flag.name.clone(),
                    "value": flag.value.clone(),
                })
            })
            .collect(),
    )
}

fn list_models_value(list_models: Option<&Option<String>>) -> Value {
    match list_models {
        None => Value::Null,
        Some(None) => Value::String("all".to_string()),
        Some(Some(pattern)) => Value::String(pattern.clone()),
    }
}

#[allow(clippy::too_many_lines)]
fn command_value(command: Option<&Commands>) -> Value {
    match command {
        Some(Commands::Install { source, local }) => json!({
            "name": "install",
            "source": source,
            "local": local,
        }),
        Some(Commands::Remove { source, local }) => json!({
            "name": "remove",
            "source": source,
            "local": local,
        }),
        Some(Commands::Update { source }) => json!({
            "name": "update",
            "source": source,
        }),
        Some(Commands::UpdateIndex) => json!({
            "name": "update-index",
        }),
        Some(Commands::Worktree {
            action,
            older_than_days,
        }) => json!({
            "name": "worktree",
            "action": action,
            "older_than_days": older_than_days,
        }),
        Some(Commands::Completions { shell }) => json!({
            "name": "completions",
            "shell": shell,
        }),
        Some(Commands::Complete { flag, prefix }) => json!({
            "name": "__complete",
            "flag": flag,
            "prefix": prefix,
        }),
        Some(Commands::ContextPreview {
            format,
            bead,
            changed_paths,
            failing_command,
            max_items,
            max_bytes,
            query,
        }) => json!({
            "name": "context-preview",
            "format": format,
            "bead": bead,
            "changed_paths": changed_paths,
            "failing_command": failing_command,
            "max_items": max_items,
            "max_bytes": max_bytes,
            "query": query,
        }),
        Some(Commands::SwarmProgress {
            input,
            since,
            format,
            out_json,
            out_text,
        }) => json!({
            "name": "swarm-progress",
            "input": input,
            "since": since,
            "format": format,
            "out_json": out_json,
            "out_text": out_text,
        }),
        Some(Commands::SwarmReplayPreview {
            trace,
            policies,
            format,
            out_json,
            out_text,
            generated_at,
        }) => json!({
            "name": "swarm-replay-preview",
            "trace": trace,
            "policies": policies,
            "format": format,
            "out_json": out_json,
            "out_text": out_text,
            "generated_at": generated_at,
        }),
        Some(Commands::ValidationBroker { command }) => validation_broker_command_value(command),
        Some(Commands::List) => json!({
            "name": "list",
        }),
        Some(Commands::Config { .. }) => json!({
            "name": "config",
        }),
        Some(Commands::Search {
            query,
            tag,
            sort,
            limit,
        }) => json!({
            "name": "search",
            "query": query,
            "tag": tag,
            "sort": sort,
            "limit": limit,
        }),
        Some(Commands::Info { name }) => json!({
            "name": "info",
            "extension": name,
        }),
        Some(Commands::Doctor {
            path,
            format,
            policy,
            ..
        }) => json!({
            "name": "doctor",
            "path": path,
            "format": format,
            "policy": policy,
        }),
        Some(Commands::Usage { format, refresh }) => json!({
            "name": "usage",
            "format": format,
            "refresh": refresh,
        }),
        Some(Commands::Migrate { path, dry_run }) => json!({
            "name": "migrate",
            "path": path,
            "dry_run": dry_run,
        }),
        Some(Commands::Token { input }) => json!({
            "name": "token",
            "input": input,
        }),
        Some(Commands::Handoff {
            to,
            out,
            session,
            print,
        }) => json!({
            "name": "handoff",
            "to": to,
            "out": out.as_ref().map(|p| p.to_string_lossy().to_string()),
            "session": session,
            "print": print,
        }),
        Some(Commands::Rules { .. }) => json!({
            "name": "rules",
        }),
        Some(Commands::Grievances { .. }) => json!({
            "name": "grievances",
        }),
        Some(Commands::Commit { .. }) => json!({
            "name": "commit",
        }),
        Some(Commands::Import { .. }) => json!({
            "name": "import",
        }),
        Some(Commands::SelfUpdate { .. }) => json!({
            "name": "self-update",
        }),
        Some(Commands::Review { .. }) => json!({
            "name": "review",
        }),
        Some(Commands::Gc { .. }) => json!({
            "name": "gc",
        }),
        Some(Commands::Stats { .. }) => json!({
            "name": "stats",
        }),
        Some(Commands::Profile { .. }) => json!({
            "name": "profile",
        }),
        None => Value::Null,
    }
}

fn validation_broker_command_value(command: &pi::cli::ValidationBrokerCommand) -> Value {
    match command {
        pi::cli::ValidationBrokerCommand::Status {
            store,
            format,
            out_json,
            out_text,
            generated_at,
        } => json!({
            "name": "validation-broker",
            "command": "status",
            "store": store,
            "format": format,
            "out_json": out_json,
            "out_text": out_text,
            "generated_at": generated_at,
        }),
        pi::cli::ValidationBrokerCommand::Plan {
            request,
            inputs,
            store,
            policy,
            format,
            out_json,
            out_text,
            generated_at,
        } => json!({
            "name": "validation-broker",
            "command": "plan",
            "request": request,
            "inputs": inputs,
            "store": store,
            "policy": policy,
            "format": format,
            "out_json": out_json,
            "out_text": out_text,
            "generated_at": generated_at,
        }),
        pi::cli::ValidationBrokerCommand::Acquire { .. }
        | pi::cli::ValidationBrokerCommand::Renew { .. }
        | pi::cli::ValidationBrokerCommand::Release { .. } => {
            validation_broker_lease_command_value(command)
        }
    }
}

fn validation_broker_lease_command_value(command: &pi::cli::ValidationBrokerCommand) -> Value {
    match command {
        pi::cli::ValidationBrokerCommand::Acquire {
            request,
            store,
            started_at,
            expires_at,
            format,
            out_json,
            out_text,
        } => json!({
            "name": "validation-broker",
            "command": "acquire",
            "request": request,
            "store": store,
            "started_at": started_at,
            "expires_at": expires_at,
            "format": format,
            "out_json": out_json,
            "out_text": out_text,
        }),
        pi::cli::ValidationBrokerCommand::Renew {
            store,
            slot_id,
            owner,
            heartbeat_at,
            expires_at,
            format,
            out_json,
            out_text,
        } => json!({
            "name": "validation-broker",
            "command": "renew",
            "store": store,
            "slot_id": slot_id,
            "owner": owner,
            "heartbeat_at": heartbeat_at,
            "expires_at": expires_at,
            "format": format,
            "out_json": out_json,
            "out_text": out_text,
        }),
        pi::cli::ValidationBrokerCommand::Release {
            store,
            slot_id,
            owner,
            at,
            reason,
            format,
            out_json,
            out_text,
        } => json!({
            "name": "validation-broker",
            "command": "release",
            "store": store,
            "slot_id": slot_id,
            "owner": owner,
            "at": at,
            "reason": reason,
            "format": format,
            "out_json": out_json,
            "out_text": out_text,
        }),
        pi::cli::ValidationBrokerCommand::Status { .. }
        | pi::cli::ValidationBrokerCommand::Plan { .. } => {
            unreachable!("status and plan commands are handled by validation_broker_command_value")
        }
    }
}

/// Run setup steps for a test case.
fn resolve_setup_path(base: &Path, relative: &str) -> Result<PathBuf, String> {
    let relative_path = Path::new(relative);
    if relative_path.is_absolute() {
        return Err(format!(
            "Setup path must be relative to the fixture temp dir: {relative}"
        ));
    }

    for component in relative_path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!(
                    "Setup path must not escape the fixture temp dir: {relative}"
                ));
            }
        }
    }

    Ok(base.join(relative_path))
}

fn run_setup_steps(steps: &[SetupStep], dir: &Path) -> Result<(), String> {
    for step in steps {
        match step {
            SetupStep::CreateFile { path, content } => {
                let file_path = resolve_setup_path(dir, path)?;
                if let Some(parent) = file_path.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("Failed to create parent dirs: {e}"))?;
                }
                std::fs::write(&file_path, content)
                    .map_err(|e| format!("Failed to create file {path}: {e}"))?;
            }
            SetupStep::CreateDir { path } => {
                let dir_path = resolve_setup_path(dir, path)?;
                std::fs::create_dir_all(&dir_path)
                    .map_err(|e| format!("Failed to create dir {path}: {e}"))?;
            }
            SetupStep::SetModified { path, unix_seconds } => {
                let entry_path = resolve_setup_path(dir, path)?;
                let mtime = filetime::FileTime::from_unix_time(*unix_seconds, 0);
                filetime::set_file_mtime(&entry_path, mtime)
                    .map_err(|e| format!("Failed to set mtime for {path}: {e}"))?;
            }
            SetupStep::RunCommand { command } => {
                #[cfg(windows)]
                let mut setup_command = std::process::Command::new("cmd");
                #[cfg(not(windows))]
                let mut setup_command = std::process::Command::new("bash");

                #[cfg(windows)]
                setup_command.arg("/C");
                #[cfg(not(windows))]
                setup_command.arg("-c");

                let output = setup_command
                    .arg(command)
                    .current_dir(dir)
                    .output()
                    .map_err(|e| format!("Failed to run command: {e}"))?;
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(format!("Setup command failed: {stderr}"));
                }
            }
        }
    }
    Ok(())
}

/// Extract text content from tool output.
fn extract_text_content(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| {
            if let ContentBlock::Text(text) = block {
                Some(text.text.clone())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Run truncation conformance tests.
pub fn run_truncation_tests(fixture: &FixtureFile) -> Vec<TestResult> {
    let mut results = Vec::new();

    for case in &fixture.cases {
        let result = run_truncation_test_case(case);
        results.push(result);
    }

    results
}

/// Run a single truncation test case.
fn run_truncation_test_case(case: &TestCase) -> TestResult {
    use pi::tools::{truncate_head, truncate_tail};

    let case_name = case.display_name();

    let content = case.input["content"].as_str().unwrap_or("");
    let max_lines = usize::try_from(
        case.input["max_lines"]
            .as_u64()
            .unwrap_or(pi::tools::DEFAULT_MAX_LINES as u64),
    )
    .unwrap_or(pi::tools::DEFAULT_MAX_LINES);
    let max_bytes = usize::try_from(
        case.input["max_bytes"]
            .as_u64()
            .unwrap_or(pi::tools::DEFAULT_MAX_BYTES as u64),
    )
    .unwrap_or(pi::tools::DEFAULT_MAX_BYTES);

    let direction = case
        .input
        .get("direction")
        .and_then(Value::as_str)
        .map(|value| value.trim().to_ascii_lowercase());
    let use_tail = match direction.as_deref() {
        Some("tail") => true,
        Some("head") => false,
        Some(other) => {
            return TestResult::fail(
                &case_name,
                format!("Invalid truncation direction '{other}' (expected 'head' or 'tail')"),
            );
        }
        None => case.name.contains("tail"),
    };

    let result = if use_tail {
        truncate_tail(content, max_lines, max_bytes)
    } else {
        truncate_head(content, max_lines, max_bytes)
    };

    // Build details JSON for validation
    let details = serde_json::json!({
        "truncated": result.truncated,
        "truncated_by": result.truncated_by.map(|t| match t {
            pi::tools::TruncatedBy::Lines => "lines",
            pi::tools::TruncatedBy::Bytes => "bytes",
        }),
        "total_lines": result.total_lines,
        "output_lines": result.output_lines,
        "total_bytes": result.total_bytes,
        "output_bytes": result.output_bytes,
        "first_line_exceeds_limit": result.first_line_exceeds_limit,
        "last_line_partial": result.last_line_partial,
    });

    match validate_expected_with_goldens(&case.expected, &result.content, Some(&details)) {
        Ok(()) => {
            let mut test_result = TestResult::pass(&case_name);
            test_result.actual_content = Some(result.content);
            test_result.actual_details = Some(details);
            test_result
        }
        Err(msg) => {
            let mut test_result = TestResult::fail(&case_name, msg);
            test_result.actual_content = Some(result.content);
            test_result.actual_details = Some(details);
            test_result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_setup_create_file() {
        let temp_dir = TempDir::new().unwrap();
        let steps = vec![SetupStep::CreateFile {
            path: "test.txt".to_string(),
            content: "hello".to_string(),
        }];

        run_setup_steps(&steps, temp_dir.path()).unwrap();

        let content = std::fs::read_to_string(temp_dir.path().join("test.txt")).unwrap();
        assert_eq!(content, "hello");
    }

    #[test]
    fn test_setup_create_nested_file() {
        let temp_dir = TempDir::new().unwrap();
        let steps = vec![SetupStep::CreateFile {
            path: "nested/dir/test.txt".to_string(),
            content: "content".to_string(),
        }];

        run_setup_steps(&steps, temp_dir.path()).unwrap();

        let content = std::fs::read_to_string(temp_dir.path().join("nested/dir/test.txt")).unwrap();
        assert_eq!(content, "content");
    }

    #[test]
    fn test_setup_create_dir() {
        let temp_dir = TempDir::new().unwrap();
        let steps = vec![SetupStep::CreateDir {
            path: "mydir".to_string(),
        }];

        run_setup_steps(&steps, temp_dir.path()).unwrap();

        assert!(temp_dir.path().join("mydir").is_dir());
    }

    #[test]
    fn test_setup_set_modified() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        std::fs::write(&file_path, "hello").unwrap();
        let steps = vec![SetupStep::SetModified {
            path: "test.txt".to_string(),
            unix_seconds: 1_700_000_000,
        }];

        run_setup_steps(&steps, temp_dir.path()).unwrap();

        let modified = std::fs::metadata(&file_path).unwrap().modified().unwrap();
        let expected = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        assert_eq!(modified, expected);
    }

    #[test]
    fn test_setup_rejects_parent_dir_escape() {
        let temp_dir = TempDir::new().unwrap();
        let steps = vec![SetupStep::CreateFile {
            path: "../escape.txt".to_string(),
            content: "nope".to_string(),
        }];

        let err =
            run_setup_steps(&steps, temp_dir.path()).expect_err("should reject parent dir escape");
        assert!(err.contains("must not escape"));
    }

    #[test]
    fn test_setup_rejects_absolute_path() {
        let temp_dir = TempDir::new().unwrap();
        let absolute = temp_dir.path().join("abs.txt");
        let steps = vec![SetupStep::CreateDir {
            path: absolute.to_string_lossy().to_string(),
        }];

        let err =
            run_setup_steps(&steps, temp_dir.path()).expect_err("should reject absolute path");
        assert!(err.contains("must be relative"));
    }

    #[test]
    fn test_setup_set_modified_rejects_parent_dir_escape() {
        let temp_dir = TempDir::new().unwrap();
        let steps = vec![SetupStep::SetModified {
            path: "../escape.txt".to_string(),
            unix_seconds: 1_700_000_000,
        }];

        let err =
            run_setup_steps(&steps, temp_dir.path()).expect_err("should reject parent dir escape");
        assert!(err.contains("must not escape"));
    }
}
