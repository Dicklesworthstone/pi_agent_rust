//! Real audit exporter -> confined files -> public comparison operation.
use super::*;
use crate::security_scan::SecurityScanTool;
use crate::tools::Tool as _;
use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

fn runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::new()
        .enable_parking(false)
        .worker_threads(2)
        .blocking_threads(1, 8)
        .build()
        .unwrap()
}

fn receive(socket: &mut TcpStream) -> (String, Value) {
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut bytes = Vec::new();
    loop {
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let end = end + 4;
            let headers = String::from_utf8(bytes[..end].to_vec()).unwrap();
            let length = headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .map_or(0, |(_, value)| value.trim().parse::<usize>().unwrap());
            assert!(length <= 8192);
            if bytes.len() >= end + length {
                let body = if length == 0 {
                    Value::Null
                } else {
                    serde_json::from_slice(&bytes[end..end + length]).unwrap()
                };
                return (headers, body);
            }
        }
        assert!(Instant::now() < deadline && bytes.len() < 16384);
        let mut chunk = [0; 2048];
        let count = socket.read(&mut chunk).expect("bounded peer read");
        assert!(count > 0, "unexpected request EOF");
        bytes.extend_from_slice(&chunk[..count]);
    }
}

fn peer(responses: Vec<(u16, Value)>) -> (String, std::thread::JoinHandle<Vec<(String, Value)>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let thread = std::thread::spawn(move || {
        let mut captured = Vec::new();
        for (status, body) in responses {
            let deadline = Instant::now() + Duration::from_secs(20);
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "audit never connected");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("peer accept: {error}"),
                }
            };
            captured.push(receive(&mut socket));
            let bytes = serde_json::to_vec(&body).unwrap();
            write!(socket, "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len()).unwrap();
            socket.write_all(&bytes).unwrap();
        }
        captured
    });
    (base, thread)
}

#[test]
fn public_comparison_consumes_real_audit_exports_without_requerying_osv() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.lock"),
        concat!(
            "version = 4\n[[package]]\nname='public-crate'\nversion='1.0.0'\n",
            "source='registry+https://github.com/rust-lang/crates.io-index'\n",
            "[[package]]\nname='private-workspace'\nversion='1.0.0'\n"
        ),
    )
    .unwrap();
    let (base, server) = peer(vec![
        (
            200,
            json!({"results":[{"vulns":[{"id":"GHSA-example","modified":"2026-01-01T00:00:00Z"}]}]}),
        ),
        (
            200,
            json!({"id":"GHSA-example","summary":"Offline protocol fixture"}),
        ),
        (200, json!({"results":[{}]})),
        (503, json!({"error":"PRIVATE-CANARY"})),
    ]);
    let tool = SecurityScanTool::new(dir.path())
        .with_osv_base_url(&base)
        .unwrap();
    let runtime = runtime();
    for (name, failed) in [
        ("before.sarif", false),
        ("after.sarif", false),
        ("outage.sarif", true),
    ] {
        let output = runtime
            .block_on(tool.execute(
                "audit",
                json!({
                    "op":"audit_dependencies","sarifOut":name
                }),
                None,
            ))
            .unwrap();
        assert_eq!(output.is_error, failed);
    }
    let captured = server.join().unwrap(); // No listener remains for comparison.
    assert_eq!(captured.len(), 4);
    assert!(captured[0].0.starts_with("POST /v1/querybatch "));
    assert!(captured[1].0.starts_with("GET /v1/vulns/GHSA-example "));
    for (_, body) in &captured {
        assert!(!body.to_string().contains("private-workspace"));
        assert!(!body.to_string().contains("Cargo.lock"));
    }
    let unchanged = std::fs::read(dir.path().join("before.sarif")).unwrap();
    let output = runtime.block_on(tool.execute("compare", json!({
        "op":"compare_dependencies","baseline":"before.sarif","current":"after.sarif","limit":1
    }), None)).unwrap();
    assert!(!output.is_error);
    let details = output.details.unwrap();
    assert_eq!(details["counts"]["no_longer_observed"], 1);
    assert_eq!(details["baseline"]["excludedEntries"], 1);
    assert_eq!(details["baseline"]["sha256"], snapshot::digest(&unchanged));
    let outage = runtime
        .block_on(tool.execute(
            "compare",
            json!({
                "op":"compare_dependencies","baseline":"before.sarif","current":"outage.sarif"
            }),
            None,
        ))
        .unwrap();
    assert!(outage.is_error);
    assert_eq!(outage.details.unwrap()["counts"]["unknown"], 1);
    assert_eq!(
        std::fs::read(dir.path().join("before.sarif")).unwrap(),
        unchanged
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 4);

    // The server is still gone: triage is a report transformation, not a
    // fresh lookup, and comparison must keep the dispositioned match visible.
    let review = runtime
        .block_on(tool.execute(
            "triage",
            json!({
                "op":"disposition_dependencies","current":"before.sarif",
                "expectedSha256":details["baseline"]["sha256"],
                "fingerprint":details["findings"][0]["fingerprint"],
                "decision":"false_positive","reason":"Requires separate applicability analysis",
                "sarifOut":"reviewed.sarif"
            }),
            None,
        ))
        .unwrap();
    assert!(!review.is_error);
    assert_eq!(review.details.unwrap()["advisoryMatches"], 1);
    let reviewed = runtime
        .block_on(tool.execute(
            "compare-triage",
            json!({
                "op":"compare_dependencies","baseline":"before.sarif","current":"reviewed.sarif"
            }),
            None,
        ))
        .unwrap()
        .details
        .unwrap();
    assert_eq!(reviewed["counts"]["persistent"], 1);
    assert_eq!(reviewed["findings"][0]["currentDecision"], "false_positive");
    assert_eq!(reviewed["findings"][0]["baselineDecision"], Value::Null);
    assert_eq!(
        std::fs::read(dir.path().join("before.sarif")).unwrap(),
        unchanged
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 5);
}

#[test]
fn public_review_validates_arguments_and_io_authority_before_reading() {
    let dir = tempfile::tempdir().unwrap();
    let tool = SecurityScanTool::new(dir.path());
    for extra in [
        json!({"limit":0}),
        json!({"limit":51}),
        json!({"timeoutMs":1}),
    ] {
        let mut input = json!({"op":"compare_dependencies","baseline":"missing.sarif","current":"missing.sarif"});
        input
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let failure =
            futures::executor::block_on(tool.execute("invalid", input, None)).unwrap_err();
        assert!(!failure.to_string().contains("cannot open"));
    }
    let restricted = asupersync::Cx::for_request().restrict::<asupersync::cx::cap::None>();
    let _guard = restricted.set_current_restricted();
    let failure = futures::executor::block_on(tool.execute(
        "denied",
        json!({
            "op":"compare_dependencies","baseline":"missing.sarif","current":"missing.sarif"
        }),
        None,
    ))
    .unwrap_err();
    assert!(failure.to_string().contains("I/O capability"));
}
