//! Bounded, session-configured assistant replies through `write out://`.
//!
//! A reply is persisted only as its normal tool result. There is no shared
//! message store, filesystem target, undo unit, or task-completion signal.

use super::{ContentBlock, Error, Result, TextContent, ToolOutput, ToolUpdate, WriteTool};
use serde_json::Value;
use std::ffi::OsStr;

const SCHEMA: &str = "pi.interim_response.v1";
const MAX_REPLY_BYTES: usize = 64 * 1024;

pub(crate) fn is_interim_response_path(path: &str) -> bool {
    path.get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("out://"))
}

fn valid_text(text: &str) -> bool {
    text.len() <= MAX_REPLY_BYTES && !text.trim().is_empty()
}

/// Extract an accepted, bounded interim reply from a write-tool result.
///
/// Callers should also pair the result with the `write` invocation. Ordinary
/// outputs, failed writes and malformed metadata never become assistant prose.
#[must_use]
pub fn interim_response_text(output: &ToolOutput) -> Option<&str> {
    if output.is_error {
        return None;
    }
    output
        .details
        .as_ref()
        .and_then(interim_response_text_from_details)
}

/// History consumers additionally check the tool name and result's error flag.
pub(crate) fn interim_response_text_from_details(details: &Value) -> Option<&str> {
    if details.get("schema").and_then(Value::as_str) != Some(SCHEMA) {
        return None;
    }
    details
        .get("text")
        .and_then(Value::as_str)
        .filter(|text| valid_text(text))
}

fn child_markers_present(depth: Option<&OsStr>, inherited_channel: bool) -> bool {
    inherited_channel
        || depth.is_some_and(|value| value.to_str().and_then(|s| s.parse::<u64>().ok()) != Some(0))
}

pub(super) fn caller_is_child() -> bool {
    child_markers_present(
        std::env::var_os("PI_SUBAGENT_DEPTH").as_deref(),
        crate::agent_hub::ipc::inherited_channel_present(),
    )
}

fn validate_caller(enabled: bool, child: bool) -> Result<()> {
    if child {
        return Err(Error::tool(
            "write",
            "Subagents cannot reply to the user through out://; report through your result instead.",
        ));
    }
    if !enabled {
        return Err(Error::tool(
            "write",
            "out:// is off in this session (out.enabled; enabled by default only in the interactive TUI). Reply in your final message instead.",
        ));
    }
    Ok(())
}

/// The same exact envelope governs nonmutating effects and execution.
pub(super) fn validated_input(input: &Value) -> Result<&str> {
    let fields = input
        .as_object()
        .ok_or_else(|| Error::validation("out:// requires an object with path and content"))?;
    if !fields
        .get("path")
        .and_then(Value::as_str)
        .is_some_and(is_interim_response_path)
    {
        return Err(Error::validation("Expected an out:// reply path"));
    }
    if fields
        .keys()
        .any(|key| !matches!(key.as_str(), "path" | "content"))
    {
        return Err(Error::validation(
            "out:// accepts only path and content; conflict selections are not replies",
        ));
    }
    let text = fields
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::validation("out:// requires Markdown content as a string"))?;
    if text.len() > MAX_REPLY_BYTES {
        return Err(Error::tool(
            "write",
            format!(
                "out:// replies are capped at 64 KiB UTF-8 (got {} bytes); shorten the reply or split it up.",
                text.len()
            ),
        ));
    }
    if text.trim().is_empty() {
        return Err(Error::tool(
            "write",
            "out:// needs non-empty Markdown content",
        ));
    }
    Ok(text)
}

impl WriteTool {
    pub(super) fn execute_interim_response(
        &self,
        input: &Value,
        on_update: Option<&(dyn Fn(ToolUpdate) + Send + Sync)>,
    ) -> Result<ToolOutput> {
        validate_caller(self.output_enabled, caller_is_child())?;
        let text = validated_input(input)?;
        let output = ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new("Shown to the user."))],
            details: Some(serde_json::json!({"schema": SCHEMA, "text": text})),
            is_error: false,
        };
        if let Some(on_update) = on_update {
            on_update(ToolUpdate {
                content: output.content.clone(),
                details: output.details.clone(),
            });
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{Config, ReadTool, Tool, ToolEffects, ToolRegistry};
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    #[test]
    fn native_child_markers_and_malformed_depth_fail_closed() {
        assert!(!child_markers_present(None, false));
        assert!(!child_markers_present(Some(OsStr::new("0")), false));
        for depth in [None, Some(OsStr::new("0")), Some(OsStr::new("1"))] {
            assert!(child_markers_present(depth, true));
        }
        for depth in ["1", "2", "", "garbage", "-1", "18446744073709551616"] {
            assert!(child_markers_present(Some(OsStr::new(depth)), false));
        }
        assert!(validate_caller(true, false).is_ok());
        assert!(
            validate_caller(false, false)
                .unwrap_err()
                .to_string()
                .contains("out.enabled")
        );
        for enabled in [false, true] {
            assert!(
                validate_caller(enabled, true)
                    .unwrap_err()
                    .to_string()
                    .contains("Subagents")
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_child_marker_is_never_treated_as_top_level() {
        use std::os::unix::ffi::OsStrExt as _;
        assert!(child_markers_present(
            Some(OsStr::from_bytes(b"\xff")),
            false
        ));
    }

    #[test]
    fn only_complete_valid_reply_envelopes_are_nonmutating() {
        let tool = WriteTool::new(std::path::Path::new("."));
        for path in ["out://", "OUT://progress", "out://../ignored"] {
            let input = json!({"path": path, "content": "## Progress\n\nStill working."});
            assert_eq!(tool.effects_for_input(&input), ToolEffects::read());
        }
        for input in [
            json!({"path": "output.md", "content": "text"}),
            json!({"path": "out:/", "content": "text"}),
            json!({"path": "out://"}),
            json!({"path": "out://", "content": 42}),
            json!({"path": "out://", "content": " \n "}),
            json!({"path": "out://", "content": "x", "conflicts": []}),
            json!({"path": "out://", "content": "x", "extra": true}),
            json!({"path": "out://", "content": "x".repeat(MAX_REPLY_BYTES + 1)}),
        ] {
            assert!(tool.effects_for_input(&input).writes(), "{input}");
        }
    }

    #[test]
    fn replies_survive_result_serialization_without_files_or_undo_entries() {
        asupersync::test_utils::run_test(|| async {
            let cwd = tempfile::tempdir().unwrap();
            let recorder = Arc::new(crate::undo::FileMutationRecorder::default());
            let tool = WriteTool::with_before_persist_hook(cwd.path(), || {
                panic!("interim replies must not enter filesystem persistence")
            })
            .with_output_enabled(true)
            .with_mutation_recorder(Some(Arc::clone(&recorder)));
            let updates = Arc::new(Mutex::new(Vec::new()));
            let captured = Arc::clone(&updates);
            let markdown = "## Progress\n\nThe parser is fixed; checking the next input. 🦀\n";
            let output = tool
                .execute(
                    "reply-1",
                    json!({"path": "OUT://../ignored", "content": markdown}),
                    Some(Box::new(move |update| {
                        captured.lock().unwrap().push(update)
                    })),
                )
                .await
                .unwrap();
            assert_eq!(interim_response_text(&output), Some(markdown));
            assert!(
                matches!(output.content.as_slice(), [ContentBlock::Text(text)] if text.text == "Shown to the user.")
            );
            let restored: ToolOutput =
                serde_json::from_str(&serde_json::to_string(&output).unwrap()).unwrap();
            assert_eq!(interim_response_text(&restored), Some(markdown));
            let updates = updates.lock().unwrap();
            assert_eq!(updates.len(), 1);
            assert_eq!(
                updates[0]
                    .details
                    .as_ref()
                    .and_then(interim_response_text_from_details),
                Some(markdown)
            );
            assert_eq!(std::fs::read_dir(cwd.path()).unwrap().count(), 0);
            assert_eq!(recorder.stats().undo_depth, 0);
            assert_eq!(recorder.stats().store_bytes, 0);
        });
    }

    #[test]
    fn replies_count_utf8_bytes_and_reject_before_publishing() {
        asupersync::test_utils::run_test(|| async {
            let cwd = tempfile::tempdir().unwrap();
            let tool = WriteTool::new(cwd.path()).with_output_enabled(true);
            let at_limit = "🦀".repeat(MAX_REPLY_BYTES / 4);
            let output = tool
                .execute("max", json!({"path": "out://", "content": at_limit}), None)
                .await
                .unwrap();
            assert_eq!(
                interim_response_text(&output).unwrap().len(),
                MAX_REPLY_BYTES
            );
            for input in [
                json!({"path": "out://", "content": ""}),
                json!({"path": "out://", "content": " \n\t"}),
                json!({"path": "out://", "content": format!("{at_limit}x")}),
                json!({"path": "out://", "content": "reply", "conflicts": []}),
                json!({"path": "out://", "content": "reply", "extra": true}),
                json!({"path": "out://", "content": 42}),
                json!({"path": "out://"}),
            ] {
                let result = tool
                    .execute(
                        "invalid",
                        input,
                        Some(Box::new(|_| {
                            panic!("rejected replies must not publish updates")
                        })),
                    )
                    .await;
                assert!(result.is_err());
            }
            assert_eq!(std::fs::read_dir(cwd.path()).unwrap().count(), 0);
        });
    }

    #[test]
    fn registry_requires_explicit_host_enablement_and_does_not_share_it() {
        asupersync::test_utils::run_test(|| async {
            let cwd = tempfile::tempdir().unwrap();
            for enabled in [Some(true), Some(false), None] {
                let config = Config {
                    out: enabled.map(|enabled| crate::config::OutSettings {
                        enabled: Some(enabled),
                    }),
                    ..Config::default()
                };
                let registry = ToolRegistry::new(&["write"], cwd.path(), Some(&config));
                let tool = registry.get("write").unwrap();
                let output = tool
                    .execute(
                        "reply",
                        json!({"path": "out://", "content": "Progress"}),
                        Some(Box::new(move |_| {
                            assert_eq!(enabled, Some(true), "disabled replies cannot publish")
                        })),
                    )
                    .await;
                if enabled == Some(true) {
                    assert_eq!(interim_response_text(&output.unwrap()), Some("Progress"));
                    assert!(tool.description().contains("out://"));
                } else {
                    assert!(output.unwrap_err().to_string().contains("out.enabled"));
                    assert!(!tool.description().contains("out://"));
                }
            }
            assert_eq!(std::fs::read_dir(cwd.path()).unwrap().count(), 0);
        });
    }

    #[test]
    fn reading_a_reply_never_falls_back_to_a_file_or_json_query() {
        asupersync::test_utils::run_test(|| async {
            let cwd = tempfile::tempdir().unwrap();
            let reader = ReadTool::new(cwd.path());
            for path in ["out://", "OUT://progress", "out://reply.json?q=.text"] {
                let error = reader
                    .execute("read-reply", json!({"path": path}), None)
                    .await
                    .unwrap_err();
                assert!(error.to_string().contains("write-only"), "{error}");
            }
            assert_eq!(std::fs::read_dir(cwd.path()).unwrap().count(), 0);
        });
    }

    #[test]
    fn failed_or_malformed_results_never_render_as_assistant_replies() {
        for details in [
            json!({"schema": "other", "text": "hello"}),
            json!({"schema": SCHEMA}),
            json!({"schema": SCHEMA, "text": " \n "}),
            json!({"schema": SCHEMA, "text": 1}),
            json!({"schema": SCHEMA, "text": "x".repeat(MAX_REPLY_BYTES + 1)}),
        ] {
            assert_eq!(interim_response_text_from_details(&details), None);
        }
        let failed = ToolOutput {
            content: Vec::new(),
            details: Some(json!({"schema": SCHEMA, "text": "not delivered"})),
            is_error: true,
        };
        assert_eq!(interim_response_text(&failed), None);
    }
}
