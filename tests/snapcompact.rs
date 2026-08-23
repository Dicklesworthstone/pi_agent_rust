//! Snapcompact compaction-mode unit tests (bd-cv653.7.6).
//!
//! Pure logic: rasterizer determinism, PNG structure, details round-trip,
//! frame attachment/stripping, budget accounting, and mode parsing.
//! No mocks, fixtures, network, or providers (suite: unit).

#![allow(clippy::similar_names)]

use pi::compaction::CompactionRenderMode;
use pi::compaction_snap::{
    COMPACTION_SUMMARY_PREFIX, COMPACTION_SUMMARY_SUFFIX, SnapFrame, SnapPayload,
    attach_frames, frames_from_details, png_encode, render_frames, strip_snapcompact_images,
};
use pi::model::{
    ContentBlock, ImageContent, Message, TextContent, UserContent, UserMessage,
};

const FRAME_WIDTH: u32 = 960;

fn sample_transcript() -> String {
    let mut lines = vec![
        "[User]: Fix the login bug in src/auth.rs".to_string(),
        "[Assistant]: I'll start by reading the auth module.".to_string(),
        "[Tool Call]: read(src/auth.rs)".to_string(),
        "[Tool Result]: fn login(user: &str) -> Result<Token, AuthError>".to_string(),
        "[Assistant]: Found the issue: expired tokens are cached.".to_string(),
        "[User]: Please add a regression test.".to_string(),
    ];
    // Pad past one frame so multi-frame splitting is exercised.
    for i in 0..200 {
        lines.push(format!("[Assistant]: progress line {i} with some detail text"));
    }
    lines.join("\n")
}

fn summary_body_text() -> String {
    format!("{COMPACTION_SUMMARY_PREFIX}body{COMPACTION_SUMMARY_SUFFIX}")
}

#[test]
fn rendering_is_deterministic_byte_for_byte() {
    let t = sample_transcript();
    let a = render_frames(&t);
    let b = render_frames(&t);
    assert_eq!(a.len(), b.len());
    assert!(!a.is_empty());
    for (fa, fb) in a.iter().zip(b.iter()) {
        assert_eq!(
            fa.png, fb.png,
            "identical transcripts must yield identical PNG bytes"
        );
        assert_eq!(fa.width, FRAME_WIDTH);
        assert_eq!(fa.width, fb.width);
        assert_eq!(fa.height, fb.height);
    }
}

// Golden hash pins the exact renderer output (font table + palette +
// geometry + flate2 output). Any intentional visual change requires
// updating this constant in the same commit, with reviewer-visible diff.
const GOLDEN_SHA256: &str = "REPLACE_WITH_GENERATED_SHA";

#[test]
fn golden_hash_matches_committed_renderer_output() {
    let small = render_frames("[User]: golden\n");
    assert_eq!(small.len(), 1);
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(small[0].png.as_bytes());
    let digest: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        digest, GOLDEN_SHA256,
        "renderer output drifted from committed golden"
    );
}

#[test]
fn long_transcripts_split_into_bounded_frames() {
    let frames = render_frames(&sample_transcript());
    assert!(frames.len() >= 2, "200+ lines must span multiple frames");
    for frame in &frames {
        assert!(frame.height <= 1280);
    }
}

#[test]
fn non_ascii_and_tabs_render_deterministically_as_placeholders() {
    let a = render_frames("[User]: héllo\tworld 日本");
    let b = render_frames("[User]: h?llo?world ???");
    assert_eq!(a, b, "non-printables normalize to '?' consistently");
}

/// Reference bitwise IEEE CRC-32 mirroring the implementation contract.
mod reference_crc {
    pub fn crc32_ieee(data: &[u8]) -> u32 {
        let mut crc: u32 = 0xFFFF_FFFF;
        for &b in data {
            crc ^= u32::from(b);
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
        !crc
    }
}

#[test]
fn png_output_is_structurally_valid() {
    let rgb = vec![128u8; 8 * 6 * 3];
    let png = png_encode(8, 6, &rgb);
    assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);

    let mut off = 8;
    let mut saw_idat = false;
    while off < png.len() {
        let len =
            u32::from_be_bytes([png[off], png[off + 1], png[off + 2], png[off + 3]]) as usize;
        let kind = &png[off + 4..off + 8];
        let data_start = off + 8;
        let crc_at = data_start + len;
        let expected_crc = u32::from_be_bytes([
            png[crc_at],
            png[crc_at + 1],
            png[crc_at + 2],
            png[crc_at + 3],
        ]);
        assert_eq!(
            reference_crc::crc32_ieee(&png[off + 4..crc_at]),
            expected_crc,
            "chunk {kind:?} CRC mismatch"
        );
        match kind {
            b"IHDR" => assert_eq!(len, 13),
            b"IDAT" => saw_idat = true,
            b"IEND" => assert_eq!(len, 0),
            _ => {}
        }
        off = crc_at + 4;
    }
    assert!(saw_idat, "missing IDAT chunk");
}

#[test]
fn details_round_trip_preserves_payload_and_other_keys() {
    let frames = render_frames("[User]: hello\n");
    let payload = SnapPayload::new(frames);
    let existing = serde_json::json!({
        "readFiles": ["src/auth.rs"],
        "modifiedFiles": [],
    });
    let merged = pi::compaction_snap::payload_to_details(Some(existing), &payload);

    assert_eq!(
        merged.get("readFiles").and_then(|v| v.as_str()),
        Some("src/auth.rs"),
        "unrelated detail keys survive merge"
    );
    let extracted =
        frames_from_details(Some(&merged)).expect("schema-valid payload extracts");
    assert_eq!(
        extracted.schema,
        pi::compaction_snap::SNAPCOMPACT_DETAILS_SCHEMA
    );
    assert_eq!(extracted.frames.len(), payload.frames.len());

    // Corrupt schema fails closed.
    let mut bad = merged.clone();
    bad["snapcompact"]["schema"] = serde_json::Value::String("alien.v99".into());
    assert!(frames_from_details(Some(&bad)).is_none());
    // Missing key fails closed.
    assert!(frames_from_details(Some(&serde_json::json!({}))).is_none());
}

fn base_summary_message() -> Message {
    Message::User(UserMessage {
        content: UserContent::Text(summary_body_text()),
        timestamp: 7,
    })
}

fn frame_stub(data: &str) -> SnapFrame {
    SnapFrame {
        png: data.to_string(),
        width: 10,
        height: 10,
    }
}

#[test]
fn attach_frames_places_images_after_text_block() {
    let payload = SnapPayload::new(vec![frame_stub("QUFB"), frame_stub("QkJC")]);
    let attached = attach_frames(base_summary_message(), Some(&payload));

    let Message::User(user) = attached else {
        panic!("user message expected");
    };
    let UserContent::Blocks(blocks) = &user.content else {
        panic!("blocks expected after attachment");
    };
    assert_eq!(blocks.len(), 3);
    assert!(matches!(&blocks[0], ContentBlock::Text(t) if t.text.contains("<summary>")));
    assert!(matches!(&blocks[1], ContentBlock::Image(ImageContent { data, mime_type })
        if data == "QUFB" && mime_type == "image/png"));
    assert_eq!(user.timestamp, 7, "timestamp preserved from source entry");

    // No payload → untouched text message (structural; Message lacks PartialEq).
    match attach_frames(base_summary_message(), None) {
        Message::User(u) => match u.content {
            UserContent::Text(t) => assert_eq!(t, summary_body_text()),
            other => panic!("expected plain text content, got {other:?}"),
        },
        _ => panic!("expected user message"),
    }
}

#[test]
fn strip_removes_only_compaction_summary_images() {
    let summary = Message::User(UserMessage {
        content: UserContent::Blocks(vec![
            ContentBlock::Text(TextContent::new(summary_body_text())),
            ContentBlock::Image(ImageContent { data: "QQ==".into(), mime_type: "image/png".into() }),
            ContentBlock::Image(ImageContent { data: "Qg==".into(), mime_type: "image/png".into() }),
        ]),
        timestamp: 1,
    });
    let user_photo = Message::User(UserMessage {
        content: UserContent::Blocks(vec![ContentBlock::Image(ImageContent {
            data: "Qw==".into(),
            mime_type: "image/png".into(),
        })]),
        timestamp: 2,
    });
    let assistant = Message::User(UserMessage {
        content: UserContent::Text("plain".to_string()),
        timestamp: 3,
    });
    let mut msgs = vec![summary, user_photo, assistant];

    let stats = strip_snapcompact_images(&mut msgs, false);
    assert_eq!(stats.removed_frames, 2);
    assert_eq!(stats.affected_messages, 1);

    let UserContent::Blocks(left) = user_blocks(&msgs[0]) else {
        panic!("blocks expected");
    };
    assert_eq!(left.len(), 1, "frames stripped, text kept");
    let UserContent::Blocks(photo) = user_blocks(&msgs[1]) else {
        panic!("blocks expected");
    };
    assert_eq!(photo.len(), 1, "user-pasted images are never touched");

    assert_eq!(
        strip_snapcompact_images(&mut msgs, true),
        pi::compaction_snap::StripStats::default()
    );
}

fn user_blocks(m: &Message) -> &UserContent {
    match m {
        Message::User(u) => &u.content,
        _ => panic!("user message expected"),
    }
}

#[test]
fn budget_accounting_counts_attached_frame_tokens() {
    use pi::compaction::estimate_entries_context_tokens;
    use pi::session::{EntryBase, MessageEntry, SessionEntry, SessionMessage};

    fn base(id: &str) -> EntryBase {
        EntryBase {
            id: Some(id.to_string()),
            parent_id: None,
            timestamp: "0".to_string(),
        }
    }

    let text_only = SessionEntry::Message(MessageEntry {
        base: base("m1"),
        message: SessionMessage::User {
            content: UserContent::Text("hi".to_string()),
            timestamp: Some(0),
        },
    });
    let with_image = SessionEntry::Message(MessageEntry {
        base: base("m2"),
        message: SessionMessage::User {
            content: UserContent::Blocks(vec![
                ContentBlock::Text(TextContent::new("hi")),
                ContentBlock::Image(ImageContent {
                    data: "QQ==".to_string(),
                    mime_type: "image/png".to_string(),
                }),
            ]),
            timestamp: Some(0),
        },
    });

    let flat_image_chars = 1200_usize * 3; // IMAGE_TOKEN_ESTIMATE × CHARS_PER_TOKEN_ESTIMATE
    let delta = estimate_entries_context_tokens(&[&with_image])
        - estimate_entries_context_tokens(&[&text_only]);
    assert_eq!(
        delta, flat_image_chars as u64,
        "each attached snapcompact frame must be billed at the documented flat image estimate"
    );
}

#[test]
fn render_mode_parses_config_values() {
    let mode: CompactionRenderMode = "snapcompact".parse().expect("parse snapcompact");
    assert_eq!(mode, CompactionRenderMode::SnapCompact);
    let mode: CompactionRenderMode = "text".parse().expect("parse text");
    assert_eq!(mode, CompactionRenderMode::Text);
    assert!("bogus".parse::<CompactionRenderMode>().is_err());
    assert_eq!(CompactionRenderMode::default(), CompactionRenderMode::Text);
}
