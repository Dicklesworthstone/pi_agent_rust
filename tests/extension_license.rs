//! Integration tests for the extension_license module.

use pi::extension_license::{
    License, PolicyVerdict, Redistributable, ScreeningInput, ScreeningReport, SecuritySeverity,
    VerdictStatus, detect_license_from_content, detect_license_from_spdx, redistributable,
    scan_security, screen_extensions,
};

// ────────────────────────────────────────────────────────────────────────────
// License detection from content
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn detect_mit_full_text() {
    let content = r#"MIT License

Copyright (c) 2024 Test Author

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED."#;
    assert_eq!(detect_license_from_content(content), License::Mit);
}

#[test]
fn detect_apache2_full_header() {
    let content = r#"
                                 Apache License
                           Version 2.0, January 2004
                        http://www.apache.org/licenses/

   TERMS AND CONDITIONS FOR USE, REPRODUCTION, AND DISTRIBUTION
"#;
    assert_eq!(detect_license_from_content(content), License::Apache2);
}

#[test]
fn detect_isc_license() {
    let content = "ISC License\n\nCopyright (c) 2024\n\nPermission to use, copy, modify...";
    assert_eq!(detect_license_from_content(content), License::Isc);
}

#[test]
fn detect_bsd3_license() {
    let content = "Redistribution and use in source and binary forms, with or without modification...\nNeither the name of the copyright holder...";
    assert_eq!(detect_license_from_content(content), License::Bsd3);
}

#[test]
fn detect_bsd2_license() {
    let content = "Redistribution and use in source and binary forms, with or without modification...\nProvided that conditions are met.";
    assert_eq!(detect_license_from_content(content), License::Bsd2);
}

#[test]
fn detect_gpl2_license() {
    let content = "GNU GENERAL PUBLIC LICENSE\nVersion 2, June 1991";
    assert_eq!(detect_license_from_content(content), License::Gpl2);
}

#[test]
fn detect_agpl3_license() {
    let content = "GNU AFFERO GENERAL PUBLIC LICENSE\nVersion 3, 19 November 2007";
    assert_eq!(detect_license_from_content(content), License::Agpl3);
}

#[test]
fn detect_lgpl21_license() {
    let content = "GNU LESSER GENERAL PUBLIC LICENSE\nVersion 2.1, February 1999";
    assert_eq!(detect_license_from_content(content), License::Lgpl21);
}

#[test]
fn detect_mpl2_license() {
    let content = "Mozilla Public License Version 2.0";
    assert_eq!(detect_license_from_content(content), License::Mpl2);
}

#[test]
fn detect_unlicense() {
    let content = "This is free and unencumbered software released into the public domain.";
    assert_eq!(detect_license_from_content(content), License::Unlicense);
}

#[test]
fn detect_cc0_license() {
    let content = "Creative Commons Zero v1.0 Universal\nCC0 1.0";
    assert_eq!(detect_license_from_content(content), License::Cc0);
}

#[test]
fn detect_unknown_content() {
    let content = "This is a proprietary license agreement between parties.";
    assert_eq!(detect_license_from_content(content), License::Unknown);
}

#[test]
fn detect_empty_content() {
    assert_eq!(detect_license_from_content(""), License::Unknown);
}

// ────────────────────────────────────────────────────────────────────────────
// SPDX detection
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn spdx_all_permissive() {
    assert_eq!(detect_license_from_spdx("MIT"), License::Mit);
    assert_eq!(detect_license_from_spdx("Apache-2.0"), License::Apache2);
    assert_eq!(detect_license_from_spdx("ISC"), License::Isc);
    assert_eq!(detect_license_from_spdx("BSD-2-Clause"), License::Bsd2);
    assert_eq!(detect_license_from_spdx("BSD-3-Clause"), License::Bsd3);
    assert_eq!(detect_license_from_spdx("Unlicense"), License::Unlicense);
    assert_eq!(detect_license_from_spdx("CC0-1.0"), License::Cc0);
}

#[test]
fn spdx_all_copyleft() {
    assert_eq!(detect_license_from_spdx("GPL-2.0"), License::Gpl2);
    assert_eq!(detect_license_from_spdx("GPL-3.0"), License::Gpl3);
    assert_eq!(detect_license_from_spdx("AGPL-3.0"), License::Agpl3);
    assert_eq!(detect_license_from_spdx("LGPL-2.1"), License::Lgpl21);
    assert_eq!(detect_license_from_spdx("MPL-2.0"), License::Mpl2);
}

#[test]
fn spdx_case_insensitive() {
    assert_eq!(detect_license_from_spdx("mit"), License::Mit);
    assert_eq!(detect_license_from_spdx("apache-2.0"), License::Apache2);
}

#[test]
fn spdx_gpl_variants() {
    assert_eq!(detect_license_from_spdx("GPL-2.0-only"), License::Gpl2);
    assert_eq!(detect_license_from_spdx("GPL-2.0-or-later"), License::Gpl2);
    assert_eq!(detect_license_from_spdx("GPL-3.0-only"), License::Gpl3);
    assert_eq!(detect_license_from_spdx("GPL-3.0-or-later"), License::Gpl3);
    assert_eq!(detect_license_from_spdx("AGPL-3.0-only"), License::Agpl3);
    assert_eq!(
        detect_license_from_spdx("AGPL-3.0-or-later"),
        License::Agpl3
    );
    assert_eq!(detect_license_from_spdx("LGPL-2.1-only"), License::Lgpl21);
    assert_eq!(
        detect_license_from_spdx("LGPL-2.1-or-later"),
        License::Lgpl21
    );
}

#[test]
fn spdx_whitespace_handling() {
    assert_eq!(detect_license_from_spdx("  MIT  "), License::Mit);
    assert_eq!(detect_license_from_spdx("Apache 2.0"), License::Apache2);
}

// ────────────────────────────────────────────────────────────────────────────
// Redistributability
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn redist_permissive_all_yes() {
    for license in &[
        License::Mit,
        License::Apache2,
        License::Isc,
        License::Bsd2,
        License::Bsd3,
        License::Unlicense,
        License::Cc0,
    ] {
        assert_eq!(
            redistributable(license),
            Redistributable::Yes,
            "Expected Yes for {license}"
        );
    }
}

#[test]
fn redist_copyleft_all() {
    for license in &[
        License::Gpl2,
        License::Gpl3,
        License::Agpl3,
        License::Lgpl21,
        License::Mpl2,
    ] {
        assert_eq!(
            redistributable(license),
            Redistributable::Copyleft,
            "Expected Copyleft for {license}"
        );
    }
}

#[test]
fn redist_unknown_variants() {
    assert_eq!(redistributable(&License::Unknown), Redistributable::Unknown);
    assert_eq!(
        redistributable(&License::Custom("WTFPL".into())),
        Redistributable::Unknown
    );
}

// ────────────────────────────────────────────────────────────────────────────
// Security scanning
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn security_clean_code() {
    let code = r#"
        import Anthropic from "@anthropic-ai/sdk";
        export default function() {
            return { name: "hello" };
        }
    "#;
    assert!(scan_security(code).is_empty());
}

#[test]
fn security_eval_detected() {
    let code = "const result = eval(userInput);";
    let findings = scan_security(code);
    assert!(!findings.is_empty());
    assert!(
        findings
            .iter()
            .any(|f| f.severity == SecuritySeverity::Warning && f.pattern == "eval(")
    );
}

#[test]
fn security_new_function_detected() {
    let code = "const fn = new Function('return 42');";
    let findings = scan_security(code);
    assert!(findings.iter().any(|f| f.pattern == "new Function("));
}

#[test]
fn security_cookie_access_critical() {
    let code = "const session = document.cookie;";
    let findings = scan_security(code);
    assert!(
        findings
            .iter()
            .any(|f| f.severity == SecuritySeverity::Critical)
    );
}

#[test]
fn security_child_process_info() {
    let code = "const { exec } = require('child_process');";
    let findings = scan_security(code);
    assert!(
        findings
            .iter()
            .any(|f| f.severity == SecuritySeverity::Info && f.pattern == "child_process")
    );
}

#[test]
fn security_multiple_findings() {
    let code = "eval(x); const c = document.cookie; new Function('x');";
    let findings = scan_security(code);
    assert!(findings.len() >= 3);
}

#[test]
fn security_http_fetch_warning() {
    let code = r#"fetch("http://example.com/api")"#;
    let findings = scan_security(code);
    assert!(
        findings
            .iter()
            .any(|f| f.severity == SecuritySeverity::Warning)
    );
}

// ────────────────────────────────────────────────────────────────────────────
// Policy screening pipeline
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn screen_empty_input() {
    let report = screen_extensions(&[], "test-task");
    assert_eq!(report.stats.total_screened, 0);
    assert_eq!(report.stats.pass, 0);
    assert_eq!(report.stats.needs_review, 0);
    assert!(!report.generated_at.is_empty());
    assert_eq!(report.task, "test-task");
}

#[test]
fn screen_all_mit() {
    let inputs: Vec<ScreeningInput> = (0..5)
        .map(|i| ScreeningInput {
            canonical_id: format!("owner/ext-{i}"),
            known_license: Some("MIT".to_string()),
            source_tier: Some("community".to_string()),
        })
        .collect();
    let report = screen_extensions(&inputs, "test");
    assert_eq!(report.stats.total_screened, 5);
    assert_eq!(report.stats.pass, 5);
    assert_eq!(report.stats.pass_with_warnings, 0);
    assert_eq!(report.stats.excluded, 0);
    assert_eq!(report.stats.needs_review, 0);
}

#[test]
fn screen_mixed_licenses() {
    let inputs = vec![
        ScreeningInput {
            canonical_id: "a/mit".into(),
            known_license: Some("MIT".into()),
            source_tier: None,
        },
        ScreeningInput {
            canonical_id: "b/apache".into(),
            known_license: Some("Apache-2.0".into()),
            source_tier: None,
        },
        ScreeningInput {
            canonical_id: "c/gpl".into(),
            known_license: Some("GPL-3.0".into()),
            source_tier: None,
        },
        ScreeningInput {
            canonical_id: "d/unknown".into(),
            known_license: None,
            source_tier: None,
        },
    ];
    let report = screen_extensions(&inputs, "test");
    assert_eq!(report.stats.pass, 2); // MIT + Apache
    assert_eq!(report.stats.pass_with_warnings, 1); // GPL
    assert_eq!(report.stats.needs_review, 1); // unknown
}

#[test]
fn screen_verdicts_sorted_by_canonical_id() {
    let inputs = vec![
        ScreeningInput {
            canonical_id: "z/last".into(),
            known_license: Some("MIT".into()),
            source_tier: None,
        },
        ScreeningInput {
            canonical_id: "a/first".into(),
            known_license: Some("MIT".into()),
            source_tier: None,
        },
        ScreeningInput {
            canonical_id: "m/middle".into(),
            known_license: Some("MIT".into()),
            source_tier: None,
        },
    ];
    let report = screen_extensions(&inputs, "test");
    let ids: Vec<&str> = report
        .verdicts
        .iter()
        .map(|v| v.canonical_id.as_str())
        .collect();
    assert_eq!(ids, vec!["a/first", "m/middle", "z/last"]);
}

#[test]
fn screen_license_distribution() {
    let inputs = vec![
        ScreeningInput {
            canonical_id: "a/x".into(),
            known_license: Some("MIT".into()),
            source_tier: None,
        },
        ScreeningInput {
            canonical_id: "b/y".into(),
            known_license: Some("MIT".into()),
            source_tier: None,
        },
        ScreeningInput {
            canonical_id: "c/z".into(),
            known_license: Some("Apache-2.0".into()),
            source_tier: None,
        },
    ];
    let report = screen_extensions(&inputs, "test");
    assert_eq!(report.stats.license_distribution.get("MIT"), Some(&2));
    assert_eq!(
        report.stats.license_distribution.get("Apache-2.0"),
        Some(&1)
    );
}

// ────────────────────────────────────────────────────────────────────────────
// Serde round-trips
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn verdict_serde_all_statuses() {
    for status in &[
        VerdictStatus::Pass,
        VerdictStatus::PassWithWarnings,
        VerdictStatus::Excluded,
        VerdictStatus::NeedsReview,
    ] {
        let v = PolicyVerdict {
            canonical_id: "test/ext".into(),
            license: "MIT".into(),
            license_source: "pool".into(),
            redistributable: Redistributable::Yes,
            security_findings: vec![],
            verdict: *status,
            notes: "test".into(),
        };
        let json = serde_json::to_string(&v).unwrap();
        let back: PolicyVerdict = serde_json::from_str(&json).unwrap();
        assert_eq!(back.verdict, *status);
    }
}

#[test]
fn report_serde_round_trip() {
    let report = screen_extensions(
        &[ScreeningInput {
            canonical_id: "test/ext".into(),
            known_license: Some("MIT".into()),
            source_tier: Some("community".into()),
        }],
        "serde-test",
    );
    let json = serde_json::to_string_pretty(&report).unwrap();
    let back: ScreeningReport = serde_json::from_str(&json).unwrap();
    assert_eq!(back.stats.total_screened, 1);
    assert_eq!(back.stats.pass, 1);
    assert_eq!(back.verdicts[0].canonical_id, "test/ext");
}

// ────────────────────────────────────────────────────────────────────────────
// Golden corpus test with real data
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn golden_corpus_screening() {
    // Only run if real data files exist.
    let validated_path = "docs/extension-validated-dedup.json";
    let pool_path = "docs/extension-candidate-pool.json";

    if !std::path::Path::new(validated_path).exists() || !std::path::Path::new(pool_path).exists() {
        eprintln!("Skipping golden corpus test: data files not found");
        return;
    }

    let validated_text = std::fs::read_to_string(validated_path).unwrap();
    let validated: pi::extension_validation::ValidationReport =
        serde_json::from_str(&validated_text).unwrap();

    let pool_text = std::fs::read_to_string(pool_path).unwrap();
    let pool: pi::extension_popularity::CandidatePool = serde_json::from_str(&pool_text).unwrap();

    // Build license map.
    let mut license_map = std::collections::HashMap::new();
    for item in &pool.items {
        if item.license != "UNKNOWN" && !item.license.is_empty() {
            license_map.insert(item.id.clone(), item.license.clone());
            license_map.insert(item.name.clone(), item.license.clone());
        }
    }

    let inputs: Vec<ScreeningInput> = validated
        .candidates
        .iter()
        .filter(|c| c.status == pi::extension_validation::ValidationStatus::TrueExtension)
        .map(|c| ScreeningInput {
            canonical_id: c.canonical_id.clone(),
            known_license: license_map
                .get(&c.canonical_id)
                .or_else(|| license_map.get(&c.name))
                .cloned(),
            source_tier: c.source_tier.clone(),
        })
        .collect();

    let report = screen_extensions(&inputs, "golden-test");

    // Basic sanity checks.
    assert!(
        report.stats.total_screened >= 300,
        "Expected at least 300 extensions, got {}",
        report.stats.total_screened
    );
    assert!(
        report.stats.pass >= 100,
        "Expected at least 100 passing (MIT/Apache), got {}",
        report.stats.pass
    );

    // All verdicts should have non-empty canonical_id.
    for v in &report.verdicts {
        assert!(!v.canonical_id.is_empty(), "Empty canonical_id in verdict");
    }

    // License distribution should have at least MIT.
    assert!(
        report.stats.license_distribution.contains_key("MIT"),
        "Expected MIT in license distribution"
    );

    // Stats should sum correctly.
    let sum = report.stats.pass
        + report.stats.pass_with_warnings
        + report.stats.excluded
        + report.stats.needs_review;
    assert_eq!(
        sum, report.stats.total_screened,
        "Stats don't sum to total_screened"
    );

    eprintln!(
        "Golden corpus: {} screened, {} pass, {} needs_review",
        report.stats.total_screened, report.stats.pass, report.stats.needs_review
    );
}

// ────────────────────────────────────────────────────────────────────────────
// Edge cases
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn license_spdx_display() {
    assert_eq!(License::Mit.spdx(), "MIT");
    assert_eq!(License::Apache2.spdx(), "Apache-2.0");
    assert_eq!(License::Gpl3.spdx(), "GPL-3.0");
    assert_eq!(License::Unknown.spdx(), "UNKNOWN");
    assert_eq!(License::Custom("BUSL-1.1".into()).spdx(), "BUSL-1.1");
}

#[test]
fn license_display_matches_spdx() {
    let licenses = vec![
        License::Mit,
        License::Apache2,
        License::Isc,
        License::Bsd2,
        License::Bsd3,
        License::Gpl2,
        License::Gpl3,
        License::Agpl3,
        License::Lgpl21,
        License::Mpl2,
        License::Unlicense,
        License::Cc0,
        License::Unknown,
    ];
    for lic in licenses {
        assert_eq!(lic.to_string(), lic.spdx());
    }
}

#[test]
fn screen_custom_license_needs_review() {
    let inputs = vec![ScreeningInput {
        canonical_id: "test/custom".into(),
        known_license: Some("BUSL-1.1".into()),
        source_tier: None,
    }];
    let report = screen_extensions(&inputs, "test");
    assert_eq!(report.stats.needs_review, 1);
    assert_eq!(report.verdicts[0].verdict, VerdictStatus::NeedsReview);
}
