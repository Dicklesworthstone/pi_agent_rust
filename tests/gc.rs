//! Integration tests for retention-policy garbage collection (`pi gc`) (bd-cv653.7.11).

use std::fs;
use std::path::PathBuf;
use tempfile::tempdir;

use pi::gc::{
    check_storage_pressure, format_bytes, parse_retention_days, GarbageCollector, GcOptions,
    GcStoreKind, GC_LEDGER_SCHEMA,
};

#[test]
fn test_retention_parsing_and_formatting() {
    assert_eq!(parse_retention_days("30d"), Some(30));
    assert_eq!(parse_retention_days("7d"), Some(7));
    assert_eq!(parse_retention_days("24h"), Some(1));
    assert_eq!(parse_retention_days("48h"), Some(2));
    assert_eq!(parse_retention_days("14"), Some(14));
    assert_eq!(parse_retention_days("invalid"), None);

    assert_eq!(format_bytes(100), "100 B");
    assert_eq!(format_bytes(1024), "1.0 KB");
    assert_eq!(format_bytes(10 * 1024 * 1024), "10.00 MB");
}

#[test]
fn test_gc_plan_preserves_pinned_and_named_sessions() {
    let Ok(tmp) = tempdir() else { return };
    let sessions_dir = tmp.path().join("sessions");
    let _ = fs::create_dir_all(&sessions_dir);

    let s_named = sessions_dir.join("named_session.jsonl");
    let s_pinned = sessions_dir.join("pinned_session.jsonl");
    let s_normal = sessions_dir.join("normal_session.jsonl");

    let _ = fs::write(
        &s_named,
        "{\"version\":3,\"id\":\"named-1\",\"name\":\"my-feature\"}\n",
    );
    let _ = fs::write(&s_pinned, "{\"version\":3,\"id\":\"pinned-1\"}\n");
    let _ = fs::write(sessions_dir.join("pinned_session.pinned"), "pinned");
    let _ = fs::write(&s_normal, "{\"version\":3,\"id\":\"normal-1\"}\n");

    let options = GcOptions {
        older_than_days: 0,
        keep_last: 0, // Even with keep_last 0, named & pinned are protected
        prune_caches: false,
        dry_run: true,
        empty_trash: false,
        restore_target: None,
        custom_sessions_dir: Some(sessions_dir.clone()),
        custom_trash_dir: None,
        custom_ledger_path: None,
    };

    let plan = GarbageCollector::plan(&options).expect("plan");
    assert!(plan.items_protected.iter().any(|i| i.path == s_named));
    assert!(plan.items_protected.iter().any(|i| i.path == s_pinned));
    assert!(plan.items_to_prune.iter().any(|i| i.path == s_normal));
    assert_eq!(plan.items_to_prune.len(), 1);
}

#[test]
fn test_gc_dry_run_and_live_sweep_with_trash_and_ledger() {
    let Ok(tmp) = tempdir() else { return };
    let sessions_dir = tmp.path().join("sessions");
    let trash_dir = tmp.path().join("trash");
    let ledger_file = tmp.path().join("gc_ledger.jsonl");

    let _ = fs::create_dir_all(&sessions_dir);

    // Create 3 normal sessions
    let s1 = sessions_dir.join("session_1.jsonl");
    let s2 = sessions_dir.join("session_2.jsonl");
    let s3 = sessions_dir.join("session_3.jsonl");

    let _ = fs::write(&s1, "{\"version\":3,\"id\":\"s1\"}\n");
    let _ = fs::write(&s2, "{\"version\":3,\"id\":\"s2\"}\n");
    let _ = fs::write(&s3, "{\"version\":3,\"id\":\"s3\"}\n");

    // 1. Dry run with keep_last = 1
    let dry_options = GcOptions {
        older_than_days: 0,
        keep_last: 1,
        prune_caches: false,
        dry_run: true,
        empty_trash: false,
        restore_target: None,
        custom_sessions_dir: Some(sessions_dir.clone()),
        custom_trash_dir: Some(trash_dir.clone()),
        custom_ledger_path: Some(ledger_file.clone()),
    };

    let dry_res = GarbageCollector::run(&dry_options).expect("dry run");
    assert!(dry_res.dry_run);
    assert_eq!(dry_res.plan.items_to_prune.len(), 2);
    // Ensure dry run did not touch files
    assert!(s1.exists());
    assert!(s2.exists());
    assert!(s3.exists());

    // 2. Live execution
    let live_options = GcOptions {
        older_than_days: 0,
        keep_last: 1,
        prune_caches: false,
        dry_run: false,
        empty_trash: false,
        restore_target: None,
        custom_sessions_dir: Some(sessions_dir.clone()),
        custom_trash_dir: Some(trash_dir.clone()),
        custom_ledger_path: Some(ledger_file.clone()),
    };

    let live_res = GarbageCollector::run(&live_options).expect("live run");
    assert!(!live_res.dry_run);
    assert_eq!(live_res.pruned_items, 2);
    assert_eq!(live_res.trashed_items, 2);

    // Verify ledger was written
    assert!(ledger_file.exists());
    let ledger_content = fs::read_to_string(&ledger_file).expect("read ledger");
    assert!(ledger_content.contains(GC_LEDGER_SCHEMA));
    assert!(ledger_content.contains("trashed"));

    // 3. Restore session
    let restore_options = GcOptions {
        older_than_days: 0,
        keep_last: 1,
        prune_caches: false,
        dry_run: false,
        empty_trash: false,
        restore_target: Some("session_".to_string()),
        custom_sessions_dir: Some(sessions_dir.clone()),
        custom_trash_dir: Some(trash_dir.clone()),
        custom_ledger_path: Some(ledger_file.clone()),
    };

    let restore_res = GarbageCollector::run(&restore_options).expect("restore");
    assert_eq!(restore_res.errors.len(), 0);

    // 4. Empty trash
    let empty_options = GcOptions {
        older_than_days: 0,
        keep_last: 1,
        prune_caches: false,
        dry_run: false,
        empty_trash: true,
        restore_target: None,
        custom_sessions_dir: Some(sessions_dir.clone()),
        custom_trash_dir: Some(trash_dir.clone()),
        custom_ledger_path: Some(ledger_file.clone()),
    };

    let empty_res = GarbageCollector::run(&empty_options).expect("empty trash");
    assert_eq!(empty_res.errors.len(), 0);
}

#[test]
fn test_orphaned_sidecar_detection() {
    let Ok(tmp) = tempdir() else { return };
    let sessions_dir = tmp.path().join("sessions");
    let _ = fs::create_dir_all(&sessions_dir);

    // Live session with sidecar
    let s_live = sessions_dir.join("live.jsonl");
    let s_live_meta = sessions_dir.join("live.meta.json");
    let _ = fs::write(&s_live, "{\"version\":3}\n");
    let _ = fs::write(&s_live_meta, "{\"summary\":\"live session\"}\n");

    // Orphaned sidecar without live.jsonl
    let s_orphan_meta = sessions_dir.join("orphan.meta.json");
    let _ = fs::write(&s_orphan_meta, "{\"summary\":\"orphan session\"}\n");

    let options = GcOptions {
        older_than_days: 30,
        keep_last: 5,
        prune_caches: false,
        dry_run: true,
        empty_trash: false,
        restore_target: None,
        custom_sessions_dir: Some(sessions_dir),
        custom_trash_dir: None,
        custom_ledger_path: None,
    };

    let plan = GarbageCollector::plan(&options).expect("plan");
    assert!(plan
        .items_to_prune
        .iter()
        .any(|i| i.path == s_orphan_meta && i.store == GcStoreKind::Sidecars));
    assert!(!plan.items_to_prune.iter().any(|i| i.path == s_live_meta));
}

#[test]
fn test_storage_pressure_detection() {
    let Ok(tmp) = tempdir() else { return };
    let sessions_dir = tmp.path().join("sessions");
    let _ = fs::create_dir_all(&sessions_dir);

    // Initial check: empty store, clean
    let clean_status = check_storage_pressure(&sessions_dir, 30);
    assert!(!clean_status.is_elevated);
    assert!(clean_status.recommendation.is_none());

    // Create 60 aged sessions to trigger pressure threshold (> 50)
    for i in 0..60 {
        let s = sessions_dir.join(format!("sess_{i}.jsonl"));
        let _ = fs::write(&s, "{\"version\":3}\n");
    }

    let elevated_status = check_storage_pressure(&sessions_dir, 0);
    assert!(elevated_status.is_elevated);
    assert!(elevated_status.recommendation.is_some());
    assert!(elevated_status
        .recommendation
        .unwrap()
        .contains("Storage pressure detected"));
}
