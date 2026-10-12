use super::*;
use super::super::tests::fixture;

fn input_for(bytes: &[u8], decision: Decision) -> Input {
    let snapshot = Snapshot::parse(bytes).unwrap();
    Input {
        _op: "disposition_dependencies".into(),
        current: "audit.sarif".into(),
        expected_sha256: snapshot.digest,
        fingerprint: snapshot.findings.keys().next().unwrap().clone(),
        decision,
        reason: "Reviewed applicability with the maintainer".into(),
        sarif_out: "reviewed.sarif".into(),
    }
}

fn review(document: &Value, decision: Decision) -> Value {
    let bytes = serde_json::to_vec(document).unwrap();
    let snapshot = Snapshot::parse(&bytes).unwrap();
    let (reviewed, _) = annotate(&bytes, &snapshot, &input_for(&bytes, decision)).unwrap();
    Snapshot::parse(&serde_json::to_vec(&reviewed).unwrap()).unwrap();
    reviewed
}

#[test]
fn decisions_preserve_matches_coverage_and_foreign_metadata() {
    let mut original = fixture("npm", &[("1.0.0", &["GHSA-a", "GHSA-b"], true)]);
    original["runs"][0]["results"][0]["properties"]["externalNotes"] = json!({"keep":true});
    original["runs"][0]["results"][0]["suppressions"] = json!([{
        "kind":"inSource","status":"underReview","justification":"Other reviewer"
    }]);
    for decision in [Decision::AcceptedRisk, Decision::FalsePositive] {
        let reviewed = review(&original, decision);
        assert_eq!(reviewed["runs"][0]["properties"], original["runs"][0]["properties"]);
        assert_eq!(reviewed["runs"][0]["invocations"], original["runs"][0]["invocations"]);
        assert_eq!(reviewed["runs"][0]["results"].as_array().unwrap().len(), 2);
        assert!(reviewed["runs"][0]["results"].as_array().unwrap().iter()
            .all(|finding| finding["suppressions"].is_array()));
        for (old, new) in original["runs"][0]["results"].as_array().unwrap().iter()
            .zip(reviewed["runs"][0]["results"].as_array().unwrap())
        {
            assert_eq!(old["fingerprints"], new["fingerprints"]);
            assert_eq!(old["properties"]["dependency"], new["properties"]["dependency"]);
            assert_eq!(old["properties"]["externalNotes"], new["properties"]["externalNotes"]);
            if let Some(old_suppressions) = old.get("suppressions").and_then(Value::as_array) {
                for suppression in old_suppressions {
                    assert!(new["suppressions"].as_array().unwrap().contains(suppression));
                }
            }
        }
        let before = Snapshot::parse(&serde_json::to_vec(&original).unwrap()).unwrap();
        let after = Snapshot::parse(&serde_json::to_vec(&reviewed).unwrap()).unwrap();
        assert_eq!(after.findings, before.findings);
        assert_eq!(after.packages, before.packages);
        assert_eq!(after.reviews.len(), 1);
        let output = super::super::compare_snapshots(&before, &after, 0, 20).unwrap();
        let details = output.details.unwrap();
        assert_eq!(details["counts"]["persistent"], 2);
        assert_eq!(details["counts"]["no_longer_observed"], 0);
        assert_eq!(details["current"]["reviewedFindings"], 1);
        assert!(details["findings"].as_array().unwrap().iter().any(|finding| {
            finding["currentDecision"] == serde_json::to_value(decision).unwrap()
        }));
    }
}

#[test]
fn reopen_removes_only_owned_suppression_and_does_not_change_future_audits() {
    let mut original = fixture("crates.io", &[("1.0.0", &["GHSA-a"], true)]);
    let foreign = json!({"kind":"external","status":"accepted","justification":"Independent disposition"});
    original["runs"][0]["results"][0]["suppressions"] = json!([foreign.clone()]);
    let accepted = review(&original, Decision::AcceptedRisk);
    let changed = review(&accepted, Decision::FalsePositive);
    assert_eq!(changed["runs"][0]["results"][0]["suppressions"].as_array().unwrap().len(), 2);
    let reopened = review(&changed, Decision::Reopen);
    assert_eq!(reopened["runs"][0]["results"][0]["suppressions"], json!([foreign]));
    let record = &reopened["runs"][0]["results"][0]["properties"]["piDependencyReview"];
    assert_eq!(record["decision"], "reopen");
    assert_eq!(record["sourceSha256"], snapshot::digest(&serde_json::to_vec(&changed).unwrap()));
    let future = fixture("crates.io", &[("1.0.0", &["GHSA-a"], true)]);
    let parsed = Snapshot::parse(&serde_json::to_vec(&future).unwrap()).unwrap();
    assert!(parsed.reviews.is_empty());
    assert_eq!(parsed.findings.len(), 1);
}

#[test]
fn null_suppression_metadata_is_admitted_then_normalized_consistently() {
    let mut original = fixture("crates.io", &[("1.0.0", &["GHSA-a", "GHSA-b"], true)]);
    for result in original["runs"][0]["results"].as_array_mut().unwrap() {
        result["suppressions"] = Value::Null;
    }
    let reviewed = review(&original, Decision::AcceptedRisk);
    let suppressions: Vec<_> = reviewed["runs"][0]["results"].as_array().unwrap().iter()
        .map(|finding| finding["suppressions"].as_array().unwrap()).collect();
    assert_eq!(suppressions.iter().map(|items| items.len()).sum::<usize>(), 1);
    assert_eq!(suppressions.iter().filter(|items| items.is_empty()).count(), 1);
}

#[test]
fn copied_malformed_and_conflicting_review_records_are_rejected() {
    let original = fixture("crates.io", &[("1.0.0", &["GHSA-a"], true)]);
    let reviewed = review(&original, Decision::AcceptedRisk);
    for pointer in [
        "/runs/0/results/0/properties/piDependencyReview/fingerprint",
        "/runs/0/results/0/properties/piDependencyReview/schema",
        "/runs/0/results/0/properties/piDependencyReview/advisoryModified",
        "/runs/0/results/0/properties/piDependencyReview/sourceSha256",
        "/runs/0/results/0/properties/piDependencyReview/reviewedAt",
        "/runs/0/results/0/properties/piDependencyReview/decision",
        "/runs/0/results/0/suppressions/0/justification",
    ] {
        let mut broken = reviewed.clone();
        *broken.pointer_mut(pointer).unwrap() = json!("PRIVATE-CANARY");
        let failure = Snapshot::parse(&serde_json::to_vec(&broken).unwrap()).err().expect("refused");
        assert!(!failure.to_string().contains("PRIVATE-CANARY"));
    }
    let mut copied = fixture("crates.io", &[("2.0.0", &["GHSA-a"], true)]);
    copied["runs"][0]["results"][0]["properties"]["piDependencyReview"] =
        reviewed["runs"][0]["results"][0]["properties"]["piDependencyReview"].clone();
    copied["runs"][0]["results"][0]["suppressions"] = reviewed["runs"][0]["results"][0]["suppressions"].clone();
    assert!(Snapshot::parse(&serde_json::to_vec(&copied).unwrap()).is_err());
    let mut missing = reviewed.clone();
    missing["runs"][0]["results"][0]["properties"].as_object_mut().unwrap().remove("piDependencyReview");
    assert!(Snapshot::parse(&serde_json::to_vec(&missing).unwrap()).is_err());
    let mut duplicate = reviewed.clone();
    let suppression = duplicate["runs"][0]["results"][0]["suppressions"][0].clone();
    duplicate["runs"][0]["results"][0]["suppressions"].as_array_mut().unwrap().push(suppression);
    assert!(Snapshot::parse(&serde_json::to_vec(&duplicate).unwrap()).is_err());
    let raw = serde_json::to_string(&reviewed).unwrap();
    let duplicate = raw.replacen("\"decision\":\"accepted_risk\"", "\"decision\":\"reopen\",\"decision\":\"accepted_risk\"", 1);
    assert_ne!(duplicate, raw);
    assert!(Snapshot::parse(duplicate.as_bytes()).is_err());
}

#[test]
fn decision_refusals_do_not_echo_private_reason_or_repair_query_failures() {
    let original = fixture("crates.io", &[("1.0.0", &["GHSA-a"], false)]);
    let annotated = review(&original, Decision::FalsePositive);
    let parsed = Snapshot::parse(&serde_json::to_vec(&annotated).unwrap()).unwrap();
    assert!(!parsed.query_complete);
    assert_eq!(parsed.status, "incomplete");
    assert_eq!(annotated["runs"][0]["invocations"][0]["executionSuccessful"], false);
    assert!(super::super::compare_snapshots(&parsed, &parsed, 0, 20).unwrap().is_error);
    for reason in [" ".to_string(), "PRIVATE-CANARY".repeat(200), "PRIVATE-CANARY\0".to_string()] {
        assert!(!valid_reason(&reason));
    }
    assert!(valid_reason("Exact applicability rationale\nincluding a second line"));
    assert!(valid_reason(&"x".repeat(2048)));
    assert!(!valid_reason(&"x".repeat(2049)));
}

#[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
mod files {
    use super::*;
    use crate::security_scan::SecurityScanTool;
    use crate::tools::Tool as _;

    fn runtime() -> asupersync::runtime::Runtime {
        asupersync::runtime::RuntimeBuilder::new()
            .enable_parking(false)
            .worker_threads(2)
            .blocking_threads(1, 8)
            .build()
            .unwrap()
    }

    fn write_source(dir: &Path, complete: bool) -> (Vec<u8>, Value) {
        let bytes = serde_json::to_vec(&fixture("crates.io", &[("1.0.0", &["GHSA-a"], complete)])).unwrap();
        std::fs::write(dir.join("audit.sarif"), &bytes).unwrap();
        let input = input_for(&bytes, Decision::AcceptedRisk);
        let command = json!({"op":"disposition_dependencies","current":"audit.sarif",
            "expectedSha256":input.expected_sha256,"fingerprint":input.fingerprint,
            "decision":"accepted_risk","reason":"PRIVATE-REVIEW-REASON","sarifOut":"reviewed.sarif"});
        (bytes, command)
    }

    #[test]
    fn public_tool_publishes_verified_new_artifact_without_echoing_reason_or_losing_coverage() {
        let runtime = runtime();
        for complete in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let (source, command) = write_source(dir.path(), complete);
            let tool = SecurityScanTool::new(dir.path());
            let output = runtime.block_on(tool.execute("review", command, None)).unwrap();
            assert_eq!(output.is_error, !complete);
            assert!(!serde_json::to_string(&output).unwrap().contains("PRIVATE-REVIEW-REASON"));
            assert_eq!(std::fs::read(dir.path().join("audit.sarif")).unwrap(), source);
            let bytes = std::fs::read(dir.path().join("reviewed.sarif")).unwrap();
            let parsed = Snapshot::parse(&bytes).unwrap();
            assert_eq!(parsed.query_complete, complete);
            assert_eq!(parsed.findings.len(), 1);
            assert_eq!(parsed.reviews.len(), 1);
            let details = output.details.unwrap();
            assert_eq!(details["sha256"], snapshot::digest(&bytes));
            assert_eq!(details["sourceSha256"], snapshot::digest(&source));
            assert_eq!(details["futureAuditsAffected"], false);
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2, "no staging leak");
        }
    }

    #[test]
    fn stale_hash_missing_finding_collisions_and_symlinks_leave_inputs_and_outputs_untouched() {
        let runtime = runtime();
        let dir = tempfile::tempdir().unwrap();
        let (source, command) = write_source(dir.path(), true);
        let tool = SecurityScanTool::new(dir.path());
        for field in ["expectedSha256", "fingerprint"] {
            let mut wrong = command.clone();
            wrong[field] = json!("0".repeat(64));
            assert!(runtime.block_on(tool.execute("stale", wrong, None)).is_err());
            assert!(!dir.path().join("reviewed.sarif").exists());
        }
        std::fs::write(dir.path().join("reviewed.sarif"), "preserve-existing").unwrap();
        assert!(runtime.block_on(tool.execute("collision", command.clone(), None)).is_err());
        assert_eq!(std::fs::read_to_string(dir.path().join("reviewed.sarif")).unwrap(), "preserve-existing");
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("canary"), "PRIVATE-CANARY").unwrap();
        std::os::unix::fs::symlink(outside.path().join("canary"), dir.path().join("linked.sarif")).unwrap();
        let mut linked = command.clone();
        linked["sarifOut"] = json!("linked.sarif");
        assert!(runtime.block_on(tool.execute("link", linked, None)).is_err());
        assert_eq!(std::fs::read_to_string(outside.path().join("canary")).unwrap(), "PRIVATE-CANARY");
        assert_eq!(std::fs::read(dir.path().join("audit.sarif")).unwrap(), source);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 3, "no staging leak");
        let mut overwrite = command;
        overwrite["sarifOut"] = json!("audit.sarif");
        assert!(runtime.block_on(tool.execute("same", overwrite, None)).is_err());
    }

    #[test]
    fn cancelled_and_capability_denied_owners_cannot_publish() {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::open(dir.path()).unwrap();
        let owner = AgentCx::for_request();
        owner.cancel_with(asupersync::types::CancelKind::User, Some("stop before publish"));
        assert!(root.publish("denied.sarif", b"irrelevant", &owner).is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        let (source, command) = write_source(dir.path(), true);
        let restricted = asupersync::Cx::for_request().restrict::<asupersync::cx::cap::None>();
        let _guard = restricted.set_current_restricted();
        let tool = SecurityScanTool::new(dir.path());
        let failure = futures::executor::block_on(tool.execute("denied", command, None)).unwrap_err();
        assert!(failure.to_string().contains("I/O capability"));
        assert_eq!(std::fs::read(dir.path().join("audit.sarif")).unwrap(), source);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
