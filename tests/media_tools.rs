#![forbid(unsafe_code)]

mod common;

use common::TestHarness;
use common::logging::validate_jsonl_v2_only;
use pi::config::Config;
use pi::media_tools::{GenerateImageTool, InspectImageTool, MediaSettings, TtsTool};
use pi::model::ContentBlock;
use pi::tools::{Tool, ToolRegistry};
use serde_json::json;
use std::fs;

fn finish_case(harness: &TestHarness, case: &str) {
    harness
        .log()
        .info("verify", format!("case {case} assertions passed"));
    let path = harness.temp_path(format!("{case}.jsonl"));
    harness
        .write_jsonl_logs(&path)
        .expect("write JSONL test logs");
    let payload = std::fs::read_to_string(&path).expect("read JSONL test logs");
    let errors = validate_jsonl_v2_only(&payload);
    assert!(
        errors.is_empty(),
        "JSONL schema violations in {case}.jsonl: {errors:?}"
    );
    harness.record_artifact(format!("{case}.jsonl"), &path);
}

#[test]
fn test_inspect_image_schema_and_metadata() {
    let harness = TestHarness::new("inspect_image_schema");
    let tool = InspectImageTool::new(harness.temp_dir());

    assert_eq!(tool.name(), "inspect_image");
    assert_eq!(tool.label(), "Inspect Image");
    assert!(!tool.description().is_empty());

    let params = tool.parameters();
    assert_eq!(params["type"], "object");
    assert!(params["properties"]["path"].is_object());
    assert!(params["properties"]["prompt"].is_object());

    finish_case(&harness, "inspect_image_schema");
}

#[test]
fn test_inspect_image_fixture_analysis() {
    let harness = TestHarness::new("inspect_image_fixture");
    let img_path = harness.temp_path("fixture.png");

    // Minimal valid 1x1 PNG
    const PNG_BYTES: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];
    fs::write(&img_path, PNG_BYTES).expect("write png fixture");

    asupersync::test_utils::run_test(|| async {
        let tool = InspectImageTool::new(harness.temp_dir()).with_mock(true);
        let output = tool
            .execute(
                "call_1",
                json!({
                    "path": "fixture.png",
                    "prompt": "Explain this architecture diagram"
                }),
                None,
            )
            .await
            .expect("inspect_image execute");

        let first_block = match output.content.first() {
            Some(ContentBlock::Text(t)) => &t.text,
            _ => panic!("expected text content block"),
        };
        assert!(first_block.contains("Image Analysis"));
        assert!(first_block.contains("image/png"));

        let details = output.details.as_ref().expect("expected details metadata");
        assert_eq!(details["mime_type"], "image/png");
        assert_eq!(details["size_bytes"], PNG_BYTES.len());
    });

    finish_case(&harness, "inspect_image_fixture");
}

#[test]
fn test_inspect_image_missing_file_error() {
    let harness = TestHarness::new("inspect_image_missing_file");

    asupersync::test_utils::run_test(|| async {
        let tool = InspectImageTool::new(harness.temp_dir()).with_mock(true);
        let res = tool
            .execute("call_2", json!({ "path": "nonexistent.png" }), None)
            .await;

        match res {
            Err(e) => assert!(e.to_string().contains("image file not found")),
            Ok(_) => panic!("expected error for nonexistent file"),
        }
    });

    finish_case(&harness, "inspect_image_missing_file");
}

#[test]
fn test_inspect_image_unsupported_format_error() {
    let harness = TestHarness::new("inspect_image_unsupported");
    let text_path = harness.temp_path("bad.txt");
    fs::write(&text_path, b"hello").expect("write bad file");

    asupersync::test_utils::run_test(|| async {
        let tool = InspectImageTool::new(harness.temp_dir()).with_mock(true);
        let res = tool
            .execute("call_3", json!({ "path": "bad.txt" }), None)
            .await;

        match res {
            Err(e) => assert!(e.to_string().contains("unsupported image extension")),
            Ok(_) => panic!("expected error for unsupported extension"),
        }
    });

    finish_case(&harness, "inspect_image_unsupported");
}

#[test]
fn test_inspect_image_auth_error_when_unconfigured() {
    let harness = TestHarness::new("inspect_image_auth_error");
    let img_path = harness.temp_path("fixture.png");
    const PNG_BYTES: &[u8] = &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    fs::write(&img_path, PNG_BYTES).expect("write png");

    asupersync::test_utils::run_test(|| async {
        // mock disabled (mock_mode = false) and explicit empty api key -> named auth error
        let tool =
            InspectImageTool::with_defaults(harness.temp_dir(), Some("gemini".to_string()), None)
                .with_mock(false)
                .with_api_key(Some(String::new()));

        let res = tool
            .execute("call_auth", json!({ "path": "fixture.png" }), None)
            .await;

        match res {
            Err(e) => assert!(
                e.to_string()
                    .contains("missing API key for vision provider")
            ),
            Ok(_) => panic!("expected auth error when unconfigured"),
        }
    });

    finish_case(&harness, "inspect_image_auth_error");
}

#[test]
fn test_generate_image_schema_and_gating() {
    let harness = TestHarness::new("generate_image_schema");
    let tool = GenerateImageTool::new(harness.temp_dir());

    assert_eq!(tool.name(), "generate_image");
    assert_eq!(tool.label(), "Generate Image");

    let params = tool.parameters();
    assert!(params["properties"]["prompt"].is_object());
    assert!(params["properties"]["provider"].is_object());
    assert!(params["properties"]["size"].is_object());

    finish_case(&harness, "generate_image_schema");
}

#[test]
fn test_generate_image_writes_valid_png_artifact() {
    let harness = TestHarness::new("generate_image_artifact");

    asupersync::test_utils::run_test(|| async {
        let tool = GenerateImageTool::new(harness.temp_dir()).with_mock(true);
        let output_target = "out/test_image.png";
        let output = tool
            .execute(
                "call_4",
                json!({
                    "prompt": "A futuristic coding terminal with neon accents",
                    "size": "1024x1024",
                    "output_path": output_target
                }),
                None,
            )
            .await
            .expect("generate_image execute");

        let full_path = harness.temp_path(output_target);
        assert!(full_path.is_file(), "output image file must be written");

        let bytes = fs::read(&full_path).expect("read generated png");
        assert!(
            bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47]),
            "must have valid PNG signature"
        );

        let details = output.details.as_ref().expect("expected details metadata");
        assert_eq!(details["size"], "1024x1024");
        assert_eq!(details["provider"], "openai");
    });

    finish_case(&harness, "generate_image_artifact");
}

#[test]
fn test_generate_image_missing_prompt_error() {
    let harness = TestHarness::new("generate_image_missing_prompt");

    asupersync::test_utils::run_test(|| async {
        let tool = GenerateImageTool::new(harness.temp_dir()).with_mock(true);
        let res = tool.execute("call_5", json!({}), None).await;
        match res {
            Err(e) => assert!(
                e.to_string().contains("missing required") && e.to_string().contains("prompt")
            ),
            Ok(_) => panic!("expected error for missing prompt"),
        }
    });

    finish_case(&harness, "generate_image_missing_prompt");
}

#[test]
fn test_generate_image_auth_error_when_unconfigured() {
    let harness = TestHarness::new("generate_image_auth_error");

    asupersync::test_utils::run_test(|| async {
        let tool = GenerateImageTool::with_provider(harness.temp_dir(), Some("openai".to_string()))
            .with_mock(false)
            .with_api_key(Some(String::new()));

        let res = tool
            .execute(
                "call_auth_gen",
                json!({ "prompt": "generate a galaxy" }),
                None,
            )
            .await;

        match res {
            Err(e) => assert!(
                e.to_string()
                    .contains("missing API key for image generation provider")
            ),
            Ok(_) => panic!("expected auth error when unconfigured"),
        }
    });

    finish_case(&harness, "generate_image_auth_error");
}

#[test]
fn test_tts_schema_and_gating() {
    let harness = TestHarness::new("tts_schema");
    let tool = TtsTool::new(harness.temp_dir());

    assert_eq!(tool.name(), "tts");
    assert_eq!(tool.label(), "Text to Speech");

    let params = tool.parameters();
    assert!(params["properties"]["text"].is_object());
    assert!(params["properties"]["voice"].is_object());
    assert!(params["properties"]["format"].is_object());

    finish_case(&harness, "tts_schema");
}

#[test]
fn test_tts_writes_valid_wav_artifact() {
    let harness = TestHarness::new("tts_artifact");

    asupersync::test_utils::run_test(|| async {
        let tool = TtsTool::new(harness.temp_dir()).with_mock(true);
        let output_target = "audio/greeting.wav";
        let output = tool
            .execute(
                "call_6",
                json!({
                    "text": "System operational. All test gates passing.",
                    "voice": "eve",
                    "format": "wav",
                    "output_path": output_target
                }),
                None,
            )
            .await
            .expect("tts execute");

        let full_path = harness.temp_path(output_target);
        assert!(full_path.is_file(), "output audio file must be written");

        let bytes = fs::read(&full_path).expect("read generated audio");
        assert!(
            bytes.starts_with(b"RIFF"),
            "must have valid WAV RIFF header"
        );
        if bytes.len() >= 12 {
            assert_eq!(&bytes[8..12], b"WAVE");
        }

        let details = output.details.as_ref().expect("expected details metadata");
        assert_eq!(details["voice"], "eve");
        assert_eq!(details["format"], "wav");
        assert_eq!(details["char_count"], 43);
    });

    finish_case(&harness, "tts_artifact");
}

#[test]
fn test_tts_empty_text_error() {
    let harness = TestHarness::new("tts_empty_text");

    asupersync::test_utils::run_test(|| async {
        let tool = TtsTool::new(harness.temp_dir()).with_mock(true);
        let res = tool.execute("call_7", json!({ "text": "   " }), None).await;

        match res {
            Err(e) => assert!(e.to_string().contains("text cannot be empty")),
            Ok(_) => panic!("expected error for empty text"),
        }
    });

    finish_case(&harness, "tts_empty_text");
}

#[test]
fn test_tts_auth_error_when_unconfigured() {
    let harness = TestHarness::new("tts_auth_error");

    asupersync::test_utils::run_test(|| async {
        let tool = TtsTool::new(harness.temp_dir())
            .with_mock(false)
            .with_api_key(Some(String::new()));

        let res = tool
            .execute("call_auth_tts", json!({ "text": "hello world" }), None)
            .await;

        match res {
            Err(e) => assert!(e.to_string().contains("missing API key for TTS synthesis")),
            Ok(_) => panic!("expected auth error when unconfigured"),
        }
    });

    finish_case(&harness, "tts_auth_error");
}

#[test]
fn test_media_tools_default_gated_off() {
    let harness = TestHarness::new("media_tools_default_gated");
    let default_registry = ToolRegistry::new(&["read", "grep", "find"], harness.temp_dir(), None);

    assert!(default_registry.get("inspect_image").is_none());
    assert!(default_registry.get("generate_image").is_none());
    assert!(default_registry.get("tts").is_none());

    finish_case(&harness, "media_tools_default_gated");
}

#[test]
fn test_media_tools_opt_in_activation() {
    let harness = TestHarness::new("media_tools_opt_in");
    let mut config = Config::default();
    config.media = Some(MediaSettings {
        enable_inspect_image: Some(true),
        enable_generate_image: Some(true),
        enable_tts: Some(true),
        vision_model: Some("gemini-1.5-pro".to_string()),
        vision_provider: Some("gemini".to_string()),
        image_gen_provider: Some("openai".to_string()),
        image_gen_model: Some("dall-e-3".to_string()),
        tts_voice: Some("eve".to_string()),
        tts_provider: Some("xai".to_string()),
    });

    let enabled_registry = ToolRegistry::new(
        &["read", "inspect_image", "generate_image", "tts"],
        harness.temp_dir(),
        Some(&config),
    );

    assert!(enabled_registry.get("inspect_image").is_some());
    assert!(enabled_registry.get("generate_image").is_some());
    assert!(enabled_registry.get("tts").is_some());

    finish_case(&harness, "media_tools_opt_in");
}
