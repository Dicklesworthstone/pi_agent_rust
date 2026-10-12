use super::*;

pub(super) fn fixture(ecosystem: &str, versions: &[(&str, &[&str], bool)]) -> Value {
    let path = if ecosystem == "npm" { "package-lock.json" } else { "Cargo.lock" };
    let format = if ecosystem == "npm" { "npm" } else { "cargo" };
    let complete = !versions.is_empty() && versions.iter().all(|(_, _, complete)| *complete);
    let packages: Vec<_> = versions.iter().enumerate().map(|(index, (version, _, _))| {
        let mut location = json!({"lockfile":path});
        if ecosystem == "npm" {
            location["packagePath"] = json!(format!("node_modules/parent-{index}/node_modules/example"));
        }
        json!({"ecosystem":ecosystem,"name":"example","version":version,"locations":[location]})
    }).collect();
    let results: Vec<_> = versions.iter().enumerate().flat_map(|(index, (version, ids, _))| {
        ids.iter().map(move |id| {
            let fingerprint = snapshot::digest(format!("pi/dependency/v1\0{ecosystem}\0example\0{version}\0{id}\0").as_bytes());
            json!({"ruleId":format!("osv/{id}"),"fingerprints":{"pi/dependency/v1":fingerprint},
                "properties":{"dependency":{"packageIndex":index,"ecosystem":ecosystem,"name":"example",
                    "version":version,"advisoryId":id,"modified":"2026-01-01T00:00:00Z",
                    "fingerprint":fingerprint,"withdrawn":if complete {json!(false)} else {Value::Null},
                    "metadataAvailable":complete}}})
        })
    }).collect();
    json!({"version":"2.1.0","runs":[{
        "tool":{"driver":{"name":"pi dependency audit"}},
        "invocations":[{"executionSuccessful":complete}],"results":results,
        "properties":{"schema":"pi.security-dependency-audit/v1","observedAt":"2026-01-02T00:00:00Z",
            "status":if versions.is_empty(){"not_checked"}else if complete{"complete"}else{"incomplete"},
            "queryComplete":complete,"metadataComplete":complete,
            "completedQueries":versions.iter().filter(|(_,_,done)|*done).count(),
            "queryCompletion":versions.iter().map(|(_,_,done)|done).collect::<Vec<_>>(),
            "inventory":{"schema":crate::security_scan::dependencies::INVENTORY_SCHEMA,
                "scope":"exact public-registry versions in the selected lockfiles; no installed-code or reachability claim",
                "lockfiles":[{"path":path,"format":format,"sha256":"a".repeat(64)}],
                "packages":packages,"excluded":[]}}
    }]})
}

fn parsed(value: &Value) -> Snapshot {
    Snapshot::parse(&serde_json::to_vec(value).unwrap()).unwrap()
}

fn comparison(before: &Value, after: &Value) -> Value {
    compare_snapshots(&parsed(before), &parsed(after), 0, 50).unwrap().details.unwrap()
}

#[test]
fn cargo_and_npm_comparison_distinguishes_observations_from_package_version_changes() {
    for ecosystem in ["crates.io", "npm"] {
        let before = fixture(ecosystem, &[("1.0.0", &["GHSA-old", "GHSA-shared"], true)]);
        let after = fixture(ecosystem, &[("1.0.0", &["GHSA-new", "GHSA-shared"], true)]);
        let result = comparison(&before, &after);
        assert_eq!(result["counts"]["persistent"], 1);
        assert_eq!(result["counts"]["newly_observed"], 1);
        assert_eq!(result["counts"]["no_longer_observed"], 1);
        assert_eq!(result["counts"]["unknown"], 0);
        let upgrade = fixture(ecosystem, &[("2.0.0", &["GHSA-shared"], true)]);
        let changed = comparison(&before, &upgrade);
        assert_eq!(changed["counts"]["new_package"], 1);
        assert_eq!(changed["counts"]["package_not_in_current_inventory"], 2);
        assert_eq!(changed["counts"]["no_longer_observed"], 0);
        assert_eq!(changed["counts"]["persistent"], 0);
    }
}

#[test]
fn unfinished_queries_never_establish_absence_on_either_side() {
    let hit = fixture("npm", &[("1.0.0", &["GHSA-match"], true)]);
    let unfinished = fixture("npm", &[("1.0.0", &[], false)]);
    for (before, after, reason) in [
        (&hit, &unfinished, "current_query_incomplete"),
        (&unfinished, &hit, "baseline_query_incomplete"),
    ] {
        let output = compare_snapshots(&parsed(before), &parsed(after), 0, 20).unwrap();
        assert!(output.is_error);
        let result = output.details.unwrap();
        assert_eq!(result["counts"]["unknown"], 1);
        assert_eq!(result["findings"][0]["reason"], reason);
        assert_eq!(result["counts"]["no_longer_observed"], 0);
        assert_eq!(result["counts"]["newly_observed"], 0);
    }
    // A failed OTHER package does not erase a completed package's evidence.
    let partial = fixture("npm", &[("1.0.0", &[], true), ("2.0.0", &[], false)]);
    assert_eq!(comparison(&hit, &partial)["counts"]["no_longer_observed"], 1);
}

#[test]
fn withdrawn_and_suppressed_matches_stay_visible_and_metadata_changes_are_separate() {
    let before = fixture("crates.io", &[("1.0.0", &["GHSA-match"], true)]);
    let mut after = before.clone();
    after["runs"][0]["results"][0]["properties"]["dependency"]["withdrawn"] = json!(true);
    after["runs"][0]["results"][0]["properties"]["dependency"]["modified"] = json!("2026-01-02T00:00:00Z");
    after["runs"][0]["results"][0]["suppressions"] = json!([{"kind":"external","status":"accepted","justification":"fixture"}]);
    let result = comparison(&before, &after);
    assert_eq!(result["counts"]["persistent"], 1);
    assert_eq!(result["findings"][0]["advisoryChanged"], true);
    assert_eq!(result["findings"][0]["current"]["withdrawn"], true);
    // Metadata loss is not withdrawal, resolution or a missing query.
    after["runs"][0]["properties"]["metadataComplete"] = json!(false);
    after["runs"][0]["properties"]["status"] = json!("complete_with_metadata_gaps");
    after["runs"][0]["results"][0]["properties"]["dependency"]["withdrawn"] = Value::Null;
    after["runs"][0]["results"][0]["properties"]["dependency"]["metadataAvailable"] = json!(false);
    assert_eq!(comparison(&before, &after)["counts"]["persistent"], 1);
}

#[test]
fn different_lockfile_scopes_and_reverse_time_are_refused() {
    let before = fixture("crates.io", &[("1.0.0", &[], true)]);
    let mut after = before.clone();
    after["runs"][0]["properties"]["inventory"]["lockfiles"][0]["path"] = json!("nested/Cargo.lock");
    after["runs"][0]["properties"]["inventory"]["packages"][0]["locations"][0]["lockfile"] = json!("nested/Cargo.lock");
    assert!(parsed(&before).check_pair(&parsed(&after)).is_err());
    after = before.clone();
    after["runs"][0]["properties"]["observedAt"] = json!("2025-01-02T00:00:00Z");
    assert!(parsed(&before).check_pair(&parsed(&after)).is_err());
}

#[test]
fn report_identity_completion_and_duplicate_tampering_fail_without_payload_echoes() {
    let original = fixture("crates.io", &[("1.0.0", &["GHSA-match"], true)]);
    for pointer in [
        "/version", "/runs/0/tool/driver/name", "/runs/0/properties/schema",
        "/runs/0/properties/queryComplete", "/runs/0/properties/queryCompletion",
        "/runs/0/properties/completedQueries", "/runs/0/invocations/0/executionSuccessful",
        "/runs/0/properties/inventory/packages/0/version",
        "/runs/0/results/0/properties/dependency/packageIndex",
        "/runs/0/results/0/properties/dependency/advisoryId",
        "/runs/0/results/0/fingerprints/pi~1dependency~1v1",
    ] {
        let mut broken = original.clone();
        *broken.pointer_mut(pointer).unwrap() = json!("PRIVATE-CANARY");
        let failure = Snapshot::parse(&serde_json::to_vec(&broken).unwrap()).err().expect("refused");
        assert!(!failure.to_string().contains("PRIVATE-CANARY"), "{pointer}");
    }
    let mut duplicate = original.clone();
    let finding = duplicate["runs"][0]["results"][0].clone();
    duplicate["runs"][0]["results"].as_array_mut().unwrap().push(finding);
    assert!(Snapshot::parse(&serde_json::to_vec(&duplicate).unwrap()).is_err());
    let raw = serde_json::to_string(&original).unwrap();
    let duplicate = raw.replacen("\"packageIndex\":0", "\"packageIndex\":1,\"packageIndex\":0", 1);
    assert_ne!(duplicate, raw);
    assert!(Snapshot::parse(duplicate.as_bytes()).is_err());
}

#[test]
fn locations_and_exclusions_change_without_churning_fingerprints() {
    let before = fixture("npm", &[("1.0.0", &["GHSA-match"], true)]);
    let mut after = before.clone();
    after["runs"][0]["properties"]["inventory"]["packages"][0]["locations"][0]["packagePath"] = json!("node_modules/example");
    after["runs"][0]["properties"]["inventory"]["excluded"] = json!([{
        "location":{"lockfile":"package-lock.json","packagePath":"node_modules/private"},"reason":"local_link"
    }]);
    after["runs"][0]["properties"]["inventory"]["lockfiles"][0]["sha256"] = json!("b".repeat(64));
    let result = comparison(&before, &after);
    assert_eq!(result["counts"]["persistent"], 1);
    assert_eq!(result["exclusionsChanged"], true);
    assert_eq!(result["lockfilesChanged"], true);
    assert_eq!(result["findings"][0]["locationsChanged"], true);
}

#[test]
fn pages_cover_the_deterministic_full_union_and_bind_both_source_files() {
    let before = fixture("crates.io", &[("1.0.0", &["GHSA-c", "GHSA-a"], true)]);
    let after = fixture("crates.io", &[("1.0.0", &["GHSA-b"], true)]);
    let a = parsed(&before);
    let b = parsed(&after);
    let mut ids = Vec::new();
    for offset in 0..3 {
        let output = compare_snapshots(&a, &b, offset, 1).unwrap().details.unwrap();
        assert_eq!(output["total"], 3);
        assert_eq!(output["counts"]["no_longer_observed"], 2);
        assert_eq!(output["baseline"]["sha256"], a.digest);
        assert_eq!(output["current"]["sha256"], b.digest);
        assert_eq!(output["nextOffset"], if offset == 2 { Value::Null } else { json!(offset+1) });
        ids.push(output["findings"][0]["advisoryId"].as_str().unwrap().to_string());
    }
    assert_eq!(ids, ["GHSA-a", "GHSA-b", "GHSA-c"]);
    let end = compare_snapshots(&a, &b, usize::MAX, 50).unwrap().details.unwrap();
    assert_eq!(end["findings"], json!([]));
    assert_eq!(end["nextOffset"], Value::Null);
}

#[test]
fn not_checked_does_not_claim_a_clean_dependency_audit() {
    let empty = parsed(&fixture("crates.io", &[]));
    let output = compare_snapshots(&empty, &empty, 0, 20).unwrap();
    assert!(output.is_error);
    assert_eq!(output.details.unwrap()["queryComplete"], false);
}

#[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
#[test]
fn confined_reader_rejects_symlinks_directories_oversize_and_escape_paths() {
    let workspace = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let bytes = serde_json::to_vec(&fixture("crates.io", &[("1.0.0", &[], true)])).unwrap();
    std::fs::write(workspace.path().join("audit.sarif"), &bytes).unwrap();
    std::fs::write(other.path().join("private"), "PRIVATE-CANARY").unwrap();
    std::os::unix::fs::symlink(other.path().join("private"), workspace.path().join("linked.sarif")).unwrap();
    std::fs::create_dir(workspace.path().join("directory.sarif")).unwrap();
    std::fs::File::create(workspace.path().join("large.sarif")).unwrap().set_len(MAX_REPORT_BYTES as u64 + 1).unwrap();
    let root = Root::open(workspace.path()).unwrap();
    assert_eq!(root.read("audit.sarif").unwrap(), bytes);
    for name in ["linked.sarif", "directory.sarif", "large.sarif", "../audit.sarif", "/audit.sarif", "dir/audit.sarif"] {
        assert!(root.read(name).is_err());
    }
    assert_eq!(std::fs::read_to_string(other.path().join("private")).unwrap(), "PRIVATE-CANARY");
}

#[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
mod wire;
