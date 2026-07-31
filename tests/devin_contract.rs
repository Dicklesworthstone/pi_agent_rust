//! Regression contract extracted from the installed Devin CLI transcripts.

use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Deserialize)]
struct ToolSchemaManifest {
    schema_version: u32,
    source: String,
    transcripts_compared: usize,
    transcript_format: String,
    devin_version: String,
    installed_devin_version_at_extraction: String,
    hash_scope: String,
    canonicalization: String,
    toolset_fingerprint: String,
    sources: Vec<TranscriptSource>,
    tools: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct TranscriptSource {
    name: String,
    sha256: String,
}

#[test]
fn local_devin_tool_surface_is_pinned() {
    let manifest: ToolSchemaManifest =
        serde_json::from_str(include_str!("fixtures/devin_cli/tool_schema_manifest.json"))
            .expect("valid Devin tool schema manifest");

    let expected = BTreeMap::from([
        ("apply_patch".to_string(), "44136fa355b3".to_string()),
        ("ask_user_question".to_string(), "543035bcee45".to_string()),
        ("cloud_handoff".to_string(), "4d42c47084fe".to_string()),
        ("edit".to_string(), "70aa9c762f6e".to_string()),
        ("exec".to_string(), "75d325f0ee6e".to_string()),
        ("exit_plan_mode".to_string(), "565d6980e2e2".to_string()),
        ("find_file_by_name".to_string(), "f1ece32204c1".to_string()),
        ("get_output".to_string(), "ba9743b81207".to_string()),
        ("grep".to_string(), "f566c924a162".to_string()),
        ("kill_shell".to_string(), "badcd1021e24".to_string()),
        ("mcp_call_tool".to_string(), "ed2861c67f38".to_string()),
        ("mcp_list_servers".to_string(), "99334726611c".to_string()),
        ("mcp_list_tools".to_string(), "c23907be861b".to_string()),
        ("mcp_read_resource".to_string(), "062b2771400a".to_string()),
        ("notebook_edit".to_string(), "4e14c9c22aa1".to_string()),
        ("notebook_read".to_string(), "62d1d592440c".to_string()),
        ("read".to_string(), "86b738b12cbd".to_string()),
        ("read_subagent".to_string(), "5988d9e4a2af".to_string()),
        ("request_scope".to_string(), "ca6b3e746d7c".to_string()),
        ("run_subagent".to_string(), "7a1917d4c752".to_string()),
        ("shell_command".to_string(), "bbe0da554d85".to_string()),
        ("skill".to_string(), "bc95e1244aa0".to_string()),
        ("todo_write".to_string(), "4af7b79177ba".to_string()),
        ("update_plan".to_string(), "83a12786224d".to_string()),
        ("web_search".to_string(), "c42356a674c9".to_string()),
        ("webfetch".to_string(), "d0ffe6943a2d".to_string()),
        ("write".to_string(), "4a7f885005eb".to_string()),
        ("write_to_process".to_string(), "10c1bd8e64f0".to_string()),
    ]);

    assert_eq!(manifest.schema_version, 1);
    assert_eq!(
        manifest.source,
        "local_devin_cli_transcript_tool_definitions"
    );
    assert_eq!(manifest.transcripts_compared, 4);
    assert_eq!(manifest.transcript_format, "ATIF-v1.7");
    assert_eq!(manifest.devin_version, "3000.2.17");
    assert_eq!(manifest.installed_devin_version_at_extraction, "3000.3.22");
    assert_eq!(manifest.hash_scope, "function.parameters");
    assert_eq!(
        manifest.canonicalization,
        "python_json_dumps_sort_keys_compact_ascii_sha256_prefix_12"
    );
    assert_eq!(manifest.toolset_fingerprint, "444e21eed402");
    assert_eq!(
        manifest.sources,
        vec![
            TranscriptSource {
                name: "wooded-guest.json".to_string(),
                sha256: "42cef0e7da607eb19af1578cada3cae6aed8e83972b6ddeb0f12e45343649748"
                    .to_string(),
            },
            TranscriptSource {
                name: "unmarred-barbecue.json".to_string(),
                sha256: "5919e087067ce352cfa8791c2dd081efb63b55c4eb6f3461c814290c0c8212da"
                    .to_string(),
            },
            TranscriptSource {
                name: "fork-class.json".to_string(),
                sha256: "7903fecfa02b4a376c1474eaf011dbdcb3254d401253f1caac7fc1a81f79ad91"
                    .to_string(),
            },
            TranscriptSource {
                name: "steep-sidecar.json".to_string(),
                sha256: "97e7b8cca32ac5fd711d4f58ec2da96ff190e9e75d5ebeca0b25cdb630223e7e"
                    .to_string(),
            },
        ]
    );
    assert_eq!(manifest.tools, expected);
}
