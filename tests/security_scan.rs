//! Integration tests for `security_scan` (bd-cv653.2.6): a seeded-vulnerability
//! fixture repo scanned end-to-end through the tool surface — plan, run to
//! SARIF, disposition suppression, and compare deltas — plus the self-scan
//! zero-hit guard over this repo's src/ (catches self-leaks of secrets).
#![allow(clippy::missing_panics_doc)]

use pi::security_scan::SecurityScanTool;
use pi::tools::Tool;

fn block_on_local<F: std::future::Future>(future: F) -> F::Output {
    // ubs:ignore-start — runtime construction is infallible in tests
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .expect("failed to build test runtime");
    // ubs:ignore-end
    runtime.block_on(future)
}

fn fixture_repo(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("pi-secscan-it-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir fixture");
    std::fs::write(
        dir.join("app.py"),
        concat!(
            "AWS_KEY = \"AKIA",
            "IOSFODNN7EXAMPLE\"\n",
            "import pickle\n",
            "pickle.loads(blob)\n",
            "query = execute(format!(\"select * from t where id={}\", uid))\n",
        ),
    )
    .expect("seed vulns");
    dir
}

fn run_tool(tool: &SecurityScanTool, input: serde_json::Value) -> (String, bool) {
    match block_on_local(tool.execute("t", input, None)) {
        Ok(out) => {
            let text = out
                .content
                .iter()
                .filter_map(|block| match block {
                    pi::model::ContentBlock::Text(t) => Some(t.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            (text, out.is_error)
        }
        // Validation failures surface as Err — the named-refusal contract.
        Err(err) => (err.to_string(), true),
    }
}

#[test]
fn plan_run_disposition_compare_flow() {
    let dir = fixture_repo("flow");
    let tool = SecurityScanTool::new(&dir);

    // plan: scope + pack checklist.
    let (text, is_error) = run_tool(&tool, serde_json::json!({ "op": "plan" }));
    assert!(!is_error, "plan failed: {text}");
    assert!(text.contains("pi-default"), "plan missing pack: {text}");

    // run: SARIF written, seeded findings present.
    let (text, is_error) = run_tool(&tool, serde_json::json!({ "op": "run" }));
    assert!(!is_error, "run failed: {text}");
    assert!(
        text.contains("secret.aws-access-key"),
        "missing aws hit: {text}"
    );
    let sarif_path = dir.join(".pi/security-scan.sarif");
    let sarif = std::fs::read_to_string(&sarif_path).expect("sarif written");
    let doc: serde_json::Value = serde_json::from_str(&sarif).expect("sarif parses");
    assert_eq!(doc["version"].as_str(), Some("2.1.0"));
    let results = doc
        .pointer("/runs/0/results")
        .expect("results")
        .as_array()
        .expect("array")
        .clone();
    assert!(
        results.len() >= 2,
        "expected >=2 results, got {}",
        results.len()
    );
    let aws_fp = results
        .iter()
        .find(|r| r["ruleId"].as_str() == Some("secret.aws-access-key"))
        .and_then(|r| r.pointer("/fingerprints/pi~1v1"))
        .and_then(|v| v.as_str())
        .expect("aws finding fingerprint")
        .to_string();

    // disposition: false-positive suppresses on re-run.
    let (text, is_error) = run_tool(
        &tool,
        serde_json::json!({
            "op": "disposition",
            "fingerprint": aws_fp,
            "status": "false-positive",
            "reason": "fixture-only credential"
        }),
    );
    assert!(!is_error, "disposition failed: {text}");

    let (text, is_error) = run_tool(&tool, serde_json::json!({ "op": "run" }));
    assert!(!is_error, "second run failed: {text}");
    assert!(
        !text.contains("secret.aws-access-key"),
        "dispositioned finding not suppressed: {text}"
    );
    assert!(
        text.contains("1 suppressed"),
        "suppression count missing: {text}"
    );

    // compare vs the FIRST sarif is impossible (overwritten) — compare the
    // dispositioned baseline against a fresh scan after deleting the store:
    std::fs::remove_file(dir.join(".pi/security-dispositions.json")).expect("drop store");
    let (text, is_error) = run_tool(&tool, serde_json::json!({ "op": "compare" }));
    assert!(!is_error, "compare failed: {text}");
    assert!(
        text.contains("regressed") || text.contains("new"),
        "compare delta missing: {text}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn disposition_validates_status_and_reason() {
    let dir = fixture_repo("validate");
    let tool = SecurityScanTool::new(&dir);
    let (_, is_error) = run_tool(
        &tool,
        serde_json::json!({
            "op": "disposition",
            "fingerprint": "abc",
            "status": "whatever",
            "reason": "x"
        }),
    );
    assert!(is_error, "invalid status accepted");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn self_scan_src_has_zero_secret_hits() {
    // Guard against self-leaks: this repo's src/ must never trip the
    // secrets rules. (Rule-pattern strings and fragmented fixture text in
    // test modules do not match the literal-value patterns.)
    let repo = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let findings = pi::security_scan::run_scan(&repo, &["src".to_string()]).expect("scan src");
    let leaks: Vec<_> = findings
        .iter()
        .filter(|f| f.rule_id.starts_with("secret."))
        .collect();
    assert!(
        leaks.is_empty(),
        "self-scan secret leaks: {:?}",
        leaks
            .iter()
            .map(|f| format!("{}:{}:{}", f.path, f.line, f.rule_id))
            .collect::<Vec<_>>()
    );
}
