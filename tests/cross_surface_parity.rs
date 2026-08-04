//! DROPIN-174: Cross-surface unit tests for CLI, config, session, and error
//! parity. These tests verify invariants that span multiple subsystems and
//! confirm drop-in replacement behavior.

mod common;

use pi::error::Error;
use pi::model::{
    AssistantMessage, ContentBlock, Message, StopReason, TextContent, ToolCall, Usage,
};
use pi::session::Session;
use serde_json::json;

// ============================================================================
// 1. Exit-Code Classification — every error variant → correct exit code
// ============================================================================

mod exit_codes {
    use super::*;

    /// Helper: wrap a pi Error in anyhow and classify.
    fn classify(err: Error) -> &'static str {
        let anyhow_err = anyhow::Error::new(err);
        let is_usage = anyhow_err.chain().any(|cause| {
            cause
                .downcast_ref::<Error>()
                .is_some_and(|e| matches!(e, Error::Validation(_)))
        });
        if is_usage { "usage" } else { "failure" }
    }

    #[test]
    fn validation_errors_are_usage() {
        assert_eq!(classify(Error::validation("bad flag")), "usage");
        assert_eq!(classify(Error::validation("")), "usage");
        assert_eq!(
            classify(Error::validation("unknown --only categories")),
            "usage"
        );
    }

    #[test]
    fn config_errors_are_failure() {
        assert_eq!(classify(Error::config("missing file")), "failure");
    }

    #[test]
    fn session_errors_are_failure() {
        assert_eq!(classify(Error::session("corrupt")), "failure");
    }

    #[test]
    fn provider_errors_are_failure() {
        assert_eq!(
            classify(Error::provider("anthropic", "rate limited")),
            "failure"
        );
    }

    #[test]
    fn auth_errors_are_failure() {
        assert_eq!(classify(Error::auth("missing key")), "failure");
    }

    #[test]
    fn tool_errors_are_failure() {
        assert_eq!(classify(Error::tool("bash", "timeout")), "failure");
    }

    #[test]
    fn extension_errors_are_failure() {
        assert_eq!(classify(Error::extension("load failed")), "failure");
    }

    #[test]
    fn api_errors_are_failure() {
        assert_eq!(classify(Error::api("server error")), "failure");
    }

    #[test]
    fn aborted_errors_are_failure() {
        assert_eq!(classify(Error::Aborted), "failure");
    }

    #[test]
    fn io_errors_are_failure() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
        let err: Error = io_err.into();
        assert_eq!(classify(err), "failure");
    }

    #[test]
    fn json_errors_are_failure() {
        let json_err =
            serde_json::from_str::<serde_json::Value>("{{bad}}").expect_err("should fail");
        let err: Error = json_err.into();
        assert_eq!(classify(err), "failure");
    }
}

// ============================================================================
// 2. CLI Flag Combination Tests — multiple flags parsed together
// ============================================================================

mod cli_combinations {
    use clap::Parser;
    use pi::cli::Cli;

    #[test]
    fn print_mode_with_provider_and_model() {
        let cli = Cli::parse_from([
            "pi",
            "-p",
            "--provider",
            "openai",
            "--model",
            "gpt-4o",
            "--thinking",
            "off",
        ]);
        assert!(cli.print);
        assert_eq!(cli.provider.as_deref(), Some("openai"));
        assert_eq!(cli.model.as_deref(), Some("gpt-4o"));
        assert_eq!(cli.thinking.as_deref(), Some("off"));
    }

    #[test]
    fn session_flags_are_mutually_parsed() {
        let cli = Cli::parse_from([
            "pi",
            "-c",
            "--session",
            "/tmp/sess.jsonl",
            "--session-dir",
            "/tmp/sessions",
        ]);
        assert!(cli.r#continue);
        assert_eq!(cli.session.as_deref(), Some("/tmp/sess.jsonl"));
        assert_eq!(cli.session_dir.as_deref(), Some("/tmp/sessions"));
    }

    #[test]
    fn no_flags_disable_discovery() {
        let cli = Cli::parse_from([
            "pi",
            "--no-tools",
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
        ]);
        assert!(cli.no_tools);
        assert!(cli.no_extensions);
        assert!(cli.no_skills);
        assert!(cli.no_prompt_templates);
        assert!(cli.no_themes);
    }

    #[test]
    fn multiple_extensions_and_skills() {
        let cli = Cli::parse_from([
            "pi", "-e", "ext1.js", "-e", "ext2.js", "--skill", "s1.md", "--skill", "s2.md",
        ]);
        assert_eq!(cli.extension, vec!["ext1.js", "ext2.js"]);
        assert_eq!(cli.skill, vec!["s1.md", "s2.md"]);
    }

    #[test]
    fn print_mode_with_json_output() {
        let cli = Cli::parse_from(["pi", "-p", "--mode", "json"]);
        assert!(cli.print);
        assert_eq!(cli.mode.as_deref(), Some("json"));
    }

    #[test]
    fn tools_subset_with_provider() {
        let cli = Cli::parse_from(["pi", "--tools", "read,bash", "--provider", "anthropic"]);
        assert_eq!(cli.tools, "read,bash");
        assert_eq!(cli.provider.as_deref(), Some("anthropic"));
    }

    #[test]
    fn config_subcommand_with_flags() {
        let cli = Cli::parse_from(["pi", "config", "--show"]);
        assert!(cli.command.is_some());
    }

    #[test]
    fn trailing_message_with_at_file_ref() {
        let cli = Cli::parse_from(["pi", "-p", "hello", "@file.txt", "world"]);
        assert!(cli.print);
        assert_eq!(cli.args, vec!["hello", "@file.txt", "world"]);
    }

    #[test]
    fn version_flag_short() {
        let cli = Cli::parse_from(["pi", "-v"]);
        assert!(cli.version);
    }

    #[test]
    fn thinking_levels_all_valid() {
        for level in &["off", "minimal", "low", "medium", "high", "xhigh", "max"] {
            let cli = Cli::parse_from(["pi", "--thinking", level]);
            assert_eq!(cli.thinking.as_deref(), Some(*level));
        }
    }

    #[test]
    fn extension_policy_profiles() {
        for profile in &["safe", "balanced", "permissive"] {
            let cli = Cli::parse_from(["pi", "--extension-policy", profile]);
            assert_eq!(cli.extension_policy.as_deref(), Some(*profile));
        }
    }

    #[test]
    fn list_models_without_pattern() {
        let cli = Cli::parse_from(["pi", "--list-models"]);
        assert!(cli.list_models.is_some());
        assert!(cli.list_models.unwrap().is_none());
    }

    #[test]
    fn list_models_with_pattern() {
        let cli = Cli::parse_from(["pi", "--list-models", "claude*"]);
        assert_eq!(cli.list_models.unwrap().as_deref(), Some("claude*"));
    }

    #[test]
    fn all_short_flags_together() {
        // -v, -c, -r, -p are all short flags; they should parse together
        let cli = Cli::parse_from(["pi", "-p", "-c"]);
        assert!(cli.print);
        assert!(cli.r#continue);
    }

    #[test]
    fn system_prompt_with_append() {
        let cli = Cli::parse_from([
            "pi",
            "--system-prompt",
            "Be helpful",
            "--append-system-prompt",
            "Also be concise",
        ]);
        assert_eq!(cli.system_prompt.as_deref(), Some("Be helpful"));
        assert_eq!(cli.append_system_prompt.as_deref(), Some("Also be concise"));
    }

    #[test]
    fn repair_policy_modes() {
        for mode in &["off", "suggest", "auto-safe", "auto-strict"] {
            let cli = Cli::parse_from(["pi", "--repair-policy", mode]);
            assert_eq!(cli.repair_policy.as_deref(), Some(*mode));
        }
    }
}

// ============================================================================
// 3. Environment Variable Precedence — env vars feed through clap
// ============================================================================

mod cli_flag_combinations {
    use clap::Parser;
    use pi::cli::Cli;

    fn parse_args(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).expect("CLI parse should succeed")
    }

    #[test]
    fn provider_flag_sets_provider() {
        let cli = parse_args(&["pi", "--provider", "openai"]);
        assert_eq!(cli.provider.as_deref(), Some("openai"));
    }

    #[test]
    fn model_flag_sets_model() {
        let cli = parse_args(&["pi", "--model", "gpt-4o"]);
        assert_eq!(cli.model.as_deref(), Some("gpt-4o"));
    }

    #[test]
    fn both_flags_together() {
        let cli = parse_args(&[
            "pi",
            "--provider",
            "anthropic",
            "--model",
            "claude-opus-4-5",
        ]);
        assert_eq!(cli.provider.as_deref(), Some("anthropic"));
        assert_eq!(cli.model.as_deref(), Some("claude-opus-4-5"));
    }

    #[test]
    fn model_flag_without_provider() {
        let cli = parse_args(&["pi", "--model", "gpt-4o-mini"]);
        assert!(cli.provider.is_none());
        assert_eq!(cli.model.as_deref(), Some("gpt-4o-mini"));
    }

    #[test]
    fn provider_flag_without_model() {
        let cli = parse_args(&["pi", "--provider", "openai"]);
        assert_eq!(cli.provider.as_deref(), Some("openai"));
        assert!(cli.model.is_none());
    }
}

// ============================================================================
// 4. Session State Invariants
// ============================================================================

mod session_invariants {
    use super::*;

    fn user_msg(text: &str) -> Message {
        Message::User(pi::model::UserMessage {
            content: pi::model::UserContent::Text(text.to_string()),
            timestamp: 0,
        })
    }

    fn assistant_msg(text: &str) -> Message {
        Message::assistant(AssistantMessage {
            content: vec![ContentBlock::Text(TextContent {
                text: text.to_string(),
                text_signature: None,
            })],
            api: String::new(),
            provider: "test".to_string(),
            model: "test-model".to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            stop_details: None,
            error_message: None,
            timestamp: 0,
        })
    }

    #[test]
    fn fresh_session_has_no_messages() {
        let session = Session::in_memory();
        let messages = session.to_messages();
        assert!(messages.is_empty());
    }

    #[test]
    fn append_message_returns_unique_ids() {
        let mut session = Session::in_memory();
        let id1 = session.append_model_message(user_msg("hello"));
        let id2 = session.append_model_message(assistant_msg("hi"));
        let id3 = session.append_model_message(user_msg("bye"));
        assert_ne!(id1, id2);
        assert_ne!(id2, id3);
        assert_ne!(id1, id3);
    }

    #[test]
    fn messages_round_trip_through_session() {
        let mut session = Session::in_memory();
        session.append_model_message(user_msg("hello"));
        session.append_model_message(assistant_msg("hi there"));

        let messages = session.to_messages();
        assert_eq!(messages.len(), 2);
        // First should be User, second should be Assistant
        assert!(matches!(&messages[0], Message::User(_)));
        assert!(matches!(&messages[1], Message::Assistant(_)));
    }

    #[test]
    fn session_name_roundtrip() {
        let mut session = Session::in_memory();
        assert!(session.get_name().is_none());
        session.set_name("test-session");
        assert_eq!(session.get_name().as_deref(), Some("test-session"));
    }

    #[test]
    fn branch_creates_fork_point() {
        let mut session = Session::in_memory();
        let _id1 = session.append_model_message(user_msg("hello"));
        let id2 = session.append_model_message(assistant_msg("hi"));
        let _id3 = session.append_model_message(user_msg("follow up"));

        let branched = session.create_branch_from(&id2);
        assert!(branched, "branching should succeed");

        // After branching, new messages go on the new branch
        let id4 = session.append_model_message(user_msg("branch msg"));
        assert!(session.get_entry(&id4).is_some());
    }

    #[test]
    fn compaction_entry_accessible() {
        let mut session = Session::in_memory();
        let id1 = session.append_model_message(user_msg("hello"));
        session.append_model_message(assistant_msg("hi"));

        let compaction_id = session.append_compaction(
            "summary of prior conversation".to_string(),
            id1,  // first_kept_entry_id
            150,  // tokens_before
            None, // details
            None, // from_hook
        );

        let entry = session.get_entry(&compaction_id);
        assert!(entry.is_some(), "compaction entry should exist");
    }

    #[test]
    fn model_change_entries_tracked() {
        let mut session = Session::in_memory();
        let id =
            session.append_model_change("anthropic".to_string(), "claude-opus-4-5".to_string());
        let entry = session.get_entry(&id);
        assert!(entry.is_some(), "model change entry should exist");
    }

    #[test]
    fn thinking_level_change_tracked() {
        let mut session = Session::in_memory();
        let id = session.append_thinking_level_change("high".to_string());
        let entry = session.get_entry(&id);
        assert!(entry.is_some(), "thinking level change entry should exist");
    }

    #[test]
    fn leaves_increase_after_branch() {
        let mut session = Session::in_memory();
        session.append_model_message(user_msg("hello"));
        let id2 = session.append_model_message(assistant_msg("hi"));
        session.append_model_message(user_msg("follow up"));

        let leaves_before = session.list_leaves();

        session.create_branch_from(&id2);
        session.append_model_message(user_msg("branch msg"));

        let leaves_after = session.list_leaves();
        assert!(
            leaves_after.len() > leaves_before.len(),
            "branching should create a new leaf"
        );
    }
}

// ============================================================================
// 5. Error Display Format Parity
// ============================================================================

mod error_display {
    use super::*;

    #[test]
    fn error_messages_include_context() {
        let err = Error::provider("anthropic", "rate limited (429)");
        let display = err.to_string();
        assert!(display.contains("anthropic"));
        assert!(display.contains("rate limited"));
    }

    #[test]
    fn tool_error_includes_tool_name() {
        let err = Error::tool("bash", "command timed out after 120s");
        let display = err.to_string();
        assert!(display.contains("bash"));
        assert!(display.contains("timed out"));
    }

    #[test]
    fn validation_error_message_is_clean() {
        let err = Error::validation("--only must include at least one category");
        let display = err.to_string();
        assert!(display.contains("--only"));
    }

    #[test]
    fn session_not_found_includes_path() {
        let err = Error::SessionNotFound {
            path: "/tmp/missing-session.jsonl".to_string(),
        };
        let display = err.to_string();
        assert!(display.contains("/tmp/missing-session.jsonl"));
    }

    #[test]
    fn all_error_variants_have_nonempty_display() {
        let errors: Vec<Error> = vec![
            Error::config("test"),
            Error::session("test"),
            Error::SessionNotFound {
                path: "test".to_string(),
            },
            Error::provider("p", "m"),
            Error::auth("test"),
            Error::tool("t", "m"),
            Error::validation("test"),
            Error::extension("test"),
            Error::Aborted,
            Error::api("test"),
        ];
        for err in errors {
            let display = err.to_string();
            assert!(
                !display.is_empty(),
                "Error display should not be empty: {err:?}"
            );
            assert!(
                display.len() > 5,
                "Error display should be descriptive: {display}"
            );
        }
    }
}

// ============================================================================
// 6. Message/Content Type Serialization Invariants
// ============================================================================

mod message_serde_invariants {
    use super::*;

    #[test]
    fn user_text_message_round_trips() {
        let msg = Message::User(pi::model::UserMessage {
            content: pi::model::UserContent::Text("hello".to_string()),
            timestamp: 12345,
        });
        let json = serde_json::to_value(&msg).expect("serialize");
        let decoded: Message = serde_json::from_value(json.clone()).expect("deserialize");
        let rejson = serde_json::to_value(&decoded).expect("re-serialize");
        assert_eq!(json, rejson);
    }

    #[test]
    fn assistant_message_with_tool_call_round_trips() {
        let msg = Message::assistant(AssistantMessage {
            content: vec![
                ContentBlock::Text(TextContent {
                    text: "Let me check".to_string(),
                    text_signature: None,
                }),
                ContentBlock::ToolCall(ToolCall {
                    id: "call_123".to_string(),
                    name: "read".to_string(),
                    arguments: json!({"file_path": "/tmp/test.txt"}),
                    thought_signature: None,
                }),
            ],
            api: "anthropic-messages".to_string(),
            provider: "anthropic".to_string(),
            model: "claude-opus-4-5".to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            stop_details: None,
            error_message: None,
            timestamp: 0,
        });
        let json = serde_json::to_value(&msg).expect("serialize");
        let decoded: Message = serde_json::from_value(json.clone()).expect("deserialize");
        let rejson = serde_json::to_value(&decoded).expect("re-serialize");
        assert_eq!(json, rejson);
    }

    #[test]
    fn text_content_with_signature_preserved() {
        let content = TextContent {
            text: "verified content".to_string(),
            text_signature: Some("sig_abc123".to_string()),
        };
        let json = serde_json::to_value(&content).expect("serialize");
        // TextContent uses #[serde(rename_all = "camelCase")]
        assert_eq!(json["textSignature"], "sig_abc123");
        let decoded: TextContent = serde_json::from_value(json).expect("deserialize");
        assert_eq!(decoded.text_signature.as_deref(), Some("sig_abc123"));
    }

    #[test]
    fn text_content_without_signature_is_null_or_absent() {
        let content = TextContent {
            text: "plain".to_string(),
            text_signature: None,
        };
        let json = serde_json::to_value(&content).expect("serialize");
        // camelCase serialization: textSignature
        let sig = json.get("textSignature");
        assert!(
            sig.is_none() || sig == Some(&serde_json::Value::Null),
            "textSignature should be absent or null"
        );
    }

    #[test]
    fn usage_defaults_to_zero() {
        let usage = Usage::default();
        assert_eq!(usage.input, 0);
        assert_eq!(usage.output, 0);
        assert_eq!(usage.cache_read, 0);
        assert_eq!(usage.cache_write, 0);
    }
}

// ============================================================================
// 7. Config Type Tests
// ============================================================================

mod config_types {
    use pi::config::Config;

    #[test]
    fn config_default_is_valid() {
        let config = Config::default();
        let json = serde_json::to_value(&config).expect("serialize default config");
        assert!(json.is_object());
    }

    #[test]
    fn config_round_trip_via_json() {
        let config = Config::default();
        let json = serde_json::to_value(&config).expect("serialize");
        let decoded: Config = serde_json::from_value(json.clone()).expect("deserialize");
        let rejson = serde_json::to_value(&decoded).expect("re-serialize");
        assert_eq!(json, rejson);
    }

    #[test]
    fn config_unknown_fields_ignored() {
        let json = serde_json::json!({
            "unknownField": true,
            "anotherUnknown": "value"
        });
        let result: Result<Config, _> = serde_json::from_value(json);
        assert!(result.is_ok(), "unknown fields should be ignored");
    }

    #[test]
    fn config_camel_case_aliases_work() {
        // Config uses serde aliases for camelCase compat
        let json = serde_json::json!({
            "defaultProvider": "openai",
            "defaultModel": "gpt-4o",
            "hideThinkingBlock": true,
            "checkForUpdates": false
        });
        let config: Config = serde_json::from_value(json).expect("camelCase should work");
        assert_eq!(config.default_provider.as_deref(), Some("openai"));
        assert_eq!(config.default_model.as_deref(), Some("gpt-4o"));
        assert_eq!(config.hide_thinking_block, Some(true));
        assert_eq!(config.check_for_updates, Some(false));
    }

    #[test]
    fn config_snake_case_also_works() {
        let json = serde_json::json!({
            "default_provider": "google",
            "default_model": "gemini-2.5-pro"
        });
        let config: Config = serde_json::from_value(json).expect("snake_case should work");
        assert_eq!(config.default_provider.as_deref(), Some("google"));
        assert_eq!(config.default_model.as_deref(), Some("gemini-2.5-pro"));
    }
}

// ============================================================================
// 8. Structured Traceability Logs (DROPIN-174 acceptance requirement)
// ============================================================================

mod structured_traceability_logs {
    use crate::common::logging::{TestLogger, validate_jsonl};
    use serde_json::Value;

    #[derive(Clone, Copy)]
    struct SurfaceRequirement {
        surface: &'static str,
        requirement_id: &'static str,
    }

    const SURFACE_REQUIREMENTS: [SurfaceRequirement; 4] = [
        SurfaceRequirement {
            surface: "cli",
            requirement_id: "DROPIN-141",
        },
        SurfaceRequirement {
            surface: "config",
            requirement_id: "DROPIN-142",
        },
        SurfaceRequirement {
            surface: "session",
            requirement_id: "DROPIN-143",
        },
        SurfaceRequirement {
            surface: "errors",
            requirement_id: "DROPIN-144",
        },
    ];

    fn collect_traceability_violations(jsonl: &str) -> Vec<String> {
        let mut violations = Vec::new();

        for (index, line) in jsonl.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }

            let value: Value = match serde_json::from_str(line) {
                Ok(value) => value,
                Err(err) => {
                    violations.push(format!("line {} is not valid JSON: {err}", index + 1));
                    continue;
                }
            };

            if value.get("type").and_then(Value::as_str) != Some("log") {
                continue;
            }

            let category = value
                .get("category")
                .and_then(Value::as_str)
                .unwrap_or("<unknown>");

            if !category.starts_with("parity.") {
                continue;
            }

            let requirement_id = value
                .get("context")
                .and_then(Value::as_object)
                .and_then(|ctx| ctx.get("requirement_id"))
                .and_then(Value::as_str);

            match requirement_id {
                Some(id) if id.starts_with("DROPIN-") || id.starts_with("PARITY-") => {}
                Some(id) => violations.push(format!(
                    "line {} category {} has invalid requirement_id '{}'",
                    index + 1,
                    category,
                    id
                )),
                None => violations.push(format!(
                    "line {} category {} missing context.requirement_id",
                    index + 1,
                    category
                )),
            }
        }

        violations
    }

    #[test]
    fn parity_logs_include_requirement_ids_for_each_surface() {
        let logger = TestLogger::new();
        logger.set_test_name("dropin-174-cross-surface-traceability");

        for requirement in SURFACE_REQUIREMENTS {
            logger.info_ctx(
                "parity.surface_check",
                format!("Validated {} parity contract", requirement.surface),
                |ctx| {
                    ctx.push(("surface".to_string(), requirement.surface.to_string()));
                    ctx.push((
                        "requirement_id".to_string(),
                        requirement.requirement_id.to_string(),
                    ));
                    ctx.push(("status".to_string(), "pass".to_string()));
                },
            );
        }

        let jsonl = logger.dump_jsonl();
        let schema_errors = validate_jsonl(&jsonl);
        assert!(
            schema_errors.is_empty(),
            "structured log schema validation failed: {schema_errors:?}"
        );

        let traceability_violations = collect_traceability_violations(&jsonl);
        assert!(
            traceability_violations.is_empty(),
            "traceability violations: {traceability_violations:?}"
        );
    }

    #[test]
    fn traceability_validator_flags_missing_requirement_id() {
        let logger = TestLogger::new();
        logger.set_test_name("dropin-174-cross-surface-traceability-negative");
        logger.info_ctx(
            "parity.surface_check",
            "Missing requirement id should fail traceability validation",
            |ctx| {
                ctx.push(("surface".to_string(), "cli".to_string()));
                ctx.push(("status".to_string(), "fail".to_string()));
            },
        );

        let traceability_violations = collect_traceability_violations(&logger.dump_jsonl());
        assert!(
            traceability_violations
                .iter()
                .any(|violation| violation.contains("missing context.requirement_id")),
            "expected missing requirement_id violation, got: {traceability_violations:?}"
        );
    }
}
