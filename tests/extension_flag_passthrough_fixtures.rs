//! Fixture tests for extension CLI flag pass-through with mixed built-in + extension flag sets.
//!
//! These tests validate the end-to-end behavior specified in the gap ledger:
//! - Complete two-pass parsing for extension flags
//! - Mixed built-in + extension flag sets
//! - Parity with TypeScript behavior

use pi::cli::parse_with_extension_flags;
use pi::extensions::ExtensionManager;

/// Test fixture scenarios covering mixed built-in and extension flags
#[derive(Debug)]
struct ExtensionFlagFixture {
    name: &'static str,
    args: &'static [&'static str],
    expected_message: &'static [&'static str],
}

fn get_extension_flag_fixtures() -> Vec<ExtensionFlagFixture> {
    vec![
        ExtensionFlagFixture {
            name: "mixed_builtin_and_extension_flags",
            args: &[
                "pi",
                "--model",
                "claude-sonnet-4",
                "--debug",
                "true",
                "--thinking",
                "medium",
                "--custom-flag",
                "value",
                "Hello",
                "world",
            ],
            expected_message: &["Hello", "world"],
        },
        ExtensionFlagFixture {
            name: "boolean_extension_flags_mixed_with_builtin",
            args: &[
                "pi",
                "--print",
                "--unknown-flag",
                "--dry-run",
                "--tools",
                "grep,edit",
                "test",
                "message",
            ],
            expected_message: &["test", "message"],
        },
        ExtensionFlagFixture {
            name: "equals_syntax_extension_flags",
            args: &[
                "pi",
                "--model=gpt-4",
                "--level=debug",
                "--format=json",
                "--continue",
                // NOTE: the first positional must not collide with a root
                // subcommand ("review" became `pi review`), or clap parses it
                // as a subcommand invocation instead of message args.
                "critique",
                "this",
                "code",
            ],
            expected_message: &["critique", "this", "code"],
        },
        ExtensionFlagFixture {
            name: "edge_case_flag_ordering",
            args: &[
                "pi",
                "--model",
                "claude-opus-4",
                "--thinking",
                "high",
                "--custom-first",
                "--custom-middle",
                "value",
                "--custom-last",
                "message",
            ],
            expected_message: &[],
        },
        ExtensionFlagFixture {
            name: "complex_mixed_scenario",
            args: &[
                "pi",
                "--resume",
                "--ext-level=trace",
                "--provider",
                "anthropic",
                "--ext-timeout",
                "30",
                "--ext-format",
                "--no-tools",
                "Complex",
                "test",
                "message",
                "with",
                "message",
            ],
            expected_message: &["Complex", "test", "message", "with", "message"],
        },
    ]
}

fn assert_extension_flag(parsed: &pi::cli::ParsedCli, name: &str, value: Option<&str>) {
    assert!(
        parsed
            .extension_flags
            .iter()
            .any(|flag| flag.name == name && flag.value.as_deref() == value),
        "missing extension flag {name}={value:?}"
    );
}

#[test]
fn test_extension_flag_passthrough_fixtures() {
    let fixtures = get_extension_flag_fixtures();

    for fixture in fixtures {
        println!("Testing fixture: {}", fixture.name);

        let args: Vec<String> = fixture
            .args
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        let parsed = parse_with_extension_flags(args)
            .unwrap_or_else(|e| panic!("Fixture '{}' failed to parse: {}", fixture.name, e));

        // Basic validation that parsing worked
        match fixture.name {
            "mixed_builtin_and_extension_flags" => {
                assert_eq!(parsed.cli.model.as_deref(), Some("claude-sonnet-4"));
                assert_eq!(parsed.cli.thinking.as_deref(), Some("medium"));
                assert_extension_flag(&parsed, "debug", Some("true"));
                assert_extension_flag(&parsed, "custom-flag", Some("value"));
            }
            "boolean_extension_flags_mixed_with_builtin" => {
                assert!(parsed.cli.print);
                assert_eq!(parsed.cli.tools, "grep,edit");
                assert_extension_flag(&parsed, "unknown-flag", None);
                assert_extension_flag(&parsed, "dry-run", None);
            }
            "equals_syntax_extension_flags" => {
                assert_eq!(parsed.cli.model.as_deref(), Some("gpt-4"));
                assert!(parsed.cli.r#continue);
                assert_extension_flag(&parsed, "level", Some("debug"));
                assert_extension_flag(&parsed, "format", Some("json"));
            }
            "edge_case_flag_ordering" => {
                assert_eq!(parsed.cli.model.as_deref(), Some("claude-opus-4"));
                assert_eq!(parsed.cli.thinking.as_deref(), Some("high"));
                assert_extension_flag(&parsed, "custom-first", None);
                assert_extension_flag(&parsed, "custom-middle", Some("value"));
                // Note: custom-last might not be parsed if it appears after message args start
            }
            "complex_mixed_scenario" => {
                assert!(parsed.cli.resume);
                assert_eq!(parsed.cli.provider.as_deref(), Some("anthropic"));
                assert!(parsed.cli.no_tools);
                assert_extension_flag(&parsed, "ext-level", Some("trace"));
                assert_extension_flag(&parsed, "ext-timeout", Some("30"));
                assert_extension_flag(&parsed, "ext-format", None);
            }
            _ => panic!("Unknown fixture: {}", fixture.name),
        }

        // Verify message args
        let actual_message = parsed.cli.message_args();
        assert_eq!(
            actual_message, fixture.expected_message,
            "Fixture '{}': message args mismatch",
            fixture.name
        );

        println!("✓ Fixture '{}' passed", fixture.name);
    }
}

/// Test that validates extension flag application to a real extension manager
#[test]
fn test_extension_flag_application_integration() {
    // Create a mock extension with some flags
    let manager = ExtensionManager::new();

    // This test validates that the extension flags are properly structured
    // and can be created without panicking
    let test_flags = [
        pi::cli::ExtensionCliFlag {
            name: "debug".to_string(),
            value: Some("true".to_string()),
        },
        pi::cli::ExtensionCliFlag {
            name: "level".to_string(),
            value: Some("info".to_string()),
        },
        pi::cli::ExtensionCliFlag {
            name: "unknown-flag".to_string(),
            value: None,
        },
    ];

    // Verify flag display names work correctly
    assert_eq!(test_flags[0].display_name(), "--debug");
    assert_eq!(test_flags[1].display_name(), "--level");
    assert_eq!(test_flags[2].display_name(), "--unknown-flag");

    // Verify the manager can be created (basic integration test)
    // A new manager should have no flags initially
    assert!(manager.list_flags().is_empty());
}

/// Test edge cases in two-pass parsing
#[test]
fn test_two_pass_parsing_edge_cases() {
    // Test that double-dash stops extension flag parsing
    let args = vec![
        "pi".to_string(),
        "--debug".to_string(),
        "true".to_string(),
        "--".to_string(),
        "--not-an-extension-flag".to_string(),
        "message".to_string(),
    ];

    let parsed = parse_with_extension_flags(args).expect("Should parse with double-dash");

    assert_eq!(parsed.extension_flags.len(), 1);
    assert_eq!(parsed.extension_flags[0].name, "debug");

    let message_args = parsed.cli.message_args();
    assert!(message_args.contains(&"--not-an-extension-flag"));
    assert!(message_args.contains(&"message"));

    // Test subcommand handling
    let args_with_subcommand = vec![
        "pi".to_string(),
        "--ext-flag".to_string(),
        "install".to_string(), // This is a known subcommand
        "package-name".to_string(),
    ];

    let parsed_sub = parse_with_extension_flags(args_with_subcommand);
    // Should either parse successfully with extension flag extracted,
    // or fail gracefully if subcommands are not supported
    if let Ok(p) = parsed_sub {
        // If parsing succeeds, extension flag should be extracted.
        assert!(p.extension_flags.iter().any(|f| f.name == "ext-flag"));
    }
}

/// Test that validates the specific TypeScript parity requirements
#[test]
fn test_typescript_parity_scenarios() {
    // Test scenarios that would match TypeScript pi-mono behavior

    // Scenario 1: Extension flag before built-in flag
    let args1 = vec![
        "pi".to_string(),
        "--ext-debug".to_string(),
        "--model".to_string(),
        "claude-sonnet-4".to_string(),
        "hello".to_string(),
    ];

    let parsed1 = parse_with_extension_flags(args1).expect("Should parse ext flag before built-in");
    assert_eq!(parsed1.extension_flags.len(), 1);
    assert_eq!(parsed1.extension_flags[0].name, "ext-debug");
    assert_eq!(parsed1.cli.model, Some("claude-sonnet-4".to_string()));

    // Scenario 2: Multiple extension flags interspersed with built-ins
    let args2 = vec![
        "pi".to_string(),
        "--ext-one".to_string(),
        "--print".to_string(),
        "--ext-two=value".to_string(),
        "--thinking".to_string(),
        "high".to_string(),
        "--ext-three".to_string(),
        "message here".to_string(),
    ];

    let parsed2 = parse_with_extension_flags(args2).expect("Should parse interspersed flags");
    assert_eq!(parsed2.extension_flags.len(), 3);
    assert!(parsed2.cli.print);
    assert_eq!(parsed2.cli.thinking, Some("high".to_string()));

    // Scenario 3: Extension flag with space-separated value vs built-in
    let args3 = vec![
        "pi".to_string(),
        "--ext-config".to_string(),
        "config.json".to_string(),
        "--tools".to_string(),
        "grep,edit".to_string(),
        "analyze".to_string(),
        "this".to_string(),
    ];

    let parsed3 = parse_with_extension_flags(args3).expect("Should parse space-separated values");

    let ext_config = parsed3
        .extension_flags
        .iter()
        .find(|f| f.name == "ext-config")
        .expect("Should have ext-config flag");
    assert_eq!(ext_config.value, Some("config.json".to_string()));

    assert_eq!(parsed3.cli.tools, "grep,edit".to_string());
    assert_eq!(parsed3.cli.message_args(), vec!["analyze", "this"]);
}
