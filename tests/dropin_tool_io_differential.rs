#![allow(clippy::too_many_lines)]

use pi::model::ContentBlock;
use pi::sse::SseParser;
use pi::tools::{
    BashTool, EditTool, FindTool, GrepTool, LsTool, ReadTool, Tool, ToolOutput, ToolUpdate,
    WriteTool, truncate_head, truncate_tail,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Deserialize)]
struct Scenario {
    id: String,
    surface: String,
    #[serde(rename = "case")]
    case_name: String,
}

fn load_scenarios() -> Vec<Scenario> {
    serde_json::from_str(include_str!(
        "dropin_tool_io_differential/g09_tool_io_scenarios.json"
    ))
    .expect("G09 tool I/O scenarios fixture must parse")
}

fn text_from_blocks(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn output_text(output: &ToolOutput) -> String {
    text_from_blocks(&output.content)
}

fn assert_contains(haystack: &str, needle: &str, scenario_id: &str) {
    assert!(
        haystack.contains(needle),
        "{scenario_id}: expected output to contain {needle:?}, got {haystack:?}"
    );
}

fn assert_not_contains(haystack: &str, needle: &str, scenario_id: &str) {
    assert!(
        !haystack.contains(needle),
        "{scenario_id}: expected output not to contain {needle:?}, got {haystack:?}"
    );
}

async fn execute<T: Tool + ?Sized>(tool: &T, input: Value) -> pi::PiResult<ToolOutput> {
    tool.execute("g09-tool-io", input, None).await
}

async fn execute_text<T: Tool + ?Sized>(tool: &T, input: Value) -> pi::PiResult<String> {
    execute(tool, input)
        .await
        .map(|output| output_text(&output))
}

fn write_fixture(path: impl AsRef<Path>, content: &str) {
    std::fs::write(path, content).expect("write fixture file");
}

async fn run_scenario(scenario: &Scenario) -> pi::PiResult<()> {
    let temp_dir = tempfile::tempdir().expect("create isolated G09 scenario directory");
    let root = temp_dir.path();

    match scenario.case_name.as_str() {
        "bash_stdout_basic" => {
            let output = execute_text(
                &BashTool::new(root),
                json!({"command": "printf 'alpha\\n'"}),
            )
            .await?;
            assert_contains(&output, "alpha", &scenario.id);
        }
        "bash_stderr_basic" => {
            let output = execute_text(
                &BashTool::new(root),
                json!({"command": "printf 'err-line\\n' >&2"}),
            )
            .await?;
            assert_contains(&output, "err-line", &scenario.id);
        }
        "bash_stdout_stderr_merged" => {
            let output = execute_text(
                &BashTool::new(root),
                json!({"command": "printf 'stdout-one\\n'; printf 'stderr-one\\n' >&2"}),
            )
            .await?;
            assert_contains(&output, "stdout-one", &scenario.id);
            assert_contains(&output, "stderr-one", &scenario.id);
        }
        "bash_exit_nonzero" => {
            let output = execute(
                &BashTool::new(root),
                json!({"command": "printf 'bad\\n'; exit 42"}),
            )
            .await?;
            assert!(
                output.is_error,
                "{}: non-zero exit must be an error",
                scenario.id
            );
            assert_contains(
                &output_text(&output),
                "Command exited with code 42",
                &scenario.id,
            );
        }
        "bash_no_output" => {
            let output = execute_text(&BashTool::new(root), json!({"command": "true"})).await?;
            assert_contains(&output, "(no output)", &scenario.id);
        }
        "bash_timeout_message" => {
            let output = execute(
                &BashTool::new(root),
                json!({"command": "sleep 2", "timeout": 1}),
            )
            .await?;
            assert!(output.is_error, "{}: timeout must be an error", scenario.id);
            assert_contains(
                &output_text(&output),
                "Command timed out after 1 seconds",
                &scenario.id,
            );
        }
        "bash_timeout_kills_descendant" => {
            let leak_path = root.join("leaked.txt");
            let command = "sh -c 'sleep 2; printf leaked > leaked.txt' & sleep 5";
            let output = execute(
                &BashTool::new(root),
                json!({"command": command, "timeout": 1}),
            )
            .await?;
            assert!(output.is_error, "{}: timeout must be an error", scenario.id);
            std::thread::sleep(Duration::from_secs(3));
            assert!(
                !leak_path.exists(),
                "{}: timed-out bash process group leaked a descendant writer",
                scenario.id
            );
        }
        "bash_command_prefix_env" => {
            let tool = BashTool::with_shell(
                root,
                None,
                Some("export PI_G09_PREFIXED=from-prefix".to_string()),
            );
            let output =
                execute_text(&tool, json!({"command": "printf \"$PI_G09_PREFIXED\\n\""})).await?;
            assert_contains(&output, "from-prefix", &scenario.id);
        }
        "bash_line_truncation_details" => {
            let command = "for i in $(seq 1 2105); do echo line-$i; done";
            let output = execute(&BashTool::new(root), json!({"command": command})).await?;
            let details = output
                .details
                .as_ref()
                .expect("line truncation must report details");
            assert!(
                details.get("truncation").is_some(),
                "{}: truncated bash output must include truncation details",
                scenario.id
            );
            assert_contains(&output_text(&output), "Showing lines", &scenario.id);
        }
        "bash_incremental_update" => {
            let updates = Arc::new(Mutex::new(Vec::<String>::new()));
            let captured = Arc::clone(&updates);
            let on_update = Box::new(move |update: ToolUpdate| {
                captured
                    .lock()
                    .expect("updates mutex")
                    .push(text_from_blocks(&update.content));
            });
            let output = BashTool::new(root)
                .execute(
                    "g09-tool-io",
                    json!({"command": "for i in $(seq 1 5); do echo update-$i; sleep 0.02; done"}),
                    Some(on_update),
                )
                .await?;
            assert_contains(&output_text(&output), "update-5", &scenario.id);
            assert!(
                !updates.lock().expect("updates mutex").is_empty(),
                "{}: bash should emit incremental updates while streaming output",
                scenario.id
            );
        }
        "write_creates_parent_dirs" => {
            let output = execute_text(
                &WriteTool::new(root),
                json!({"path": "nested/dir/file.txt", "content": "created"}),
            )
            .await?;
            assert_contains(&output, "Successfully wrote", &scenario.id);
            assert_eq!(
                std::fs::read_to_string(root.join("nested/dir/file.txt"))
                    .expect("read written file"),
                "created",
                "{}: write must create parent directories and persist content",
                scenario.id
            );
        }
        "write_reports_utf16_code_units" => {
            let output = execute_text(
                &WriteTool::new(root),
                json!({"path": "emoji.txt", "content": "😀"}),
            )
            .await?;
            assert_contains(&output, "Successfully wrote 2 bytes", &scenario.id);
        }
        "write_rejects_parent_escape" => {
            let result = execute(
                &WriteTool::new(root),
                json!({"path": "../escape.txt", "content": "nope"}),
            )
            .await;
            assert!(
                result.is_err(),
                "{}: write must reject parent escape",
                scenario.id
            );
        }
        "read_full_text" => {
            write_fixture(root.join("sample.txt"), "alpha\nbeta\n");
            let output = execute_text(&ReadTool::new(root), json!({"path": "sample.txt"})).await?;
            assert_contains(&output, "alpha", &scenario.id);
            assert_contains(&output, "beta", &scenario.id);
        }
        "read_offset_limit" => {
            write_fixture(root.join("sample.txt"), "alpha\nbeta\ngamma\ndelta\n");
            let output = execute_text(
                &ReadTool::new(root),
                json!({"path": "sample.txt", "offset": 2, "limit": 2}),
            )
            .await?;
            assert_contains(&output, "beta", &scenario.id);
            assert_contains(&output, "gamma", &scenario.id);
            assert_not_contains(&output, "alpha", &scenario.id);
            assert_not_contains(&output, "delta", &scenario.id);
        }
        "read_hashline" => {
            write_fixture(root.join("sample.txt"), "alpha\n");
            let output = execute_text(
                &ReadTool::new(root),
                json!({"path": "sample.txt", "hashline": true}),
            )
            .await?;
            assert_contains(&output, "1#", &scenario.id);
            assert_contains(&output, "alpha", &scenario.id);
        }
        "read_line_truncation_details" => {
            let content = (1..=2005)
                .map(|line| format!("line-{line}"))
                .collect::<Vec<_>>()
                .join("\n");
            write_fixture(root.join("long.txt"), &content);
            let output = execute(&ReadTool::new(root), json!({"path": "long.txt"})).await?;
            assert!(
                output
                    .details
                    .as_ref()
                    .and_then(|details| details.get("truncation"))
                    .is_some(),
                "{}: read truncation must include metadata",
                scenario.id
            );
            assert_contains(&output_text(&output), "Showing lines", &scenario.id);
        }
        "read_rejects_parent_escape" => {
            let result = execute(&ReadTool::new(root), json!({"path": "../escape.txt"})).await;
            assert!(
                result.is_err(),
                "{}: read must reject parent escape",
                scenario.id
            );
        }
        "write_read_unicode_roundtrip" => {
            execute(
                &WriteTool::new(root),
                json!({"path": "unicode.txt", "content": "café 🚀\n"}),
            )
            .await?;
            let output = execute_text(&ReadTool::new(root), json!({"path": "unicode.txt"})).await?;
            assert_contains(&output, "café 🚀", &scenario.id);
        }
        "edit_replaces_unique_text" => {
            write_fixture(root.join("edit.txt"), "alpha\nneedle\nomega\n");
            let output = execute_text(
                &EditTool::new(root),
                json!({"path": "edit.txt", "oldText": "needle", "newText": "replacement"}),
            )
            .await?;
            assert_contains(&output, "Successfully replaced", &scenario.id);
            assert_eq!(
                std::fs::read_to_string(root.join("edit.txt")).expect("read edited file"),
                "alpha\nreplacement\nomega\n",
                "{}: edit must replace only the unique matched range",
                scenario.id
            );
        }
        "edit_rejects_missing_text" => {
            write_fixture(root.join("edit.txt"), "alpha\n");
            let result = execute(
                &EditTool::new(root),
                json!({"path": "edit.txt", "oldText": "missing", "newText": "replacement"}),
            )
            .await;
            assert!(
                result.is_err(),
                "{}: edit must reject missing oldText",
                scenario.id
            );
        }
        "grep_literal_match" => {
            write_fixture(root.join("grep.txt"), "alpha\nneedle\n");
            let output = execute_text(
                &GrepTool::new(root),
                json!({"pattern": "needle", "path": "grep.txt", "literal": true}),
            )
            .await?;
            assert_contains(&output, "grep.txt:2: needle", &scenario.id);
        }
        "grep_context_window" => {
            write_fixture(root.join("grep.txt"), "before\nneedle\nafter\n");
            let output = execute_text(
                &GrepTool::new(root),
                json!({"pattern": "needle", "path": "grep.txt", "literal": true, "context": 1}),
            )
            .await?;
            assert_contains(&output, "before", &scenario.id);
            assert_contains(&output, "needle", &scenario.id);
            assert_contains(&output, "after", &scenario.id);
        }
        "grep_match_limit_details" => {
            write_fixture(root.join("grep.txt"), "needle-1\nneedle-2\nneedle-3\n");
            let output = execute(
                &GrepTool::new(root),
                json!({"pattern": "needle", "path": "grep.txt", "literal": true, "limit": 1}),
            )
            .await?;
            assert!(
                output
                    .details
                    .as_ref()
                    .and_then(|details| details.get("matchLimitReached"))
                    .is_some(),
                "{}: grep limit must be reported in details",
                scenario.id
            );
        }
        "grep_no_match_message" => {
            write_fixture(root.join("grep.txt"), "alpha\n");
            let output = execute_text(
                &GrepTool::new(root),
                json!({"pattern": "needle", "path": "grep.txt", "literal": true}),
            )
            .await?;
            assert_contains(&output, "No matches found", &scenario.id);
        }
        "grep_rejects_parent_escape" => {
            let result = execute(
                &GrepTool::new(root),
                json!({"pattern": "needle", "path": "../"}),
            )
            .await;
            assert!(
                result.is_err(),
                "{}: grep must reject parent escape",
                scenario.id
            );
        }
        "find_glob_basic" => {
            write_fixture(root.join("alpha.txt"), "a");
            write_fixture(root.join("beta.md"), "b");
            let output = execute_text(&FindTool::new(root), json!({"pattern": "*.txt"})).await?;
            assert_contains(&output, "alpha.txt", &scenario.id);
            assert_not_contains(&output, "beta.md", &scenario.id);
        }
        "find_limit_details" => {
            write_fixture(root.join("one.txt"), "1");
            write_fixture(root.join("two.txt"), "2");
            let output = execute(
                &FindTool::new(root),
                json!({"pattern": "*.txt", "limit": 1}),
            )
            .await?;
            assert!(
                output
                    .details
                    .as_ref()
                    .and_then(|details| details.get("resultLimitReached"))
                    .is_some(),
                "{}: find limit must be reported in details",
                scenario.id
            );
        }
        "find_no_match_message" => {
            let output =
                execute_text(&FindTool::new(root), json!({"pattern": "*.nomatch"})).await?;
            assert_contains(&output, "No files found matching pattern", &scenario.id);
        }
        "find_rejects_parent_escape" => {
            let result = execute(
                &FindTool::new(root),
                json!({"pattern": "*.txt", "path": "../"}),
            )
            .await;
            assert!(
                result.is_err(),
                "{}: find must reject parent escape",
                scenario.id
            );
        }
        "ls_sorted_basic" => {
            write_fixture(root.join("b.txt"), "b");
            write_fixture(root.join("a.txt"), "a");
            std::fs::create_dir(root.join("subdir")).expect("create directory fixture");
            let output = execute_text(&LsTool::new(root), json!({})).await?;
            let a_pos = output.find("a.txt").expect("a.txt in ls output");
            let b_pos = output.find("b.txt").expect("b.txt in ls output");
            let dir_pos = output.find("subdir/").expect("subdir/ in ls output");
            assert!(
                a_pos < b_pos && b_pos < dir_pos,
                "{}: ls output must be deterministic alphabetical order: {output:?}",
                scenario.id
            );
        }
        "ls_limit_details" => {
            write_fixture(root.join("a.txt"), "a");
            write_fixture(root.join("b.txt"), "b");
            let output = execute(&LsTool::new(root), json!({"limit": 1})).await?;
            assert!(
                output
                    .details
                    .as_ref()
                    .and_then(|details| details.get("entryLimitReached"))
                    .is_some(),
                "{}: ls limit must be reported in details",
                scenario.id
            );
        }
        "ls_empty_directory" => {
            let empty = root.join("empty");
            std::fs::create_dir(&empty).expect("create empty directory fixture");
            let output = execute_text(&LsTool::new(root), json!({"path": "empty"})).await?;
            assert_contains(&output, "(empty directory)", &scenario.id);
        }
        "ls_rejects_parent_escape" => {
            let result = execute(&LsTool::new(root), json!({"path": "../"})).await;
            assert!(
                result.is_err(),
                "{}: ls must reject parent escape",
                scenario.id
            );
        }
        "sse_crlf_boundary" => {
            let mut parser = SseParser::new();
            let events = parser.feed("event: response.output_text.delta\r\ndata: hello\r\n\r\n");
            assert_eq!(events.len(), 1, "{}: CRLF must emit one event", scenario.id);
            assert_eq!(events[0].event.as_ref(), "response.output_text.delta");
            assert_eq!(events[0].data, "hello");
        }
        "sse_split_event" => {
            let mut parser = SseParser::new();
            assert_eq!(
                parser.feed("event: message\n"),
                [] as [pi::http::sse::SseEvent; 0]
            );
            let events = parser.feed("data: split\n\n");
            assert_eq!(events.len(), 1, "{}: split event must emit", scenario.id);
            assert_eq!(events[0].data, "split");
        }
        "sse_multiline_data" => {
            let mut parser = SseParser::new();
            let events = parser.feed("data: alpha\ndata: beta\n\n");
            assert_eq!(events[0].data, "alpha\nbeta");
        }
        "sse_id_retry_carry" => {
            let mut parser = SseParser::new();
            let first = parser.feed("id: 9\nretry: 250\ndata: one\n\n");
            let second = parser.feed("data: two\n\n");
            assert_eq!(first[0].id.as_deref(), Some("9"));
            assert_eq!(first[0].retry, Some(250));
            assert_eq!(second[0].id.as_deref(), Some("9"));
            assert_eq!(second[0].retry, Some(250));
        }
        "sse_empty_event_name" => {
            let mut parser = SseParser::new();
            let events = parser.feed("event\ndata: body\n\n");
            assert_eq!(events[0].event.as_ref(), "message");
        }
        "truncate_tail_line_window" => {
            let content = "1\n2\n3\n4\n5".to_string();
            let truncation = truncate_tail(content, 2, 1024);
            assert!(
                truncation.truncated,
                "{}: tail truncation expected",
                scenario.id
            );
            assert_eq!(truncation.content, "4\n5");
            assert_eq!(truncation.total_lines, 5);
            assert_eq!(truncation.output_lines, 2);
        }
        "truncate_head_unicode_boundary" => {
            let truncation = truncate_head("αβγδε".to_string(), usize::MAX, 5);
            assert!(
                truncation.truncated,
                "{}: byte truncation expected",
                scenario.id
            );
            assert_eq!(truncation.content, "αβ");
        }
        other => panic!("unhandled G09 scenario case: {other}"),
    }

    Ok(())
}

#[test]
fn fixture_defines_required_g09_matrix() {
    let scenarios = load_scenarios();
    assert!(
        scenarios.len() >= 30,
        "G09 requires at least 30 executable tool I/O scenarios"
    );

    let mut ids = BTreeSet::new();
    let mut by_surface = BTreeMap::<&str, usize>::new();
    for scenario in &scenarios {
        assert!(ids.insert(scenario.id.as_str()), "duplicate scenario id");
        *by_surface.entry(scenario.surface.as_str()).or_default() += 1;
    }

    for required in ["bash", "file", "search", "streaming", "truncation"] {
        assert!(
            by_surface.contains_key(required),
            "missing required G09 surface: {required}"
        );
    }
}

#[test]
fn tool_io_scenarios_match_dropin_contract() {
    asupersync::test_utils::run_test(|| async {
        let scenarios = load_scenarios();
        for scenario in scenarios {
            run_scenario(&scenario)
                .await
                .unwrap_or_else(|err| panic!("{} failed: {err}", scenario.id));
        }
    });
}
