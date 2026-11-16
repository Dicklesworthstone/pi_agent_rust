//! E2E tests for library integration (markdown/syntax/structured-concurrency).
//!
//! Verifies that `rich_rust` markdown rendering, syntax highlighting, and
//! `ExtensionRegion` structured cleanup all work end-to-end.

mod common;

use common::TestHarness;
use pi::extensions::{ExtensionManager, ExtensionRegion};
use pi::theme::{Theme, looks_like_theme_path};
use pi::tui::PiConsole;
use proptest::prelude::*;
use std::fmt::Write;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create a `PiConsole` that forces color output and sinks to /dev/null.
fn test_console() -> PiConsole {
    PiConsole::with_color()
}

// ============================================================================
// 1. Markdown Rendering
// ============================================================================

#[test]
fn markdown_renders_without_panic() {
    let harness = TestHarness::new("markdown_renders_without_panic");
    let console = test_console();

    let markdown = "# Title\n\nSome **bold** and *italic* text.\n\n- Item 1\n- Item 2\n";
    harness.log().info_ctx("markdown", "render basic", |ctx| {
        ctx.push(("input_len".to_string(), markdown.len().to_string()));
    });

    // Should not panic; output goes to sink
    console.render_markdown(markdown);
}

#[test]
fn markdown_with_code_block_renders_without_panic() {
    let harness = TestHarness::new("markdown_with_code_block_renders_without_panic");
    let console = test_console();

    let markdown = "# Code Example\n\n```rust\nfn main() {\n    println!(\"hello\");\n}\n```\n";
    harness
        .log()
        .info_ctx("markdown", "render with code block", |ctx| {
            ctx.push(("input_len".to_string(), markdown.len().to_string()));
            ctx.push(("language".to_string(), "rust".to_string()));
        });

    console.render_markdown(markdown);
}

#[test]
fn markdown_with_multiple_code_blocks() {
    let harness = TestHarness::new("markdown_with_multiple_code_blocks");
    let console = test_console();

    let markdown = concat!(
        "# Multi-language\n\n",
        "Rust:\n```rust\nlet x = 42;\n```\n\n",
        "Python:\n```python\nx = 42\n```\n\n",
        "JavaScript:\n```javascript\nconst x = 42;\n```\n",
    );
    harness
        .log()
        .info_ctx("markdown", "multi-language blocks", |ctx| {
            ctx.push(("input_len".to_string(), markdown.len().to_string()));
        });

    console.render_markdown(markdown);
}

#[test]
fn markdown_empty_input() {
    let harness = TestHarness::new("markdown_empty_input");
    let console = test_console();

    harness.log().info_ctx("markdown", "empty input", |ctx| {
        ctx.push(("input".to_string(), "\"\"".to_string()));
    });

    console.render_markdown("");
}

#[test]
fn markdown_nested_formatting() {
    let harness = TestHarness::new("markdown_nested_formatting");
    let console = test_console();

    let markdown = concat!(
        "## Nested\n\n",
        "1. First item with **bold**\n",
        "2. Second with `inline code`\n",
        "3. Third with [a link](https://example.com)\n\n",
        "> A blockquote with *emphasis*\n\n",
        "---\n\n",
        "Final paragraph.\n",
    );
    harness
        .log()
        .info_ctx("markdown", "nested formatting", |ctx| {
            ctx.push(("input_len".to_string(), markdown.len().to_string()));
        });

    console.render_markdown(markdown);
}

#[test]
fn markdown_unterminated_fence_falls_back() {
    let harness = TestHarness::new("markdown_unterminated_fence_falls_back");
    let console = test_console();

    let markdown = "Start\n\n```rust\nfn broken() {\n  // never closed\n";
    harness
        .log()
        .info_ctx("markdown", "unterminated fence", |ctx| {
            ctx.push(("input_len".to_string(), markdown.len().to_string()));
        });

    // Unterminated fence should not crash; falls back to plain markdown
    console.render_markdown(markdown);
}

#[test]
fn markdown_only_code_block() {
    let harness = TestHarness::new("markdown_only_code_block");
    let console = test_console();

    let markdown = "```python\ndef hello():\n    print('world')\n```\n";
    harness
        .log()
        .info_ctx("markdown", "code block only", |ctx| {
            ctx.push(("language".to_string(), "python".to_string()));
        });

    console.render_markdown(markdown);
}

#[test]
fn markdown_typescript_code_block() {
    let harness = TestHarness::new("markdown_typescript_code_block");
    let console = test_console();

    let markdown = concat!(
        "```typescript\n",
        "interface User {\n",
        "  name: string;\n",
        "  age: number;\n",
        "}\n\n",
        "function greet(user: User): string {\n",
        "  return `Hello, ${user.name}!`;\n",
        "}\n",
        "```\n",
    );
    harness
        .log()
        .info_ctx("markdown", "typescript block", |ctx| {
            ctx.push(("language".to_string(), "typescript".to_string()));
        });

    console.render_markdown(markdown);
}

#[test]
fn markdown_large_document() {
    let harness = TestHarness::new("markdown_large_document");
    let console = test_console();

    // Generate a large markdown document with headings and code blocks
    let mut markdown = String::with_capacity(10_000);
    for i in 0..20 {
        let _ = write!(markdown, "## Section {i}\n\n");
        markdown.push_str("Paragraph with **bold** and *italic* text.\n\n");
        markdown.push_str("```rust\n");
        let _ = write!(
            markdown,
            "fn section_{i}() {{\n    println!(\"{i}\");\n}}\n"
        );
        markdown.push_str("```\n\n");
    }

    harness.log().info_ctx("markdown", "large document", |ctx| {
        ctx.push(("input_len".to_string(), markdown.len().to_string()));
        ctx.push(("sections".to_string(), "20".to_string()));
    });

    console.render_markdown(&markdown);
}

// ============================================================================
// 2. Theme + Glamour Integration
// ============================================================================

#[test]
fn theme_dark_produces_valid_glamour_config() {
    let harness = TestHarness::new("theme_dark_produces_valid_glamour_config");

    let theme = Theme::dark();
    let config = theme.glamour_style_config();

    harness
        .log()
        .info_ctx("theme", "dark glamour config", |ctx| {
            ctx.push(("theme_name".to_string(), theme.name.clone()));
            ctx.push((
                "has_document_color".to_string(),
                config.document.style.color.is_some().to_string(),
            ));
            ctx.push((
                "has_heading_color".to_string(),
                config.heading.style.color.is_some().to_string(),
            ));
        });

    // Dark theme should configure document + heading + link colors
    assert!(config.document.style.color.is_some());
    assert!(config.heading.style.color.is_some());
    assert!(config.link.color.is_some());
    assert!(config.code.style.color.is_some());
}

#[test]
fn theme_light_produces_valid_glamour_config() {
    let harness = TestHarness::new("theme_light_produces_valid_glamour_config");

    let theme = Theme::light();
    let config = theme.glamour_style_config();

    harness
        .log()
        .info_ctx("theme", "light glamour config", |ctx| {
            ctx.push(("theme_name".to_string(), theme.name.clone()));
            ctx.push((
                "has_document_color".to_string(),
                config.document.style.color.is_some().to_string(),
            ));
        });

    assert!(config.document.style.color.is_some());
    assert!(config.heading.style.color.is_some());
    assert!(config.code.style.color.is_some());
}

#[test]
fn theme_solarized_produces_valid_glamour_config() {
    let harness = TestHarness::new("theme_solarized_produces_valid_glamour_config");

    let theme = Theme::solarized();
    let config = theme.glamour_style_config();

    harness
        .log()
        .info_ctx("theme", "solarized glamour config", |ctx| {
            ctx.push(("theme_name".to_string(), theme.name.clone()));
        });

    assert!(config.document.style.color.is_some());
    assert!(config.heading.style.color.is_some());
    assert!(config.code.style.color.is_some());
}

#[test]
fn glamour_renders_markdown_with_theme() {
    let harness = TestHarness::new("glamour_renders_markdown_with_theme");

    let theme = Theme::dark();
    let config = theme.glamour_style_config();

    let markdown_input = "# Hello\n\nThis is a **test** with `code`.";
    let rendered = glamour::Renderer::new()
        .with_style_config(config)
        .with_word_wrap(80)
        .render(markdown_input);

    harness
        .log()
        .info_ctx("glamour", "render with theme", |ctx| {
            ctx.push(("input_len".to_string(), markdown_input.len().to_string()));
            ctx.push(("output_len".to_string(), rendered.len().to_string()));
            ctx.push(("output_empty".to_string(), rendered.is_empty().to_string()));
        });

    assert!(
        !rendered.is_empty(),
        "glamour should produce non-empty output"
    );
    // The rendered output should contain recognizable text content
    assert!(
        rendered.contains("Hello") || rendered.contains("test"),
        "rendered output should contain markdown content"
    );
}

#[test]
fn glamour_renders_code_blocks() {
    let harness = TestHarness::new("glamour_renders_code_blocks");

    let theme = Theme::dark();
    let config = theme.glamour_style_config();

    let markdown_input = "Some text:\n\n```\nlet x = 42;\n```\n";
    let rendered = glamour::Renderer::new()
        .with_style_config(config)
        .with_word_wrap(80)
        .render(markdown_input);

    harness
        .log()
        .info_ctx("glamour", "render code block", |ctx| {
            ctx.push(("output_len".to_string(), rendered.len().to_string()));
        });

    assert!(!rendered.is_empty());
}

#[test]
fn glamour_handles_empty_input() {
    let harness = TestHarness::new("glamour_handles_empty_input");

    let theme = Theme::dark();
    let config = theme.glamour_style_config();

    let rendered = glamour::Renderer::new()
        .with_style_config(config)
        .with_word_wrap(80)
        .render("");

    harness.log().info_ctx("glamour", "empty input", |ctx| {
        ctx.push(("output_len".to_string(), rendered.len().to_string()));
    });

    // Empty input should not panic (output may be empty or whitespace)
}

// ============================================================================
// 3. Theme Discovery and Loading
// ============================================================================

#[test]
fn theme_discover_from_temp_dirs() {
    let harness = TestHarness::new("theme_discover_from_temp_dirs");
    let global_dir = harness.create_dir("global");
    let project_dir = harness.create_dir("project");

    // Create theme directories
    let global_themes_dir = global_dir.join("themes");
    let project_themes_dir = project_dir.join("themes");
    std::fs::create_dir_all(&global_themes_dir).unwrap();
    std::fs::create_dir_all(&project_themes_dir).unwrap();

    // Write a theme file
    let theme_json = serde_json::to_string_pretty(&Theme::dark()).unwrap();
    std::fs::write(global_themes_dir.join("custom.json"), &theme_json).unwrap();

    let roots = pi::theme::ThemeRoots {
        global_dir,
        project_dir,
    };

    let discovered = Theme::discover_themes_with_roots(&roots);
    harness.log().info_ctx("theme", "discover themes", |ctx| {
        ctx.push(("discovered_count".to_string(), discovered.len().to_string()));
    });

    assert_eq!(discovered.len(), 1, "should find the custom theme file");
}

#[test]
fn theme_load_from_file() {
    let harness = TestHarness::new("theme_load_from_file");

    let dark = Theme::dark();
    let json = serde_json::to_string_pretty(&dark).unwrap();
    let theme_path = harness.create_file("test_theme.json", json.as_bytes());

    let loaded = Theme::load(&theme_path);

    harness.log().info_ctx("theme", "load from file", |ctx| {
        ctx.push(("path".to_string(), theme_path.display().to_string()));
        ctx.push(("loaded_ok".to_string(), loaded.is_ok().to_string()));
    });

    let loaded = loaded.expect("should load theme from file");
    assert_eq!(loaded.name, dark.name);
    assert_eq!(loaded.colors.foreground, dark.colors.foreground);
    assert_eq!(loaded.syntax.keyword, dark.syntax.keyword);
}

#[test]
fn theme_load_invalid_json_produces_error() {
    let harness = TestHarness::new("theme_load_invalid_json_produces_error");
    let path = harness.create_file("bad_theme.json", b"{ not valid json }}}");

    let result = Theme::load(&path);

    harness.log().info_ctx("theme", "invalid json", |ctx| {
        ctx.push(("is_err".to_string(), result.is_err().to_string()));
    });

    assert!(result.is_err(), "invalid JSON should produce an error");
}

#[test]
fn theme_round_trip_serialization() {
    let harness = TestHarness::new("theme_round_trip_serialization");

    for (name, theme) in [
        ("dark", Theme::dark()),
        ("light", Theme::light()),
        ("solarized", Theme::solarized()),
    ] {
        let json = serde_json::to_string(&theme).unwrap();
        let roundtripped: Theme = serde_json::from_str(&json).unwrap();

        harness.log().info_ctx("theme", "round trip", |ctx| {
            ctx.push(("theme".to_string(), name.to_string()));
            ctx.push(("json_len".to_string(), json.len().to_string()));
        });

        assert_eq!(roundtripped.name, theme.name);
        assert_eq!(roundtripped.colors.accent, theme.colors.accent);
        assert_eq!(roundtripped.syntax.keyword, theme.syntax.keyword);
        assert_eq!(roundtripped.ui.border, theme.ui.border);
    }
}

proptest! {
    #[test]
    fn prop_theme_path_detects_json_suffix(
        stem in "[A-Za-z0-9_-]{1,24}",
        left_ws in "[ \\t]{0,2}",
        right_ws in "[ \\t]{0,2}",
    ) {
        let spec = format!("{left_ws}{stem}.json{right_ws}");
        prop_assert!(looks_like_theme_path(&spec));
    }

    #[test]
    fn prop_theme_path_detects_directory_separators(
        left in "[A-Za-z0-9_-]{1,12}",
        right in "[A-Za-z0-9_-]{1,12}",
        use_backslash in any::<bool>(),
    ) {
        let separator = if use_backslash { "\\" } else { "/" };
        let spec = format!("{left}{separator}{right}");
        prop_assert!(looks_like_theme_path(&spec));
    }

    #[test]
    fn prop_plain_theme_names_are_not_treated_as_paths(
        name in "[A-Za-z][A-Za-z0-9_-]{0,31}",
        left_ws in "[ \\t]{0,2}",
        right_ws in "[ \\t]{0,2}",
    ) {
        let spec = format!("{left_ws}{name}{right_ws}");
        prop_assert!(!looks_like_theme_path(&spec));
    }

    #[test]
    fn prop_builtin_theme_serde_and_glamour_mapping(which in 0u8..=2u8) {
        let theme = match which {
            0 => Theme::dark(),
            1 => Theme::light(),
            _ => Theme::solarized(),
        };

        let json = serde_json::to_string(&theme).unwrap();
        let parsed: Theme = serde_json::from_str(&json).unwrap();
        let config = parsed.glamour_style_config();

        prop_assert_eq!(parsed.name, theme.name);
        prop_assert_eq!(parsed.colors.background, theme.colors.background);
        prop_assert_eq!(config.document.style.color.as_deref(), Some(parsed.colors.foreground.as_str()));
        prop_assert_eq!(config.link.color.as_deref(), Some(parsed.colors.accent.as_str()));
        prop_assert_eq!(config.code.style.color.as_deref(), Some(parsed.syntax.string.as_str()));
    }
}

// ============================================================================
// 4. Structured Concurrency (ExtensionRegion)
// ============================================================================

#[test]
fn extension_region_creates_with_default_budget() {
    let harness = TestHarness::new("extension_region_creates_with_default_budget");

    let manager = ExtensionManager::new();
    let region = ExtensionRegion::new(manager);

    harness.log().info_ctx("region", "default budget", |ctx| {
        ctx.push(("debug".to_string(), format!("{region:?}")));
    });

    // Region should have the default 5s budget
    assert!(
        !format!("{region:?}").contains("shutdown_done: true"),
        "newly created region should not be shut down"
    );
}

#[test]
fn extension_region_creates_with_custom_budget() {
    let harness = TestHarness::new("extension_region_creates_with_custom_budget");

    let manager = ExtensionManager::new();
    let budget = Duration::from_millis(500);
    let region = ExtensionRegion::with_budget(manager, budget);

    harness.log().info_ctx("region", "custom budget", |ctx| {
        ctx.push(("budget_ms".to_string(), "500".to_string()));
        ctx.push(("debug".to_string(), format!("{region:?}")));
    });

    // Verify budget is reflected in debug output
    let debug = format!("{region:?}");
    assert!(
        debug.contains("500ms") || debug.contains("0.5s") || debug.contains("500"),
        "custom budget should be visible in debug output: {debug}"
    );
}

#[test]
fn extension_region_manager_access() {
    let harness = TestHarness::new("extension_region_manager_access");

    let manager = ExtensionManager::new();
    let region = ExtensionRegion::new(manager);

    // Should be able to access the inner manager
    let inner = region.manager();
    harness.log().info_ctx("region", "manager access", |ctx| {
        ctx.push(("manager_debug".to_string(), format!("{inner:?}")));
    });

    // Manager should be accessible and functional
    assert!(
        !format!("{inner:?}").is_empty(),
        "manager debug should be non-empty"
    );
}

#[test]
fn extension_region_into_inner_prevents_drop_shutdown() {
    let harness = TestHarness::new("extension_region_into_inner_prevents_drop_shutdown");

    let manager = ExtensionManager::new();
    let region = ExtensionRegion::new(manager);

    // into_inner should mark shutdown_done=true so Drop doesn't warn
    let _manager = region.into_inner();
    harness.log().info_ctx("region", "into_inner", |ctx| {
        ctx.push(("completed".to_string(), "true".to_string()));
    });

    // No panic on drop; the manager is now our responsibility
}

#[test]
fn extension_region_shutdown_idempotent() {
    let harness = TestHarness::new("extension_region_shutdown_idempotent");

    common::run_async(async move {
        let manager = ExtensionManager::new();
        let region = ExtensionRegion::new(manager);

        // First shutdown (no runtime loaded → immediate success)
        let ok1 = region.shutdown().await;
        harness.log().info_ctx("region", "first shutdown", |ctx| {
            ctx.push(("ok".to_string(), ok1.to_string()));
        });
        assert!(ok1, "shutdown of empty manager should succeed");

        // Second shutdown (idempotent)
        let ok2 = region.shutdown().await;
        harness.log().info_ctx("region", "second shutdown", |ctx| {
            ctx.push(("ok".to_string(), ok2.to_string()));
        });
        assert!(ok2, "idempotent shutdown should succeed");
    });
}

#[test]
fn extension_region_drop_without_shutdown_does_not_panic() {
    let harness = TestHarness::new("extension_region_drop_without_shutdown_does_not_panic");

    {
        let manager = ExtensionManager::new();
        let _region = ExtensionRegion::new(manager);
        // Intentionally drop without calling shutdown()
        harness
            .log()
            .info_ctx("region", "drop without shutdown", |ctx| {
                ctx.push(("action".to_string(), "dropping".to_string()));
            });
    }

    harness.log().info_ctx("region", "after drop", |ctx| {
        ctx.push(("survived".to_string(), "true".to_string()));
    });
    // If we reach here, drop didn't panic
}

#[test]
fn extension_manager_shutdown_without_runtime() {
    let harness = TestHarness::new("extension_manager_shutdown_without_runtime");

    common::run_async(async move {
        let manager = ExtensionManager::new();
        let budget = Duration::from_millis(100);

        let ok = manager.shutdown(budget).await;
        harness
            .log()
            .info_ctx("manager", "shutdown no runtime", |ctx| {
                ctx.push(("ok".to_string(), ok.to_string()));
            });

        assert!(ok, "shutdown without runtime should succeed immediately");
    });
}

#[test]
fn extension_region_budget_propagates() {
    let harness = TestHarness::new("extension_region_budget_propagates");

    common::run_async(async move {
        let manager = ExtensionManager::new();
        let budget = Duration::from_millis(200);
        let region = ExtensionRegion::with_budget(manager, budget);

        // Shutdown should use the configured budget
        let ok = region.shutdown().await;
        harness
            .log()
            .info_ctx("region", "budget propagation", |ctx| {
                ctx.push(("budget_ms".to_string(), "200".to_string()));
                ctx.push(("ok".to_string(), ok.to_string()));
            });

        assert!(ok, "shutdown with custom budget should succeed");
    });
}

// ============================================================================
// 5. PiConsole TUI Components
// ============================================================================

#[test]
fn console_render_panel() {
    let harness = TestHarness::new("console_render_panel");
    let console = test_console();

    harness.log().info_ctx("console", "render panel", |ctx| {
        ctx.push(("content".to_string(), "Test content".to_string()));
        ctx.push(("title".to_string(), "Test Panel".to_string()));
    });

    console.render_panel("Test content", "Test Panel");
}

#[test]
fn console_render_table() {
    let harness = TestHarness::new("console_render_table");
    let console = test_console();

    let headers = &["Name", "Value", "Status"];
    let rows = &[vec!["foo", "42", "ok"], vec!["bar", "100", "error"]];

    harness.log().info_ctx("console", "render table", |ctx| {
        ctx.push(("headers".to_string(), format!("{headers:?}")));
        ctx.push(("rows".to_string(), rows.len().to_string()));
    });

    console.render_table(headers, rows);
}

#[test]
fn console_render_rule() {
    let harness = TestHarness::new("console_render_rule");
    let console = test_console();

    harness.log().info_ctx("console", "render rule", |ctx| {
        ctx.push(("with_title".to_string(), "true".to_string()));
    });

    console.render_rule(Some("Section Break"));
    console.render_rule(None);
}

#[test]
fn console_render_status_messages() {
    let harness = TestHarness::new("console_render_status_messages");
    let console = test_console();

    harness
        .log()
        .info_ctx("console", "render status messages", |ctx| {
            ctx.push((
                "types".to_string(),
                "error,warning,success,info".to_string(),
            ));
        });

    console.render_error("Something went wrong");
    console.render_warning("Be careful");
    console.render_success("All good");
    console.render_info("FYI");
}

#[test]
fn console_render_usage_info() {
    let harness = TestHarness::new("console_render_usage_info");
    let console = test_console();

    harness.log().info_ctx("console", "render usage", |ctx| {
        ctx.push(("input_tokens".to_string(), "1000".to_string()));
        ctx.push(("output_tokens".to_string(), "500".to_string()));
    });

    console.render_usage(1000, 500, Some(0.015));
    console.render_usage(0, 0, None);
}

#[test]
fn console_render_model_info() {
    let harness = TestHarness::new("console_render_model_info");
    let console = test_console();

    harness
        .log()
        .info_ctx("console", "render model info", |ctx| {
            ctx.push(("model".to_string(), "claude-sonnet-4-5".to_string()));
        });

    console.render_model_info("claude-sonnet-4-5", Some("medium"));
    console.render_model_info("gpt-4o", None);
}

// ============================================================================
// 6. Console with Theme
// ============================================================================

#[test]
fn console_with_theme_does_not_panic() {
    let harness = TestHarness::new("console_with_theme_does_not_panic");

    for (name, theme) in [
        ("dark", Theme::dark()),
        ("light", Theme::light()),
        ("solarized", Theme::solarized()),
    ] {
        harness
            .log()
            .info_ctx("console", "create with theme", |ctx| {
                ctx.push(("theme".to_string(), name.to_string()));
            });

        let console = PiConsole::new_with_theme(Some(theme));
        console.render_markdown("# Test\n\n**bold** text\n");
        console.render_panel("content", "title");
    }
}
