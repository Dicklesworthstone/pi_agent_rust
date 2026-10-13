use super::*;

fn decision() -> Disposition {
    Disposition { status: "accepted".into(), reason: "Reviewed test fixture".into(), at_ms: 1 }
}

#[test]
fn report_destinations_are_relative_files_not_arbitrary_source_paths() {
    assert_eq!(report_path("./reports/run.sarif").unwrap(), Path::new("reports/run.sarif"));
    for path in ["", ".", "/outside.sarif", "../outside.sarif", "a/../b.sarif",
        "C:outside.sarif", "a\\b.sarif", "a\n.sarif", "code.rs", "a.sarif/"]
    {
        assert!(report_path(path).is_err(), "{path:?}");
    }
}

#[test]
fn stored_decisions_reject_duplicate_keys_and_invalid_policy_without_echoing_reasons() {
    let entry = r#"{"status":"accepted","reason":"PRIVATE-CANARY","atMs":1}"#;
    let valid = format!("{{\"0123456789abcdef\":{entry}}}");
    assert_eq!(decode_store(valid.as_bytes()).unwrap().len(), 1);
    for text in [
        format!("{{\"0123456789abcdef\":{entry},\"0123456789abcdef\":{entry}}}"),
        valid.replacen("\"status\":", "\"status\":\"fixed\",\"status\":", 1),
        valid.replacen("accepted", "unknown", 1),
        valid.replacen("0123456789abcdef", "not-a-fingerprint", 1),
        format!("{valid} trailing PRIVATE-CANARY"),
    ] {
        let failure = decode_store(text.as_bytes()).unwrap_err().to_string();
        assert!(!failure.contains("PRIVATE-CANARY"));
    }
    let mut store = BTreeMap::from([("0123456789abcdef".into(), decision())]);
    store.get_mut("0123456789abcdef").unwrap().reason = "x".repeat(2048);
    assert!(validate_store(&store).is_ok());
    store.get_mut("0123456789abcdef").unwrap().reason.push('x');
    assert!(validate_store(&store).is_err());
}

#[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
mod unix {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    use super::super::super::{ScanInput, SecurityScanTool};

    fn input(value: serde_json::Value) -> ScanInput { serde_json::from_value(value).unwrap() }

    #[test]
    fn default_report_replacement_does_not_truncate_the_previous_inode() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path()).unwrap();
        storage.write_report(None, b"first complete report").unwrap();
        let path = dir.path().join(DEFAULT_REPORT);
        let previous = dir.path().join("previous.sarif");
        fs::hard_link(&path, &previous).unwrap();
        storage.write_report(None, b"second complete report").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second complete report");
        assert_eq!(fs::read(&previous).unwrap(), b"first complete report");
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::read_dir(dir.path().join(".pi")).unwrap().count(), 1);
    }

    #[test]
    fn explicit_exports_never_replace_existing_files_links_or_concurrent_destinations() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path()).unwrap();
        storage.write_report(Some("audit.sarif"), b"keep this report").unwrap();
        assert!(storage.write_report(Some("audit.sarif"), b"overwrite").is_err());
        assert_eq!(fs::read(dir.path().join("audit.sarif")).unwrap(), b"keep this report");
        fs::write(outside.path().join("marker"), b"keep outside").unwrap();
        symlink(outside.path().join("marker"), dir.path().join("linked.sarif")).unwrap();
        assert!(storage.write_report(Some("linked.sarif"), b"overwrite").is_err());
        assert_eq!(fs::read(outside.path().join("marker")).unwrap(), b"keep outside");
        symlink(outside.path(), dir.path().join("alias")).unwrap();
        assert!(storage.write_report(Some("alias/new.sarif"), b"escape").is_err());
        assert!(!outside.path().join("new.sarif").exists());
        // Another creator winning between setup and publication is not an
        // overwrite authorization. The final link, not preflight, decides.
        fs::write(dir.path().join("race.sarif"), b"other writer").unwrap();
        assert!(storage.write_report(Some("race.sarif"), b"overwrite").is_err());
        assert_eq!(fs::read(dir.path().join("race.sarif")).unwrap(), b"other writer");
    }

    #[test]
    fn report_and_store_reads_refuse_symlinks_and_special_files() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("private.sarif"), b"PRIVATE-CANARY").unwrap();
        symlink(outside.path().join("private.sarif"), dir.path().join("baseline.sarif")).unwrap();
        let storage = Storage::open(dir.path()).unwrap();
        let failure = storage.read_report("baseline.sarif").unwrap_err().to_string();
        assert!(!failure.contains("PRIVATE-CANARY"));
        fs::create_dir(dir.path().join("directory.sarif")).unwrap();
        assert!(storage.read_report("directory.sarif").is_err());
        symlink(outside.path(), dir.path().join(".pi")).unwrap();
        assert!(storage.load().is_err());
        assert!(storage.update("0123456789abcdef".into(), decision()).is_err());
        assert!(!outside.path().join("security-dispositions.json").exists());
        assert!(!outside.path().join("security-dispositions.lock").exists());
    }

    #[test]
    fn pinned_workspace_survives_ancestor_renames_without_writing_a_replacement_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let storage = Storage::open(&workspace).unwrap();
        fs::rename(&workspace, dir.path().join("original")).unwrap();
        fs::create_dir(&workspace).unwrap();
        storage.write_report(Some("audit.sarif"), b"original workspace").unwrap();
        assert_eq!(fs::read(dir.path().join("original/audit.sarif")).unwrap(), b"original workspace");
        assert!(!workspace.join("audit.sarif").exists());
    }

    #[test]
    fn competing_dispositions_do_not_lose_successful_updates_and_corrupt_stores_are_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path()).unwrap();
        storage.update("0123456789abcdef".into(), decision()).unwrap();
        let (_, lock) = storage.0.lock_store().unwrap();
        assert!(storage.update("fedcba9876543210".into(), decision()).is_err());
        assert_eq!(storage.load().unwrap().len(), 1);
        drop(lock);
        assert_eq!(storage.update("fedcba9876543210".into(), decision()).unwrap(), 2);
        let path = dir.path().join(DISPOSITIONS);
        fs::write(&path, b"PRIVATE-CANARY invalid JSON").unwrap();
        assert!(storage.update("aaaaaaaaaaaaaaaa".into(), decision()).is_err());
        assert!(storage.save(&BTreeMap::new()).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"PRIVATE-CANARY invalid JSON");
    }

    #[test]
    fn disposition_transaction_keeps_the_locked_directory_identity() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path()).unwrap();
        let (locked, _lock) = storage.0.lock_store().unwrap();
        fs::rename(dir.path().join(".pi"), dir.path().join("saved-pi")).unwrap();
        fs::create_dir(dir.path().join(".pi")).unwrap();
        let map = BTreeMap::from([("0123456789abcdef".into(), decision())]);
        Storage::write_store(&locked, &map).unwrap();
        assert!(dir.path().join("saved-pi/security-dispositions.json").is_file());
        assert!(!dir.path().join(DISPOSITIONS).exists());
    }

    #[test]
    fn source_tool_uses_create_only_exports_and_atomic_default_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("code.py"), "pickle.loads(data)\n").unwrap();
        let tool = SecurityScanTool::new(dir.path());
        let request = input(serde_json::json!({"op":"run", "paths":["code.py"], "sarifOut":"saved.sarif"}));
        tool.op_run(&request).unwrap();
        let saved = fs::read(dir.path().join("saved.sarif")).unwrap();
        fs::write(dir.path().join("code.py"), "pass\n").unwrap();
        assert!(tool.op_run(&request).is_err());
        assert_eq!(fs::read(dir.path().join("saved.sarif")).unwrap(), saved);
        let default = input(serde_json::json!({"op":"run", "paths":["code.py"]}));
        tool.op_run(&default).unwrap();
        fs::write(dir.path().join("code.py"), "pickle.loads(data)\n").unwrap();
        tool.op_run(&default).unwrap();
        let report = fs::read_to_string(dir.path().join(DEFAULT_REPORT)).unwrap();
        assert_eq!(super::super::super::super::findings_from_sarif(&report).unwrap().len(), 1);
    }

    #[test]
    fn denied_owner_never_creates_persistence_files() {
        let dir = tempfile::tempdir().unwrap();
        let owner = asupersync::Cx::for_request().restrict::<asupersync::cx::cap::None>();
        let _current = owner.set_current_restricted();
        assert!(Storage::open(dir.path()).is_err());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
