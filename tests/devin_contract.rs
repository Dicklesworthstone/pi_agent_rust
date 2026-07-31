//! Regression contract extracted from the installed Devin CLI transcripts.

use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Deserialize)]
struct ToolSchemaManifest {
    schema_version: u32,
    source: String,
    transcripts_compared: usize,
    canonicalization: String,
    tools: BTreeMap<String, String>,
}

#[test]
fn local_devin_tool_surface_is_pinned() {
    let manifest: ToolSchemaManifest = serde_json::from_str(include_str!(
        "fixtures/devin_cli/tool_schema_manifest.json"
    ))
    .expect("valid Devin tool schema manifest");

    let expected = [
        "apply_patch",
        "ask_user_question",
        "cloud_handoff",
        "edit",
        "exec",
        "exit_plan_mode",
        "find_file_by_name",
        "get_output",
        "grep",
        "kill_shell",
        "mcp_call_tool",
        "mcp_list_servers",
        "mcp_list_tools",
        "mcp_read_resource",
        "notebook_edit",
        "notebook_read",
        "read",
        "read_subagent",
        "request_scope",
        "run_subagent",
        "shell_command",
        "skill",
        "todo_write",
        "update_plan",
        "web_search",
        "webfetch",
        "write",
        "write_to_process",
    ];
    let actual = manifest.tools.keys().map(String::as_str).collect::<Vec<_>>();

    assert_eq!(manifest.schema_version, 1);
    assert_eq!(
        manifest.source,
        "local_devin_cli_transcript_tool_definitions"
    );
    assert_eq!(manifest.transcripts_compared, 4);
    assert_eq!(
        manifest.canonicalization,
        "sorted_compact_json_sha256_prefix_12"
    );
    assert_eq!(actual, expected);
    assert!(manifest.tools.values().all(|digest| {
        digest.len() == 12 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    }));
}
