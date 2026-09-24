//! Integration tests for verified in-place self-updater (`pi self-update`) (bd-cv653.7.10).

mod common;

use sha2::{Digest as _, Sha256};
use std::fmt::Write as _;
use std::fs;
use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::tempdir;

use pi::self_update::{
    ChecksumMap, PackageManager, PlatformInfo, SelfUpdateOptions, SelfUpdateStatus, SelfUpdater,
};
use pi::version_check::CURRENT_VERSION;

#[test]
fn test_checksum_verification_fail_closed() {
    let raw_sums = r"
# Valid release checksums
e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  empty.tar.gz
2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae *test_binary
";
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

/// A tiny HTTP/1.1 server that mimics GitHub's release downloads: every
/// `/download/<tag>/<asset>` answers `302 Found` and points somewhere else,
/// the way github.com hands out signed `release-assets.githubusercontent.com`
/// URLs. Serves until the test process exits and records each request path.
struct RedirectingReleaseServer {
    port: u16,
    requests: Arc<Mutex<Vec<String>>>,
}

type Route = Box<dyn Fn(&str, u16) -> (u16, Vec<(String, String)>, Vec<u8>) + Send + Sync>;

impl RedirectingReleaseServer {
    fn start(route: Route) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                serve_one(stream, &route, port, &seen);
            }
        });
        Self { port, requests }
    }

    fn base(&self) -> String {
        format!("http://127.0.0.1:{}/download", self.port)
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("requests lock").clone()
    }
}

fn serve_one(mut stream: TcpStream, route: &Route, port: u16, seen: &Mutex<Vec<String>>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let path = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("")
        .to_string();
    seen.lock().expect("requests lock").push(path.clone());
    let (status, headers, body) = route(&path, port);
    let reason = match status {
        200 => "OK",
        302 => "Found",
        _ => "Not Found",
    };
    let mut response = format!("HTTP/1.1 {status} {reason}\r\n");
    for (name, value) in headers {
        let _ = write!(response, "{name}: {value}\r\n");
    }
    let _ = write!(
        response,
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.write_all(&body);
    let _ = stream.flush();
}

fn found(location: String) -> (u16, Vec<(String, String)>, Vec<u8>) {
    (302, vec![("Location".to_string(), location)], Vec::new())
}

const fn ok(body: Vec<u8>) -> (u16, Vec<(String, String)>, Vec<u8>) {
    (200, Vec::new(), body)
}

const fn not_found() -> (u16, Vec<(String, String)>, Vec<u8>) {
    (404, Vec::new(), Vec::new())
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// The server for the redirect tests: tag `v9.9.9` carries a good binary,
/// `v8.8.8` a binary whose bytes do not match its `SHA256SUMS` line, and
/// `vloop` redirects `SHA256SUMS` to itself forever.
fn release_server(asset: &str, payload: &'static [u8]) -> RedirectingReleaseServer {
    let asset = asset.to_string();
    let good_sums = format!("{}  {asset}\n", sha256_hex(payload));
    let bad_sums = format!("{}  {asset}\n", sha256_hex(b"what the checksum promised"));
    let good_asset = format!("/download/v9.9.9/{asset}");
    let bad_asset = format!("/download/v8.8.8/{asset}");
    RedirectingReleaseServer::start(Box::new(move |path, port| {
        let objects = format!("http://127.0.0.1:{port}/objects");
        match path {
            // Absolute Location with a signed-URL style query string.
            "/download/v9.9.9/SHA256SUMS" => found(format!("{objects}/good-sums?sig=a%2Fb&se=1")),
            "/download/v8.8.8/SHA256SUMS" => found(format!("{objects}/bad-sums?sig=c")),
            "/objects/good-sums?sig=a%2Fb&se=1" => ok(good_sums.clone().into_bytes()),
            "/objects/bad-sums?sig=c" => ok(bad_sums.clone().into_bytes()),
            // Path-only Location, then a second hop.
            p if p == good_asset || p == bad_asset => found("/objects/hop".to_string()),
            "/objects/hop" => found(format!("{objects}/binary?sig=d")),
            "/objects/binary?sig=d" => ok(payload.to_vec()),
            "/download/vloop/SHA256SUMS" => found("/download/vloop/SHA256SUMS".to_string()),
            _ => not_found(),
        }
    }))
}

#[test]
fn self_update_follows_release_asset_redirects_and_still_verifies_checksums() {
    let Some(platform) = PlatformInfo::current() else {
        eprintln!("Skipping: unsupported platform for self-update");
        return;
    };
    let asset = platform
        .candidate_asset_names("9.9.9")
        .into_iter()
        .next()
        .expect("at least one candidate asset name");
    let payload: &'static [u8] = b"pretend this is pi 9.9.9";
    let server = release_server(&asset, payload);
    let base = server.base();

    let (name, bytes, bad) = common::run_async(async move {
        let updater = SelfUpdater::new();
        let sums = updater
            .fetch_checksums("9.9.9", Some(&base))
            .await
            .expect("SHA256SUMS behind a 302 must be fetched");
        let (name, bytes) = updater
            .download_and_verify(&platform, "9.9.9", &sums, Some(&base))
            .await
            .expect("binary behind two redirects must download and verify");

        let bad_sums = updater
            .fetch_checksums("8.8.8", Some(&base))
            .await
            .expect("mismatching SHA256SUMS is still fetched");
        let bad = updater
            .download_and_verify(&platform, "8.8.8", &bad_sums, Some(&base))
            .await
            .map(|(name, _)| name);
        (name, bytes, bad)
    });

    assert_eq!(name, asset);
    assert_eq!(bytes, payload);
    let err = bad.expect_err("a redirected binary that fails its checksum must be rejected");
    assert!(
        err.to_string().to_ascii_lowercase().contains("checksum")
            || err.to_string().to_ascii_lowercase().contains("mismatch"),
        "unexpected error: {err}"
    );

    let requests = server.requests();
    for expected in [
        "/download/v9.9.9/SHA256SUMS".to_string(),
        "/objects/good-sums?sig=a%2Fb&se=1".to_string(),
        format!("/download/v9.9.9/{asset}"),
        "/objects/hop".to_string(),
        "/objects/binary?sig=d".to_string(),
    ] {
        assert!(
            requests.contains(&expected),
            "missing request {expected}: {requests:?}"
        );
    }
}

#[test]
fn self_update_gives_up_on_a_redirect_loop() {
    let server = release_server("pi_unused", b"unused");
    let base = server.base();
    let err = common::run_async(async move {
        SelfUpdater::new()
            .fetch_checksums("loop", Some(&base))
            .await
            .expect_err("an endless redirect chain must fail")
    });
    assert!(
        err.to_string().contains("too many redirects"),
        "unexpected error: {err}"
    );
    let hops = server
        .requests()
        .iter()
        .filter(|path| path.as_str() == "/download/vloop/SHA256SUMS")
        .count();
    assert!(hops <= 6, "redirects must be bounded, saw {hops} requests");
}
