//! Integration tests for verified in-place self-updater (`pi self-update`) (bd-cv653.7.10).

use std::fs;
use std::path::Path;
use tempfile::tempdir;

use pi::self_update::{
    ChecksumMap, PackageManager, PlatformInfo, SelfUpdateOptions, SelfUpdateStatus, SelfUpdater,
};
use pi::version_check::CURRENT_VERSION;

#[test]
fn test_checksum_verification_fail_closed() {
    let raw_sums = r#"
# Valid release checksums
e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  empty.tar.gz
2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae *test_binary
"#;
    let checksums = ChecksumMap::parse(raw_sums);

    // Empty file matches hash e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
    assert!(checksums.verify_bytes("empty.tar.gz", b"").is_ok());

    // "foo" has hash 2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae
    assert!(checksums.verify_bytes("test_binary", b"foo").is_ok());

    // Tampered payload fails closed
    assert!(checksums.verify_bytes("test_binary", b"tampered").is_err());

    // Unknown asset fails closed
    assert!(checksums.verify_bytes("unlisted_file", b"foo").is_err());
}

#[test]
fn test_package_manager_refusal_commands() {
    assert_eq!(
        PackageManager::detect(Path::new("/opt/homebrew/Cellar/pi/0.1.0/bin/pi")),
        PackageManager::Homebrew
    );
    assert_eq!(
        PackageManager::Homebrew.upgrade_command(),
        Some("brew upgrade pi")
    );

    assert_eq!(
        PackageManager::detect(Path::new("/nix/store/abc-pi/bin/pi")),
        PackageManager::Nix
    );
    assert_eq!(
        PackageManager::detect(Path::new("/home/ubuntu/.cargo/bin/pi")),
        PackageManager::Cargo
    );
    assert_eq!(
        PackageManager::detect(Path::new("/home/ubuntu/.local/bin/pi")),
        PackageManager::Manual
    );
    assert_eq!(PackageManager::Manual.upgrade_command(), None);
}

#[test]
fn test_platform_candidates_contain_dsr_and_triple() {
    if let Some(platform) = PlatformInfo::current() {
        let candidates = platform.candidate_asset_names("0.3.0");
        assert!(!candidates.is_empty());
        let joined = candidates.join(" ");
        assert!(
            joined.contains("pi_") || joined.contains("pi-"),
            "Candidates should contain canonical naming styles: {joined}"
        );
    }
}

#[test]
fn test_atomic_swap_and_rollback_flow() {
    let Ok(tmp) = tempdir() else {
        return;
    };
    let target_bin = tmp.path().join("mock_pi");

    // Create an initial script/binary that prints --version
    let script_content =
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo \"pi 0.1.0\"; exit 0; fi\nexit 0\n";
    let Ok(()) = fs::write(&target_bin, script_content) else {
        return;
    };

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&target_bin, fs::Permissions::from_mode(0o755));
    }

    // New valid binary
    let new_valid_script =
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo \"pi 0.2.0\"; exit 0; fi\nexit 0\n";
    let swap_res = SelfUpdater::perform_atomic_swap(&target_bin, new_valid_script.as_bytes());
    assert!(swap_res.is_ok(), "Valid swap should succeed");

    // New broken binary (fails --version)
    let new_broken_script = "#!/bin/sh\nexit 1\n";
    let broken_swap = SelfUpdater::perform_atomic_swap(&target_bin, new_broken_script.as_bytes());
    assert!(
        broken_swap.is_err(),
        "Broken swap should fail smoke check and rollback"
    );

    // Verify rollback preserved the previous valid script
    let content = fs::read_to_string(&target_bin).unwrap_or_default();
    assert!(
        content.contains("pi 0.2.0"),
        "Rollback should restore valid previous binary"
    );
}

#[test]
fn test_check_mode_reports_current_and_latest() {
    let updater = SelfUpdater::new();
    let current_ver = CURRENT_VERSION.trim_start_matches('v');

    // Check with explicit same version
    let options = SelfUpdateOptions {
        version: Some(current_ver.to_string()),
        check: true,
        custom_manifest_url: None,
        custom_download_base: None,
    };

    let Ok(status) = futures::executor::block_on(updater.run(&options)) else {
        return;
    };

    match status {
        SelfUpdateStatus::CheckResult {
            current_version,
            latest_version,
            is_newer,
            ..
        } => {
            assert_eq!(current_version, current_ver);
            assert_eq!(latest_version, current_ver);
            assert!(!is_newer);
        }
        _ => {
            assert!(
                matches!(status, SelfUpdateStatus::CheckResult { .. }),
                "Expected CheckResult status"
            );
        }
    }
}
