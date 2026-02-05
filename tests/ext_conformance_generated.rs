//! Auto-generated conformance tests for all extensions in the validated manifest (bd-15jg).
//!
//! Each extension in `tests/ext_conformance/VALIDATED_MANIFEST.json` gets its own `#[test]`
//! function via the `conformance_test!` macro. This provides:
//! - Parallelism (cargo test runs tests in parallel)
//! - Isolation (one failure does not block others)
//! - Clear reporting (each extension shows as pass/fail in test output)
//!
//! Tiers 1–2 run by default; tiers 3–5 are `#[ignore]` (multi-file, npm deps, UI, platform).
//!
//! Run all (including ignored):
//!   cargo test --test `ext_conformance_generated` -- --include-ignored
//!
//! Run only tier 1–2 (default):
//!   cargo test --test `ext_conformance_generated`

mod common;

use pi::extensions::{ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::tools::ToolRegistry;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

// ─── Manifest types ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct ManifestEntry {
    id: String,
    entry_path: String,
    conformance_tier: u32,
    capabilities: Capabilities,
    registrations: Registrations,
}

#[derive(Debug, Clone)]
#[allow(dead_code, clippy::struct_excessive_bools)]
struct Capabilities {
    registers_tools: bool,
    registers_commands: bool,
    registers_flags: bool,
    registers_providers: bool,
    subscribes_events: Vec<String>,
    is_multi_file: bool,
    has_npm_deps: bool,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct Registrations {
    tools: Vec<String>,
    commands: Vec<String>,
    flags: Vec<String>,
    event_handlers: Vec<String>,
}

#[derive(Debug)]
struct Manifest {
    extensions: Vec<ManifestEntry>,
}

impl Manifest {
    fn find(&self, ext_id: &str) -> Option<&ManifestEntry> {
        self.extensions.iter().find(|e| e.id == ext_id)
    }
}

// ─── Manifest loading ───────────────────────────────────────────────────────

fn artifacts_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("ext_conformance")
        .join("artifacts")
}

fn manifest_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("ext_conformance")
        .join("VALIDATED_MANIFEST.json")
}

fn load_manifest() -> &'static Manifest {
    static MANIFEST: OnceLock<Manifest> = OnceLock::new();
    MANIFEST.get_or_init(|| {
        let data = std::fs::read_to_string(manifest_path())
            .expect("Failed to read VALIDATED_MANIFEST.json");
        let json: Value =
            serde_json::from_str(&data).expect("Failed to parse VALIDATED_MANIFEST.json");

        let extensions = json["extensions"]
            .as_array()
            .expect("manifest.extensions should be an array")
            .iter()
            .map(|e| {
                let caps = &e["capabilities"];
                let regs = &e["registrations"];
                ManifestEntry {
                    id: e["id"].as_str().unwrap_or("").to_string(),
                    entry_path: e["entry_path"].as_str().unwrap_or("").to_string(),
                    conformance_tier: u32::try_from(e["conformance_tier"].as_u64().unwrap_or(0))
                        .unwrap_or(0),
                    capabilities: Capabilities {
                        registers_tools: caps["registers_tools"].as_bool().unwrap_or(false),
                        registers_commands: caps["registers_commands"].as_bool().unwrap_or(false),
                        registers_flags: caps["registers_flags"].as_bool().unwrap_or(false),
                        registers_providers: caps["registers_providers"].as_bool().unwrap_or(false),
                        subscribes_events: caps["subscribes_events"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_str().map(String::from))
                                    .collect()
                            })
                            .unwrap_or_default(),
                        is_multi_file: caps["is_multi_file"].as_bool().unwrap_or(false),
                        has_npm_deps: caps["has_npm_deps"].as_bool().unwrap_or(false),
                    },
                    registrations: Registrations {
                        tools: regs["tools"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_str().map(String::from))
                                    .collect()
                            })
                            .unwrap_or_default(),
                        commands: regs["commands"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_str().map(String::from))
                                    .collect()
                            })
                            .unwrap_or_default(),
                        flags: regs["flags"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_str().map(String::from))
                                    .collect()
                            })
                            .unwrap_or_default(),
                        event_handlers: regs["event_handlers"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_str().map(String::from))
                                    .collect()
                            })
                            .unwrap_or_default(),
                    },
                }
            })
            .collect();

        Manifest { extensions }
    })
}

// ─── Core conformance test runner ───────────────────────────────────────────

/// Load an extension from the artifacts directory, validate registrations match
/// the manifest, and capture a registration snapshot as a JSON artifact.
#[allow(clippy::too_many_lines)]
fn run_conformance_test(ext_id: &str) {
    let manifest = load_manifest();
    let Some(entry) = manifest.find(ext_id) else {
        unreachable!("Extension '{ext_id}' not found in VALIDATED_MANIFEST.json");
    };

    let harness = common::TestHarness::new(format!("conformance_{}", ext_id.replace('/', "_")));
    let cwd = harness.temp_dir().to_path_buf();

    // Resolve the extension entry file.
    let entry_file = artifacts_dir().join(&entry.entry_path);
    assert!(
        entry_file.exists(),
        "Extension artifact not found: {}",
        entry_file.display()
    );

    harness
        .log()
        .info_ctx("conformance", "Loading extension", |ctx| {
            ctx.push(("ext_id".into(), ext_id.to_string()));
            ctx.push(("entry_path".into(), entry.entry_path.clone()));
            ctx.push(("tier".into(), entry.conformance_tier.to_string()));
        });

    // Create the load spec.
    let spec = JsExtensionLoadSpec::from_entry_path(&entry_file).unwrap_or_else(|e| {
        unreachable!(
            "Failed to create JsExtensionLoadSpec for '{ext_id}' at {}: {e}",
            entry_file.display()
        )
    });

    // Start JS runtime and load the extension.
    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let js_config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        ..Default::default()
    };

    let runtime = common::run_async({
        let manager = manager.clone();
        let tools = Arc::clone(&tools);
        async move {
            JsExtensionRuntimeHandle::start(js_config, tools, manager)
                .await
                .expect("start JS runtime")
        }
    });
    manager.set_js_runtime(runtime);

    let ext_id_owned = ext_id.to_string();
    common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .load_js_extensions(vec![spec])
                .await
                .unwrap_or_else(|e| unreachable!("Failed to load extension '{ext_id_owned}': {e}"));
        }
    });

    harness
        .log()
        .info("conformance", "Extension loaded successfully");

    // Capture registration snapshot.
    let snapshot = snapshot_registrations(&manager);

    // Write snapshot artifact.
    let snapshot_path = harness.temp_path("registration_snapshot.json");
    let snapshot_json =
        serde_json::to_string_pretty(&snapshot).expect("serialize registration snapshot");
    std::fs::write(&snapshot_path, &snapshot_json).expect("write snapshot");
    harness.record_artifact("registration_snapshot", &snapshot_path);

    // ── Validate registrations against manifest ──

    // Commands: verify every manifest-listed command was registered.
    let actual_commands = manager.list_commands();
    let actual_cmd_names: Vec<&str> = actual_commands
        .iter()
        .filter_map(|v| v.get("name").and_then(Value::as_str))
        .collect();

    for expected_cmd in &entry.registrations.commands {
        assert!(
            actual_cmd_names.contains(&expected_cmd.as_str()),
            "Extension '{ext_id}': expected command '{expected_cmd}' not found in actual commands: {actual_cmd_names:?}"
        );
    }

    // If the manifest says the extension registers commands, verify at least one was captured.
    if entry.capabilities.registers_commands && !entry.registrations.commands.is_empty() {
        assert!(
            !actual_commands.is_empty(),
            "Extension '{ext_id}': manifest says it registers commands, but none were captured"
        );
    }

    // Flags: verify expected flags were registered.
    let actual_flags = manager.list_flags();
    let actual_flag_names: Vec<&str> = actual_flags
        .iter()
        .filter_map(|v| v.get("name").and_then(Value::as_str))
        .collect();

    for expected_flag in &entry.registrations.flags {
        assert!(
            actual_flag_names.contains(&expected_flag.as_str()),
            "Extension '{ext_id}': expected flag '{expected_flag}' not found in actual flags: {actual_flag_names:?}"
        );
    }

    // Tools: if manifest says the extension registers tools, verify tool defs exist.
    if entry.capabilities.registers_tools {
        let tool_defs = manager.extension_tool_defs();
        assert!(
            !tool_defs.is_empty(),
            "Extension '{ext_id}': manifest says it registers tools, but no tool defs were captured"
        );
    }

    // Providers: if manifest says it registers providers, verify they exist.
    if entry.capabilities.registers_providers {
        let providers = manager.extension_providers();
        assert!(
            !providers.is_empty(),
            "Extension '{ext_id}': manifest says it registers providers, but none were captured"
        );
    }

    harness
        .log()
        .info_ctx("conformance", "Validation passed", |ctx| {
            ctx.push(("commands".into(), actual_commands.len().to_string()));
            ctx.push(("flags".into(), actual_flags.len().to_string()));
            ctx.push((
                "tool_defs".into(),
                manager.extension_tool_defs().len().to_string(),
            ));
            ctx.push((
                "providers".into(),
                manager.extension_providers().len().to_string(),
            ));
        });

    // Write JSONL logs.
    let logs_path = harness.temp_path("test_logs.jsonl");
    if let Err(e) = harness.write_jsonl_logs_normalized(&logs_path) {
        harness
            .log()
            .warn("jsonl", format!("Failed to write JSONL logs: {e}"));
    } else {
        harness.record_artifact("jsonl_logs", &logs_path);
    }
}

/// Capture the current registration state as a JSON snapshot.
fn snapshot_registrations(manager: &ExtensionManager) -> Value {
    let commands = manager.list_commands();
    let shortcuts = manager.list_shortcuts();
    let flags = manager.list_flags();
    let providers = manager.extension_providers();
    let tool_defs = manager.extension_tool_defs();
    let models: Vec<Value> = manager
        .extension_model_entries()
        .into_iter()
        .map(|entry| serde_json::to_value(entry.model).expect("model to json"))
        .collect();

    serde_json::json!({
        "commands": commands,
        "shortcuts": shortcuts,
        "flags": flags,
        "providers": providers,
        "tool_defs": tool_defs,
        "models": models,
    })
}

// ─── Macro ──────────────────────────────────────────────────────────────────

/// Generate a conformance test for a single extension.
macro_rules! conformance_test {
    ($name:ident, $ext_id:literal) => {
        #[test]
        fn $name() {
            run_conformance_test($ext_id);
        }
    };
    ($name:ident, $ext_id:literal, ignore) => {
        #[test]
        #[ignore = "TODO: enable once full extension conformance runner is supported"]
        fn $name() {
            run_conformance_test($ext_id);
        }
    };
}

// ─── Tier 1 — Single-file, no events, no deps (37 extensions) ──────────────

conformance_test!(ext_antigravity_image_gen, "antigravity-image-gen");
conformance_test!(ext_bash_spawn_hook, "bash-spawn-hook");
conformance_test!(ext_bookmark, "bookmark");
conformance_test!(
    ext_community_hjanuschka_clipboard,
    "community/hjanuschka-clipboard"
);
conformance_test!(
    ext_community_hjanuschka_cost_tracker,
    "community/hjanuschka-cost-tracker",
    ignore
);
conformance_test!(
    ext_community_hjanuschka_flicker_corp,
    "community/hjanuschka-flicker-corp"
);
conformance_test!(
    ext_community_hjanuschka_handoff,
    "community/hjanuschka-handoff",
    ignore
);
conformance_test!(
    ext_community_hjanuschka_memory_mode,
    "community/hjanuschka-memory-mode",
    ignore
);
conformance_test!(
    ext_community_hjanuschka_oracle,
    "community/hjanuschka-oracle",
    ignore
);
conformance_test!(
    ext_community_mitsuhiko_answer,
    "community/mitsuhiko-answer",
    ignore
);
conformance_test!(
    ext_community_tmustier_arcade_mario_not,
    "community/tmustier-arcade-mario-not",
    ignore
);
conformance_test!(
    ext_community_tmustier_arcade_picman,
    "community/tmustier-arcade-picman"
);
conformance_test!(
    ext_community_tmustier_arcade_ping,
    "community/tmustier-arcade-ping"
);
conformance_test!(
    ext_community_tmustier_arcade_spice_invaders,
    "community/tmustier-arcade-spice-invaders"
);
conformance_test!(
    ext_community_tmustier_arcade_tetris,
    "community/tmustier-arcade-tetris"
);
conformance_test!(
    ext_community_tmustier_tab_status,
    "community/tmustier-tab-status",
    ignore
);
conformance_test!(
    ext_community_tmustier_usage_extension,
    "community/tmustier-usage-extension",
    ignore
);
conformance_test!(ext_custom_footer, "custom-footer");
conformance_test!(ext_handoff, "handoff", ignore);
conformance_test!(ext_hello, "hello");
conformance_test!(ext_message_renderer, "message-renderer", ignore);
conformance_test!(
    ext_npm_benvargas_pi_ancestor_discovery,
    "npm/benvargas-pi-ancestor-discovery",
    ignore
);
conformance_test!(
    ext_npm_benvargas_pi_antigravity_image_gen,
    "npm/benvargas-pi-antigravity-image-gen",
    ignore
);
conformance_test!(ext_npm_pi_command_center, "npm/pi-command-center");
conformance_test!(ext_npm_pi_model_switch, "npm/pi-model-switch");
conformance_test!(ext_npm_pi_threads, "npm/pi-threads", ignore);
conformance_test!(ext_overlay_test, "overlay-test", ignore);
conformance_test!(ext_qna, "qna", ignore);
conformance_test!(ext_question, "question");
conformance_test!(ext_questionnaire, "questionnaire");
conformance_test!(ext_send_user_message, "send-user-message");
conformance_test!(ext_session_name, "session-name");
conformance_test!(ext_snake, "snake");
conformance_test!(ext_space_invaders, "space-invaders", ignore);
conformance_test!(ext_summarize, "summarize", ignore);
conformance_test!(
    ext_third_party_rytswd_questionnaire,
    "third-party/rytswd-questionnaire"
);
conformance_test!(ext_timed_confirm, "timed-confirm");

// ─── Tier 2 — Single-file with events (83 extensions) ──────────────────────

conformance_test!(ext_auto_commit_on_exit, "auto-commit-on-exit");
conformance_test!(ext_claude_rules, "claude-rules");
conformance_test!(
    ext_community_ferologics_notify,
    "community/ferologics-notify"
);
conformance_test!(
    ext_community_hjanuschka_funny_working_message,
    "community/hjanuschka-funny-working-message"
);
conformance_test!(
    ext_community_hjanuschka_loop,
    "community/hjanuschka-loop",
    ignore
);
conformance_test!(
    ext_community_hjanuschka_plan_mode,
    "community/hjanuschka-plan-mode",
    ignore
);
conformance_test!(
    ext_community_hjanuschka_resistance,
    "community/hjanuschka-resistance"
);
conformance_test!(
    ext_community_hjanuschka_speedreading,
    "community/hjanuschka-speedreading",
    ignore
);
conformance_test!(
    ext_community_hjanuschka_status_widget,
    "community/hjanuschka-status-widget"
);
conformance_test!(
    ext_community_hjanuschka_ultrathink,
    "community/hjanuschka-ultrathink"
);
conformance_test!(
    ext_community_hjanuschka_usage_bar,
    "community/hjanuschka-usage-bar",
    ignore
);
conformance_test!(
    ext_community_mitsuhiko_cwd_history,
    "community/mitsuhiko-cwd-history",
    ignore
);
conformance_test!(
    ext_community_mitsuhiko_files,
    "community/mitsuhiko-files",
    ignore
);
conformance_test!(
    ext_community_mitsuhiko_loop,
    "community/mitsuhiko-loop",
    ignore
);
conformance_test!(ext_community_mitsuhiko_notify, "community/mitsuhiko-notify");
conformance_test!(
    ext_community_mitsuhiko_review,
    "community/mitsuhiko-review",
    ignore
);
conformance_test!(
    ext_community_mitsuhiko_todos,
    "community/mitsuhiko-todos",
    ignore
);
conformance_test!(ext_community_mitsuhiko_uv, "community/mitsuhiko-uv", ignore);
conformance_test!(
    ext_community_mitsuhiko_whimsical,
    "community/mitsuhiko-whimsical"
);
conformance_test!(
    ext_community_nicobailon_rewind_hook,
    "community/nicobailon-rewind-hook",
    ignore
);
conformance_test!(
    ext_community_ogulcancelik_ghostty_theme_sync,
    "community/ogulcancelik-ghostty-theme-sync",
    ignore
);
conformance_test!(
    ext_community_prateekmedia_token_rate,
    "community/prateekmedia-token-rate"
);
conformance_test!(
    ext_community_qualisero_background_notify,
    "community/qualisero-background-notify",
    ignore
);
conformance_test!(
    ext_community_qualisero_compact_config,
    "community/qualisero-compact-config",
    ignore
);
conformance_test!(
    ext_community_qualisero_safe_git,
    "community/qualisero-safe-git",
    ignore
);
conformance_test!(
    ext_community_qualisero_safe_rm,
    "community/qualisero-safe-rm"
);
conformance_test!(
    ext_community_qualisero_session_color,
    "community/qualisero-session-color"
);
conformance_test!(
    ext_community_qualisero_session_emoji,
    "community/qualisero-session-emoji",
    ignore
);
conformance_test!(
    ext_community_tmustier_agent_guidance,
    "community/tmustier-agent-guidance"
);
conformance_test!(
    ext_community_tmustier_ralph_wiggum,
    "community/tmustier-ralph-wiggum"
);
conformance_test!(
    ext_community_tmustier_raw_paste,
    "community/tmustier-raw-paste",
    ignore
);
conformance_test!(ext_confirm_destructive, "confirm-destructive");
conformance_test!(ext_custom_compaction, "custom-compaction", ignore);
conformance_test!(ext_custom_header, "custom-header", ignore);
conformance_test!(ext_dirty_repo_guard, "dirty-repo-guard");
conformance_test!(ext_dynamic_resources, "dynamic-resources");
conformance_test!(ext_event_bus, "event-bus");
conformance_test!(ext_file_trigger, "file-trigger");
conformance_test!(ext_git_checkpoint, "git-checkpoint");
conformance_test!(ext_inline_bash, "inline-bash");
conformance_test!(ext_input_transform, "input-transform");
conformance_test!(ext_interactive_shell, "interactive-shell", ignore);
conformance_test!(ext_mac_system_theme, "mac-system-theme", ignore);
conformance_test!(ext_modal_editor, "modal-editor", ignore);
conformance_test!(ext_model_status, "model-status");
conformance_test!(ext_notify, "notify");
conformance_test!(
    ext_npm_ogulcancelik_pi_sketch,
    "npm/ogulcancelik-pi-sketch",
    ignore
);
conformance_test!(ext_npm_pi_ephemeral, "npm/pi-ephemeral", ignore);
conformance_test!(
    ext_npm_pi_ghostty_theme_sync,
    "npm/pi-ghostty-theme-sync",
    ignore
);
conformance_test!(ext_npm_pi_md_export, "npm/pi-md-export", ignore);
conformance_test!(ext_npm_pi_notify, "npm/pi-notify");
conformance_test!(ext_npm_pi_poly_notify, "npm/pi-poly-notify", ignore);
conformance_test!(
    ext_npm_pi_prompt_template_model,
    "npm/pi-prompt-template-model",
    ignore
);
conformance_test!(ext_npm_pi_session_ask, "npm/pi-session-ask", ignore);
conformance_test!(ext_npm_pi_skill_palette, "npm/pi-skill-palette", ignore);
conformance_test!(ext_npm_pi_voice_of_god, "npm/pi-voice-of-god");
conformance_test!(ext_npm_token_rate_pi, "npm/token-rate-pi");
conformance_test!(ext_npm_vpellegrino_pi_skills, "npm/vpellegrino-pi-skills");
conformance_test!(ext_overlay_qa_tests, "overlay-qa-tests");
conformance_test!(ext_permission_gate, "permission-gate");
conformance_test!(ext_pirate, "pirate");
conformance_test!(ext_preset, "preset", ignore);
conformance_test!(ext_protected_paths, "protected-paths");
conformance_test!(ext_rainbow_editor, "rainbow-editor", ignore);
conformance_test!(ext_rpc_demo, "rpc-demo");
conformance_test!(ext_shutdown_command, "shutdown-command");
conformance_test!(ext_ssh, "ssh", ignore);
conformance_test!(ext_status_line, "status-line");
conformance_test!(ext_system_prompt_header, "system-prompt-header");
conformance_test!(
    ext_third_party_graffioh_pi_screenshots_picker,
    "third-party/graffioh-pi-screenshots-picker",
    ignore
);
conformance_test!(
    ext_third_party_graffioh_pi_super_curl,
    "third-party/graffioh-pi-super-curl",
    ignore
);
conformance_test!(
    ext_third_party_jyaunches_pi_canvas,
    "third-party/jyaunches-pi-canvas"
);
conformance_test!(
    ext_third_party_lsj5031_pi_notification_extension,
    "third-party/lsj5031-pi-notification-extension",
    ignore
);
conformance_test!(
    ext_third_party_ogulcancelik_pi_sketch,
    "third-party/ogulcancelik-pi-sketch",
    ignore
);
conformance_test!(
    ext_third_party_raunovillberg_pi_stuffed,
    "third-party/raunovillberg-pi-stuffed"
);
conformance_test!(ext_third_party_rytswd_direnv, "third-party/rytswd-direnv");
conformance_test!(ext_titlebar_spinner, "titlebar-spinner");
conformance_test!(ext_todo, "todo");
conformance_test!(ext_tool_override, "tool-override");
conformance_test!(ext_tools, "tools", ignore);
conformance_test!(ext_trigger_compact, "trigger-compact");
conformance_test!(ext_truncated_tool, "truncated-tool", ignore);
conformance_test!(ext_widget_placement, "widget-placement");

// ─── Tier 3 — Multi-file / npm deps (79 extensions) ────────────────────────

conformance_test!(
    ext_community_jyaunches_canvas,
    "community/jyaunches-canvas",
    ignore
);
conformance_test!(
    ext_community_nicobailon_interactive_shell,
    "community/nicobailon-interactive-shell",
    ignore
);
conformance_test!(
    ext_community_nicobailon_mcp_adapter,
    "community/nicobailon-mcp-adapter",
    ignore
);
conformance_test!(
    ext_community_nicobailon_powerline_footer,
    "community/nicobailon-powerline-footer",
    ignore
);
conformance_test!(
    ext_community_nicobailon_subagents,
    "community/nicobailon-subagents",
    ignore
);
conformance_test!(
    ext_community_prateekmedia_checkpoint,
    "community/prateekmedia-checkpoint",
    ignore
);
conformance_test!(
    ext_community_prateekmedia_lsp,
    "community/prateekmedia-lsp",
    ignore
);
conformance_test!(
    ext_community_prateekmedia_permission,
    "community/prateekmedia-permission",
    ignore
);
conformance_test!(
    ext_community_prateekmedia_ralph_loop,
    "community/prateekmedia-ralph-loop",
    ignore
);
conformance_test!(
    ext_community_prateekmedia_repeat,
    "community/prateekmedia-repeat",
    ignore
);
conformance_test!(
    ext_community_qualisero_pi_agent_scip,
    "community/qualisero-pi-agent-scip",
    ignore
);
conformance_test!(
    ext_community_tmustier_code_actions,
    "community/tmustier-code-actions",
    ignore
);
conformance_test!(
    ext_community_tmustier_files_widget,
    "community/tmustier-files-widget",
    ignore
);
conformance_test!(
    ext_custom_provider_anthropic,
    "custom-provider-anthropic",
    ignore
);
conformance_test!(
    ext_custom_provider_gitlab_duo,
    "custom-provider-gitlab-duo",
    ignore
);
conformance_test!(
    ext_custom_provider_qwen_cli,
    "custom-provider-qwen-cli",
    ignore
);
conformance_test!(ext_doom_overlay, "doom-overlay", ignore);
conformance_test!(
    ext_npm_aliou_pi_extension_dev,
    "npm/aliou-pi-extension-dev",
    ignore
);
conformance_test!(
    ext_npm_aliou_pi_guardrails,
    "npm/aliou-pi-guardrails",
    ignore
);
conformance_test!(ext_npm_aliou_pi_linkup, "npm/aliou-pi-linkup", ignore);
conformance_test!(ext_npm_aliou_pi_processes, "npm/aliou-pi-processes", ignore);
conformance_test!(ext_npm_aliou_pi_synthetic, "npm/aliou-pi-synthetic", ignore);
conformance_test!(ext_npm_aliou_pi_toolchain, "npm/aliou-pi-toolchain", ignore);
conformance_test!(
    ext_npm_benvargas_pi_synthetic_provider,
    "npm/benvargas-pi-synthetic-provider",
    ignore
);
conformance_test!(ext_npm_checkpoint_pi, "npm/checkpoint-pi", ignore);
conformance_test!(
    ext_npm_imsus_pi_extension_minimax_coding_plan_mcp,
    "npm/imsus-pi-extension-minimax-coding-plan-mcp",
    ignore
);
conformance_test!(
    ext_npm_juanibiapina_pi_extension_settings,
    "npm/juanibiapina-pi-extension-settings",
    ignore
);
conformance_test!(
    ext_npm_juanibiapina_pi_files,
    "npm/juanibiapina-pi-files",
    ignore
);
conformance_test!(
    ext_npm_juanibiapina_pi_gob,
    "npm/juanibiapina-pi-gob",
    ignore
);
conformance_test!(ext_npm_lsp_pi, "npm/lsp-pi", ignore);
conformance_test!(
    ext_npm_marckrenn_pi_sub_bar,
    "npm/marckrenn-pi-sub-bar",
    ignore
);
conformance_test!(
    ext_npm_marckrenn_pi_sub_core,
    "npm/marckrenn-pi-sub-core",
    ignore
);
conformance_test!(ext_npm_permission_pi, "npm/permission-pi", ignore);
conformance_test!(
    ext_npm_pi_agentic_compaction,
    "npm/pi-agentic-compaction",
    ignore
);
conformance_test!(ext_npm_pi_amplike, "npm/pi-amplike", ignore);
conformance_test!(ext_npm_pi_bash_confirm, "npm/pi-bash-confirm", ignore);
conformance_test!(ext_npm_pi_brave_search, "npm/pi-brave-search", ignore);
conformance_test!(ext_npm_pi_mermaid, "npm/pi-mermaid", ignore);
conformance_test!(ext_npm_pi_messenger, "npm/pi-messenger", ignore);
conformance_test!(ext_npm_pi_moonshot, "npm/pi-moonshot", ignore);
conformance_test!(ext_npm_pi_multicodex, "npm/pi-multicodex", ignore);
conformance_test!(ext_npm_pi_repoprompt_mcp, "npm/pi-repoprompt-mcp", ignore);
conformance_test!(ext_npm_pi_review_loop, "npm/pi-review-loop", ignore);
conformance_test!(
    ext_npm_pi_screenshots_picker,
    "npm/pi-screenshots-picker",
    ignore
);
conformance_test!(ext_npm_pi_search_agent, "npm/pi-search-agent", ignore);
conformance_test!(ext_npm_pi_shadow_git, "npm/pi-shadow-git", ignore);
conformance_test!(
    ext_npm_pi_shell_completions,
    "npm/pi-shell-completions",
    ignore
);
conformance_test!(ext_npm_pi_subdir_context, "npm/pi-subdir-context", ignore);
conformance_test!(ext_npm_pi_super_curl, "npm/pi-super-curl", ignore);
conformance_test!(ext_npm_pi_telemetry_otel, "npm/pi-telemetry-otel", ignore);
conformance_test!(ext_npm_pi_wakatime, "npm/pi-wakatime", ignore);
conformance_test!(ext_npm_pi_watch, "npm/pi-watch", ignore);
conformance_test!(ext_npm_pi_web_access, "npm/pi-web-access", ignore);
conformance_test!(ext_npm_ralph_loop_pi, "npm/ralph-loop-pi", ignore);
conformance_test!(ext_npm_repeat_pi, "npm/repeat-pi", ignore);
conformance_test!(ext_npm_vaayne_agent_kit, "npm/vaayne-agent-kit", ignore);
conformance_test!(ext_npm_vaayne_pi_mcp, "npm/vaayne-pi-mcp", ignore);
conformance_test!(ext_npm_vaayne_pi_subagent, "npm/vaayne-pi-subagent", ignore);
conformance_test!(
    ext_npm_vaayne_pi_web_tools,
    "npm/vaayne-pi-web-tools",
    ignore
);
conformance_test!(ext_npm_walterra_pi_charts, "npm/walterra-pi-charts", ignore);
conformance_test!(
    ext_npm_walterra_pi_graphviz,
    "npm/walterra-pi-graphviz",
    ignore
);
conformance_test!(ext_npm_zenobius_pi_dcp, "npm/zenobius-pi-dcp", ignore);
conformance_test!(ext_plan_mode, "plan-mode", ignore);
conformance_test!(ext_sandbox, "sandbox", ignore);
conformance_test!(ext_subagent, "subagent", ignore);
conformance_test!(
    ext_third_party_aliou_pi_extensions,
    "third-party/aliou-pi-extensions",
    ignore
);
conformance_test!(
    ext_third_party_ben_vargas_pi_packages,
    "third-party/ben-vargas-pi-packages",
    ignore
);
conformance_test!(
    ext_third_party_charles_cooper_pi_extensions,
    "third-party/charles-cooper-pi-extensions",
    ignore
);
conformance_test!(
    ext_third_party_cv_pi_ssh_remote,
    "third-party/cv-pi-ssh-remote",
    ignore
);
conformance_test!(
    ext_third_party_limouren_agent_things,
    "third-party/limouren-agent-things",
    ignore
);
conformance_test!(
    ext_third_party_marckrenn_pi_sub,
    "third-party/marckrenn-pi-sub",
    ignore
);
conformance_test!(
    ext_third_party_michalvavra_agents,
    "third-party/michalvavra-agents",
    ignore
);
conformance_test!(
    ext_third_party_openclaw_openclaw,
    "third-party/openclaw-openclaw",
    ignore
);
conformance_test!(
    ext_third_party_pasky_pi_amplike,
    "third-party/pasky-pi-amplike",
    ignore
);
conformance_test!(
    ext_third_party_qualisero_pi_agent_scip,
    "third-party/qualisero-pi-agent-scip",
    ignore
);
conformance_test!(
    ext_third_party_rytswd_slow_mode,
    "third-party/rytswd-slow-mode",
    ignore
);
conformance_test!(
    ext_third_party_w_winter_dot314,
    "third-party/w-winter-dot314",
    ignore
);
conformance_test!(
    ext_third_party_zenobi_us_pi_dcp,
    "third-party/zenobi-us-pi-dcp",
    ignore
);
conformance_test!(ext_with_deps, "with-deps", ignore);

// ─── Tier 4 — UI-heavy extensions (2 extensions) ───────────────────────────

conformance_test!(
    ext_community_nicobailon_interview_tool,
    "community/nicobailon-interview-tool",
    ignore
);
conformance_test!(
    ext_third_party_vtemian_pi_config,
    "third-party/vtemian-pi-config",
    ignore
);

// ─── Tier 5 — Platform-specific (4 extensions) ─────────────────────────────

conformance_test!(
    ext_agents_mikeastock_extensions,
    "agents-mikeastock/extensions",
    ignore
);
conformance_test!(
    ext_community_mitsuhiko_control,
    "community/mitsuhiko-control",
    ignore
);
conformance_test!(ext_npm_pi_annotate, "npm/pi-annotate", ignore);
conformance_test!(
    ext_third_party_kcosr_pi_extensions,
    "third-party/kcosr-pi-extensions",
    ignore
);
