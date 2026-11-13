#![forbid(unsafe_code)]

mod common;

use common::TestHarness;
use common::logging::validate_jsonl_v2_only;
use pi::web_remote::{
    BindMode, ControlMode, EMBEDDED_WEB_CLIENT_HTML, EMBEDDED_WEB_CLIENT_JS, TokenKind,
    WebRemoteBridge, WebRemoteManager, WebRemoteServer, WebRemoteSettings, render_half_block_qr,
};

fn finish_case(harness: &TestHarness, case: &str) {
    harness
        .log()
        .info("verify", format!("case '{case}' assertions passed"));
    let path = harness.temp_path(format!("{case}.jsonl"));
    assert!(harness.write_jsonl_logs(&path).is_ok(), "write JSONL logs");
    let payload = std::fs::read_to_string(&path).unwrap_or_default();
    let errors = validate_jsonl_v2_only(&payload);
    assert!(
        errors.is_empty(),
        "JSONL schema violations in {case}.jsonl: {errors:?}"
    );
    harness.record_artifact(format!("{case}.jsonl"), &path);
}

#[test]
fn test_threat_model_document_exists_and_valid() {
    let harness = TestHarness::new("threat_model_document");
    let doc_path = "docs/security/web-access-threat-model.md";
    assert!(
        std::path::Path::new(doc_path).is_file(),
        "threat model document must exist"
    );

    let content = std::fs::read_to_string(doc_path).unwrap_or_default();
    assert!(content.contains("pi.web.threat_model.v1"));
    assert!(content.contains("T-01"));
    assert!(content.contains("T-02"));
    assert!(content.contains("T-03"));
    assert!(content.contains("T-04"));
    assert!(content.contains("T-05"));
    assert!(content.contains("T-06"));
    assert!(content.contains("T-07"));

    finish_case(&harness, "threat_model_document");
}

#[test]
fn test_canary_secrets_obfuscation_in_web_frames() {
    let harness = TestHarness::new("canary_secrets_obfuscation");

    let bridge = WebRemoteBridge::new(WebRemoteSettings::default());
    let raw_secret = "sk-live-99887766554433221100aabbccddeeff";
    bridge.set_secrets(vec![raw_secret.to_string()]);
    let pairing_token = bridge.manager().generate_token(TokenKind::View).unwrap();
    assert!(bridge.publish_text(
        120,
        24,
        format!("Connecting using OpenAI key {raw_secret}...\nPairing {pairing_token}")
    ));
    let frame = bridge.frame_after(0).expect("sanitized published frame");
    assert_eq!(frame.schema, "pi.web.frame.v1");
    assert!(!frame.data.contains(raw_secret));
    assert!(!frame.data.contains(&pairing_token));
    assert!(frame.data.contains("Connecting using OpenAI key"));
    assert!(frame.data.contains('*'));
    let wire = serde_json::to_string(&*frame).unwrap();
    assert!(!wire.contains(raw_secret));
    assert!(!wire.contains(&pairing_token));

    finish_case(&harness, "canary_secrets_obfuscation");
}

#[test]
fn test_remote_mutating_action_requires_local_approval() {
    let harness = TestHarness::new("remote_approval_gate");

    let settings = WebRemoteSettings {
        port: 8080,
        bind_mode: BindMode::Tailscale,
        view_only: false,
        max_viewers: 2,
        require_auth_token: true,
        enable_audit_log: true,
        ..WebRemoteSettings::default()
    };

    let manager = WebRemoteManager::new(settings);
    let token = manager.generate_token(TokenKind::Steer).unwrap();
    manager
        .connect_client("remote-user", "100.64.0.5:40001", Some(&token))
        .unwrap();

    // A browser request cannot grant itself the first control lease.
    let takeover_result = manager.request_takeover("remote-user");
    assert_eq!(takeover_result, Ok(ControlMode::TakeoverPendingApproval));
    assert!(manager.authorize_input("remote-user", 1).is_err());
    assert!(
        manager
            .audit_log()
            .iter()
            .all(|entry| entry.event_type != "takeover_granted")
    );

    assert_eq!(
        manager.approve_takeover("remote-user"),
        Ok(ControlMode::RemoteControlling)
    );
    let epoch = manager.authorize_input("remote-user", 1).unwrap();
    assert!(manager.input_authorized("remote-user", epoch));
    assert!(manager.authorize_input("remote-user", 1).is_err());
    manager.revoke_control();
    assert!(!manager.input_authorized("remote-user", epoch));
    assert!(manager.authorize_input("remote-user", 2).is_err());

    // Audit logs record takeover grant with provenance
    let logs = manager.audit_log();
    assert!(logs.iter().any(|e| e.event_type == "takeover_granted"
        && e.client_id.as_deref() == Some("remote-user")
        && e.details.get("actor").map(String::as_str) == Some("local_operator")));
    assert!(!serde_json::to_string(&logs).unwrap().contains(&token));
    assert!(!format!("{manager:?}").contains(&token));

    finish_case(&harness, "remote_approval_gate");
}

#[test]
fn test_static_zero_browser_persistence_audit() {
    let harness = TestHarness::new("zero_browser_persistence_audit");

    // Static analysis over all embedded client assets
    for asset in [EMBEDDED_WEB_CLIENT_HTML, EMBEDDED_WEB_CLIENT_JS] {
        assert!(!asset.contains("localStorage"));
        assert!(!asset.contains("sessionStorage"));
        assert!(!asset.contains("indexedDB"));
        assert!(!asset.contains("openDatabase"));
    }

    finish_case(&harness, "zero_browser_persistence_audit");
}

#[test]
fn test_non_loopback_fails_closed_without_tls_and_cli_rejects_lan_bind() {
    let harness = TestHarness::new("non_loopback_tls_fail_closed");

    for bind_mode in [BindMode::Lan, BindMode::Tailscale] {
        let missing_tls = WebRemoteSettings {
            port: 0,
            bind_mode,
            ..WebRemoteSettings::default()
        };
        let err = missing_tls
            .validate()
            .expect_err("non-loopback bind without TLS must fail closed");
        assert!(err.contains("--tls-cert"), "unexpected error: {err}");
        assert!(err.contains("--tls-key"), "unexpected error: {err}");
        assert!(
            err.contains("--allow-insecure-lan"),
            "unexpected error: {err}"
        );
        assert!(WebRemoteServer::start(missing_tls).is_err());
    }

    // Partial TLS (cert without key) also fails closed.
    let partial_tls = WebRemoteSettings {
        port: 0,
        bind_mode: BindMode::Lan,
        tls_cert: Some(harness.temp_path("cert.pem")),
        tls_key: None,
        ..WebRemoteSettings::default()
    };
    assert!(partial_tls.validate().is_err());

    // Loopback 127.0.0.1 succeeds without TLS.
    let mut loopback_server = WebRemoteServer::start(WebRemoteSettings {
        port: 0,
        bind_mode: BindMode::Loopback,
        ..WebRemoteSettings::default()
    })
    .expect("127.0.0.1 loopback web server should bind without TLS");
    assert!(loopback_server.local_addr().ip().is_loopback());
    assert!(
        loopback_server
            .pairing_url()
            .starts_with("http://127.0.0.1:")
    );
    loopback_server.shutdown();

    // Real CLI subprocess check for both `pi web --bind lan` and `pi web-remote --terminal --bind lan`.
    #[cfg(feature = "ftui")]
    for args in [
        &["web", "--bind", "lan"][..],
        &["web-remote", "--terminal", "--bind", "lan"][..],
    ] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_pi"))
            .args(args)
            .output()
            .expect("spawn pi binary");
        assert!(
            !output.status.success(),
            "expected non-zero exit for {args:?}"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("--tls-cert")
                && stderr.contains("--tls-key")
                && stderr.contains("--allow-insecure-lan"),
            "expected actionable TLS error for {args:?}, got: {stderr}"
        );
    }

    finish_case(&harness, "non_loopback_tls_fail_closed");
}

fn test_gf_mul(mut a: u8, mut b: u8) -> u8 {
    let mut prod = 0u8;
    for _ in 0..8 {
        if (b & 1) != 0 {
            prod ^= a;
        }
        let hi = (a & 0x80) != 0;
        a <<= 1;
        if hi {
            a ^= 0x1D;
        }
        b >>= 1;
    }
    prod
}

fn rs_syndromes(codeword: &[u8], ec_len: usize) -> Vec<u8> {
    let mut syndromes = Vec::with_capacity(ec_len);
    let mut alpha_i = 1u8;
    for _ in 0..ec_len {
        let mut eval = 0u8;
        for &byte in codeword {
            eval = test_gf_mul(eval, alpha_i) ^ byte;
        }
        syndromes.push(eval);
        alpha_i = test_gf_mul(alpha_i, 0x02);
    }
    syndromes
}

fn parse_half_block_qr_grid(rendered: &str) -> Vec<Vec<bool>> {
    let glyph_lines: Vec<&str> = rendered
        .lines()
        .filter(|line| !line.starts_with("Payload:"))
        .collect();
    assert!(!glyph_lines.is_empty(), "QR output must have matrix lines");
    let width = glyph_lines[0].chars().count();
    let mut padded = Vec::with_capacity(glyph_lines.len() * 2);
    for line in glyph_lines {
        let chars: Vec<char> = line.chars().collect();
        assert_eq!(chars.len(), width, "uniform QR line width");
        let mut top_row = Vec::with_capacity(width);
        let mut bot_row = Vec::with_capacity(width);
        for ch in chars {
            let (t, b) = match ch {
                '█' => (true, true),
                '▀' => (true, false),
                '▄' => (false, true),
                ' ' => (false, false),
                other => panic!("unexpected non-half-block glyph {other:?}"),
            };
            top_row.push(t);
            bot_row.push(b);
        }
        padded.push(top_row);
        padded.push(bot_row);
    }
    // Strip trailing odd-row padding if total padded height > width.
    while padded.len() > width {
        let last = padded.pop().expect("row");
        assert!(
            last.iter().all(|&m| !m),
            "extra bottom half-row must be white"
        );
    }
    assert_eq!(padded.len(), width, "padded QR matrix must be square");

    // Verify 4-module quiet zone on all 4 sides.
    let quiet = 4usize;
    for (r, row) in padded.iter().enumerate() {
        for (c, &dark) in row.iter().enumerate() {
            if r < quiet || c < quiet || r >= width - quiet || c >= width - quiet {
                assert!(!dark, "quiet zone module ({r}, {c}) must be white");
            }
        }
    }
    let size = width - quiet * 2;
    (quiet..quiet + size)
        .map(|r| padded[r][quiet..quiet + size].to_vec())
        .collect()
}

fn decode_iso_18004_qr(modules: &[Vec<bool>]) -> (Vec<u8>, Vec<u8>, usize) {
    let size = modules.len();
    assert!(size >= 21 && (size - 17).is_multiple_of(4));
    let version = u8::try_from((size - 17) / 4).expect("version");

    // Verify all 3 finder patterns (7x7) and 1-module white separators.
    for (fr, fc) in [(0usize, 0usize), (0, size - 7), (size - 7, 0)] {
        for r in 0..7 {
            for c in 0..7 {
                let expected = r == 0
                    || r == 6
                    || c == 0
                    || c == 6
                    || ((2..=4).contains(&r) && (2..=4).contains(&c));
                assert_eq!(
                    modules[fr + r][fc + c],
                    expected,
                    "finder mismatch at ({}, {})",
                    fr + r,
                    fc + c
                );
            }
        }
    }
    // Dark module check.
    assert!(
        modules[size - 8][8],
        "ISO/IEC 18004 dark module must be dark"
    );

    // Timing pattern check outside finders/alignments.
    assert!(modules[6][8] && !modules[6][9] && modules[8][6] && !modules[9][6]);

    // Decode both copies of 15-bit BCH(15,5) format info and verify equality + 0 BCH remainder.
    let tl_coords = [
        (0usize, 8usize),
        (1, 8),
        (2, 8),
        (3, 8),
        (4, 8),
        (5, 8),
        (7, 8),
        (8, 8),
        (8, 7),
        (8, 5),
        (8, 4),
        (8, 3),
        (8, 2),
        (8, 1),
        (8, 0),
    ];
    let mut fmt_copy1 = 0u16;
    for (idx, (r, c)) in tl_coords.into_iter().enumerate() {
        if modules[r][c] {
            fmt_copy1 |= 1u16 << idx;
        }
    }
    let mut fmt_copy2 = 0u16;
    for idx in 0..8usize {
        if modules[8][size - 1 - idx] {
            fmt_copy2 |= 1u16 << idx;
        }
    }
    for idx in 8..15usize {
        if modules[size - 15 + idx][8] {
            fmt_copy2 |= 1u16 << idx;
        }
    }
    assert_eq!(
        fmt_copy1, fmt_copy2,
        "both QR format info copies must match"
    );
    let unmasked_fmt = fmt_copy1 ^ 0x5412;
    let mut bch_rem = unmasked_fmt;
    for shift in (10..15).rev() {
        if ((bch_rem >> shift) & 1) != 0 {
            bch_rem ^= 0x537 << (shift - 10);
        }
    }
    assert_eq!(bch_rem, 0, "format info BCH(15,5) syndrome must be zero");
    let ecc_indicator = (unmasked_fmt >> 13) & 0b11;
    assert_eq!(ecc_indicator, 0b01, "expected ECC Level L indicator 0b01");
    let mask = u8::try_from((unmasked_fmt >> 10) & 0b111).expect("mask");

    // Reconstruct function-module mask for versions 1..=6.
    let mut is_function = vec![vec![false; size]; size];
    let finder_edge = isize::try_from(size - 7).expect("QR size fits isize");
    let size_isize = isize::try_from(size).expect("QR size fits isize");
    for (fr, fc) in [(0isize, 0isize), (0, finder_edge), (finder_edge, 0)] {
        for dr in -1isize..=7 {
            for dc in -1isize..=7 {
                let rr = fr + dr;
                let cc = fc + dc;
                if rr >= 0 && cc >= 0 && rr < size_isize && cc < size_isize {
                    let r = usize::try_from(rr).expect("non-negative row");
                    let c = usize::try_from(cc).expect("non-negative col");
                    is_function[r][c] = true;
                }
            }
        }
    }
    if version >= 2 {
        let center = isize::try_from(size - 7).expect("center fits isize");
        for dr in -2isize..=2 {
            for dc in -2isize..=2 {
                let r = usize::try_from(center + dr).expect("non-negative row");
                let c = usize::try_from(center + dc).expect("non-negative col");
                let dist = dr.abs().max(dc.abs());
                assert_eq!(modules[r][c], dist != 1, "alignment pattern mismatch");
                is_function[r][c] = true;
            }
        }
    }
    for idx in 8..(size - 8) {
        is_function[6][idx] = true;
        is_function[idx][6] = true;
    }
    is_function[size - 8][8] = true;
    for idx in 0..9 {
        is_function[8][idx] = true;
        is_function[idx][8] = true;
    }
    for idx in 0..8 {
        is_function[8][size - 1 - idx] = true;
    }
    for idx in 0..7 {
        is_function[size - 1 - idx][8] = true;
    }

    let mask_fn = |r: usize, c: usize| -> bool {
        match mask {
            0 => (r + c).is_multiple_of(2),
            1 => r.is_multiple_of(2),
            2 => c.is_multiple_of(3),
            3 => (r + c).is_multiple_of(3),
            4 => ((r / 2) + (c / 3)).is_multiple_of(2),
            5 => ((r * c) % 2) + ((r * c) % 3) == 0,
            6 => (((r * c) % 2) + ((r * c) % 3)).is_multiple_of(2),
            7 => (((r + c) % 2) + ((r * c) % 3)).is_multiple_of(2),
            _ => unreachable!(),
        }
    };

    let mut bits = Vec::new();
    let mut right = isize::try_from(size - 1).expect("QR size fits isize");
    let mut upward = true;
    while right > 0 {
        if right == 6 {
            right -= 1;
        }
        for step in 0..size {
            let r = if upward { size - 1 - step } else { step };
            for c_off in [0isize, 1isize] {
                let c = usize::try_from(right - c_off).expect("QR column non-negative");
                if !is_function[r][c] {
                    let bit = modules[r][c] ^ mask_fn(r, c);
                    bits.push(bit);
                }
            }
        }
        upward = !upward;
        right -= 2;
    }

    let (data_cw_len, ec_cw_len) = match version {
        1 => (19usize, 7usize),
        2 => (34, 10),
        3 => (55, 15),
        4 => (80, 20),
        5 => (108, 26),
        _ => panic!("unexpected QR version {version} for pairing URL"),
    };
    let total_cw = data_cw_len + ec_cw_len;
    let mut codewords = Vec::with_capacity(total_cw);
    for &chunk in bits[..total_cw * 8].as_chunks::<8>().0 {
        let mut b = 0u8;
        for bit in chunk {
            b = (b << 1) | u8::from(bit);
        }
        codewords.push(b);
    }

    let read_bits = |start: usize, len: usize| -> usize {
        let mut val = 0usize;
        for &bit in &bits[start..start + len] {
            val = (val << 1) | usize::from(bit);
        }
        val
    };
    let mode = read_bits(0, 4);
    assert_eq!(mode, 0b0100, "expected byte mode 0100");
    let byte_len = read_bits(4, 8);
    let mut payload = Vec::with_capacity(byte_len);
    for idx in 0..byte_len {
        payload.push(u8::try_from(read_bits(12 + idx * 8, 8)).expect("byte"));
    }
    (payload, codewords, ec_cw_len)
}

#[test]
fn test_half_block_qr_iso_18004_round_trip_and_reed_solomon_parity() {
    let harness = TestHarness::new("qr_iso_18004_round_trip");
    let target_url = "https://192.168.1.10:8443/#token=abc";
    let rendered = render_half_block_qr(target_url);

    let mut grid = parse_half_block_qr_grid(&rendered);
    let (decoded_bytes, codewords, ec_len) = decode_iso_18004_qr(&grid);
    let syndromes = rs_syndromes(&codewords, ec_len);
    assert!(
        syndromes.iter().all(|&s| s == 0),
        "all Reed-Solomon syndromes must be zero, got {syndromes:?}"
    );
    assert_eq!(
        String::from_utf8(decoded_bytes).expect("utf8 URL"),
        target_url
    );

    // Planted negative: flip a single payload module (past the 4-bit mode + 8-bit length header) and verify RS detects corruption.
    let last = grid.len() - 1;
    grid[last - 6][last] = !grid[last - 6][last];
    let (_, corrupted_codewords, _) = decode_iso_18004_qr(&grid);
    let bad_syndromes = rs_syndromes(&corrupted_codewords, ec_len);
    assert!(
        bad_syndromes.iter().any(|&s| s != 0),
        "Reed-Solomon syndromes must detect single-module bit flip"
    );

    finish_case(&harness, "qr_iso_18004_round_trip");
}
