//! Exercise real filesystem discovery and the public tool over framed stdio.

use super::*;
use crate::config::{Config, LspServerSettings, LspSettings};
use crate::tools::Tool as _;
use std::collections::HashMap;
use std::process::{Command, Stdio};

fn write(root: &Path, name: &str, text: &str) {
    let path = root.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn scan(root: &Path, pattern: &str, limit: usize) -> Discovery {
    discover(
        &root.canonicalize().unwrap(),
        &matcher(pattern).unwrap(),
        limit,
        &AgentCx::for_current_or_request(),
        Instant::now(),
        Duration::from_secs(10),
    ).unwrap()
}

#[test]
fn discovers_cold_files_and_keeps_the_lexically_first_bounded_page() {
    let temp = tempfile::tempdir().unwrap();
    for name in ["z.scan", "nested/c.scan", "b.scan", "a.scan"] {
        write(temp.path(), name, "clean");
    }
    write(temp.path(), "other.txt", "irrelevant");
    let found = scan(temp.path(), "**/*.scan", 2);
    assert_eq!(found.files.into_iter().collect::<Vec<_>>(), vec!["a.scan", "b.scan"]);
    assert_eq!(found.matched, 4);
    assert_eq!(found.stop, Some("file_limit"));
}

#[test]
fn honors_workspace_ignore_rules_and_hidden_paths_without_a_git_repository() {
    let temp = tempfile::tempdir().unwrap();
    write(temp.path(), ".gitignore", "ignored/\n*.generated.scan\n");
    for name in ["keep.scan", "nested/keep.scan", "ignored/a.scan",
        "x.generated.scan", ".private/a.scan"] {
        write(temp.path(), name, "clean");
    }
    let found = scan(temp.path(), "**/*.scan", MAX_FILES);
    assert_eq!(found.files.into_iter().collect::<Vec<_>>(), vec!["keep.scan", "nested/keep.scan"]);
    assert_eq!(found.errors, 0);
    assert!(found.stop.is_none());
}

#[test]
fn globs_reject_escape_and_negation_instead_of_reporting_no_errors() {
    for glob in ["", "/tmp/*.rs", "../*.rs", "src/../../*.rs", "!*.rs",
        "C:/*.rs", "src\\*.rs", "src/\n*.rs", "["] {
        assert!(matcher(glob).is_err(), "{glob:?}");
    }
    assert!(matcher("./src/**/*.rs").unwrap().is_match("src/lib.rs"));
    assert!(matcher("{a,b}.scan").unwrap().is_match("b.scan"));
}

#[test]
#[cfg(unix)]
fn does_not_follow_file_or_directory_symlinks_and_rechecks_before_reading() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let outside = tempfile::tempdir().unwrap();
    write(outside.path(), "secret.scan", "secret");
    symlink(outside.path(), root.join("linked")).unwrap();
    symlink(outside.path().join("secret.scan"), root.join("linked.scan")).unwrap();
    write(&root, "real.scan", "clean");
    let found = scan(&root, "**/*.scan", MAX_FILES);
    assert_eq!(found.files.into_iter().collect::<Vec<_>>(), vec!["real.scan"]);
    assert!(read_source(&root, "linked.scan").is_err());
    assert!(read_source(&root, "linked/secret.scan").is_err());
}

#[test]
fn oversized_and_non_utf8_documents_fail_bounded_read_admission() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::File::create(root.join("big.scan")).unwrap().set_len(MAX_FILE_BYTES + 1).unwrap();
    assert!(read_source(&root, "big.scan").unwrap_err().to_string().contains("LSP_FILE_LIMIT"));
    std::fs::write(root.join("binary.scan"), [0xff, 0xfe]).unwrap();
    assert!(read_source(&root, "binary.scan").is_err());
    assert!(scoped_file(&root, "../other.scan").is_err());
}

#[test]
fn discovery_does_not_obtain_io_authority_from_an_unprivileged_owner() {
    let temp = tempfile::tempdir().unwrap();
    let error = discover(temp.path(), &matcher("**/*.scan").unwrap(), 1,
        &AgentCx::for_testing(), Instant::now(), Duration::from_secs(1)).err().unwrap();
    assert!(error.to_string().contains("LSP_IO_PERMISSION"));
}

#[test]
fn encoded_output_limits_count_json_escapes_and_utf8_bytes() {
    let value = json!({"message":"\n😀".repeat(64)});
    let bytes = value.to_string().len();
    assert_eq!(size_within(&value, bytes), Some(bytes));
    assert_eq!(size_within(&value, bytes - 1), None);
}

#[test]
fn depth_and_time_limits_remain_explicit_not_successful_empty_scans() {
    let temp = tempfile::tempdir().unwrap();
    let mut path = temp.path().to_path_buf();
    for _ in 0..MAX_DEPTH { path.push("d"); }
    std::fs::create_dir_all(&path).unwrap();
    write(&path, "unvisited.scan", "broken");
    assert_eq!(scan(temp.path(), "**/*.scan", MAX_FILES).stop, Some("depth_limit"));
    let found = discover(temp.path(), &matcher("**/*.scan").unwrap(), MAX_FILES,
        &AgentCx::for_current_or_request(), Instant::now(), Duration::ZERO).unwrap();
    assert_eq!(found.stop, Some("timeout"));
    assert!(found.files.is_empty());
}

fn fixture(root: &Path, mode: &str) -> Option<LspTool> {
    let python = ["python3", "python"].into_iter().find(|program| {
        Command::new(program).arg("--version").stdout(Stdio::null()).stderr(Stdio::null())
            .status().is_ok_and(|status| status.success())
    });
    let Some(python) = python else {
        assert!(std::env::var_os("PI_LSP_REQUIRE_PROTOCOL").is_none(),
            "Python required for workspace diagnostics protocol tests");
        eprintln!("SKIP workspace diagnostics protocol tests: Python unavailable");
        return None;
    };
    let root = root.canonicalize().unwrap();
    write(&root, ".scan-root", "");
    let script = root.join("scan_peer.py");
    std::fs::write(&script, include_str!("test_server.py")).unwrap();
    let config = Config {
        lsp: Some(LspSettings {
            servers: Some(HashMap::from([("scan-fixture".to_string(), LspServerSettings {
                command: Some(python.to_string()),
                args: Some(vec!["-I".to_string(), "-u".to_string(), script.display().to_string(), mode.to_string()]),
                extensions: Some(vec![".scan".to_string()]),
                languages: Some(vec!["plaintext".to_string()]),
                root_markers: Some(vec![".scan-root".to_string()]),
                ..Default::default()
            })])),
            ..Default::default()
        }),
        ..Default::default()
    };
    Some(LspTool::new(&root, Some(&config)))
}

fn run(tool: &LspTool, input: Value) -> Result<ToolOutput> {
    // Follow tests/lsp.rs: parking can strand timer wakeups in test runtimes.
    let runtime = asupersync::runtime::RuntimeBuilder::new()
        .enable_parking(false).worker_threads(1).blocking_threads(1, 8).build().unwrap();
    runtime.block_on(tool.execute("workspace-diagnostics", input, None))
}

fn request() -> Value {
    json!({"action":"workspace_diagnostics","file":"**/*.scan","timeout":5})
}

fn events(root: &Path) -> Vec<Value> {
    std::fs::read_to_string(root.join("requests.jsonl")).unwrap().lines()
        .map(|line| serde_json::from_str(line).unwrap()).collect()
}

#[test]
fn public_cold_scan_checks_every_match_and_preserves_explicit_empty_reports() {
    for mode in ["pull", "push"] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), mode) else { return; };
        write(temp.path(), "a.scan", "broken\n");
        write(temp.path(), "nested/b.scan", "clean\n");
        write(temp.path(), "ignored.txt", "broken\n");
        assert!(tool.registry.status().is_empty());
        let output = run(&tool, request()).unwrap();
        assert!(!output.is_error, "{mode}: {:?}", output.details);
        let report = output.details.unwrap();
        assert_eq!(report["cachedOnly"], false);
        assert_eq!(report["complete"], true);
        assert_eq!(report["checkedFiles"], 2);
        assert_eq!(report["entries"][0]["count"], 1);
        assert_eq!(report["entries"][1]["diagnostics"], json!([]));
        let log = events(temp.path());
        assert_eq!(log.iter().filter(|event| event["method"] == "initialize").count(), 1);
        assert_eq!(log.iter().filter(|event| event["method"] == "textDocument/didOpen").count(), 2);
    }
}

#[test]
fn repeated_scan_synchronizes_changed_files_instead_of_reusing_old_errors() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "pull") else { return; };
    write(temp.path(), "a.scan", "broken\n");
    assert_eq!(run(&tool, request()).unwrap().details.unwrap()["entries"][0]["count"], 1);
    write(temp.path(), "a.scan", "fixed\n");
    let report = run(&tool, request()).unwrap().details.unwrap();
    assert_eq!(report["complete"], true);
    assert_eq!(report["entries"][0]["count"], 0);
}

#[test]
fn missing_reports_and_provider_failures_are_not_mislabeled_clean_files() {
    for mode in ["error", "pending"] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), mode) else { return; };
        write(temp.path(), "a.scan", "broken\n");
        write(temp.path(), "b.scan", "clean\n");
        let mut input = request();
        input["timeout"] = json!(1);
        let output = run(&tool, input).unwrap();
        assert!(output.is_error);
        let report = output.details.unwrap();
        assert_eq!(report["complete"], false);
        assert_eq!(report["entries"][0]["status"], "error");
        assert!(report["entries"][0].get("diagnostics").is_none());
        if mode == "error" { assert_eq!(report["entries"][1]["status"], "checked"); }
    }
}

#[test]
fn a_later_analysis_cannot_leave_earlier_changed_sources_marked_current() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "drift") else { return; };
    write(temp.path(), "a.scan", "broken\n");
    write(temp.path(), "b.scan", "clean\n");
    let output = run(&tool, request()).unwrap();
    assert!(output.is_error);
    let report = output.details.unwrap();
    assert_eq!(report["complete"], false);
    assert_eq!(report["entries"][0]["status"], "error");
    assert!(report["entries"][0]["error"].as_str().unwrap().contains("LSP_DIAGNOSTIC_STALE"));
    assert!(report["entries"][0].get("diagnostics").is_none());
}

#[test]
fn report_size_limit_keeps_structured_results_and_does_not_claim_clean() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "oversize") else { return; };
    write(temp.path(), "a.scan", "broken\n");
    let report = run(&tool, request()).unwrap().details.unwrap();
    assert!(report.is_object());
    assert_eq!(report["complete"], false);
    assert_eq!(report["outputTruncated"], true);
    assert_eq!(report["entries"][0]["status"], "output_limit");
    assert_eq!(report["entries"][0]["count"], 1);
    assert!(report.to_string().len() <= MAX_PAYLOAD_BYTES);
}

#[test]
fn file_limit_and_brace_globs_reach_public_dispatch_without_guessing() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "pull") else { return; };
    for file in ["c.scan", "b.scan", "a.scan"] { write(temp.path(), file, "clean\n"); }
    let report = run(&tool, json!({"action":"workspace_diagnostics","file":"{a,b,c}.scan","limit":1,"timeout":5}))
        .unwrap().details.unwrap();
    assert_eq!(report["entries"][0]["file"], "a.scan");
    assert_eq!(report["matchedFiles"], 3);
    assert_eq!(report["stopReason"], "file_limit");
    assert_eq!(report["complete"], false);
    assert_eq!(events(temp.path()).iter().filter(|event| event["method"] == "textDocument/didOpen").count(), 1);
}

#[test]
fn unsupported_files_and_empty_matches_never_start_an_unrelated_server() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "pull") else { return; };
    let empty = run(&tool, request()).unwrap().details.unwrap();
    assert_eq!(empty["complete"], true);
    assert_eq!(empty["matchedFiles"], 0);
    write(temp.path(), "a.unknownscan", "broken\n");
    let output = run(&tool, json!({"action":"workspace_diagnostics","file":"*.unknownscan"})).unwrap();
    assert!(output.is_error);
    assert_eq!(output.details.unwrap()["complete"], false);
    assert!(tool.registry.status().is_empty());
    assert!(!temp.path().join("requests.jsonl").exists());
}

#[test]
fn total_deadline_bounds_a_server_that_never_finishes_a_document_report() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "hang") else { return; };
    write(temp.path(), "a.scan", "broken\n");
    let started = Instant::now();
    let output = run(&tool, json!({"action":"workspace_diagnostics","file":"*.scan","timeout":1})).unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(output.is_error);
    assert_eq!(output.details.unwrap()["complete"], false);
}

#[test]
fn invalid_inputs_fail_before_server_initialization() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "pull") else { return; };
    for input in [json!({"action":"workspace_diagnostics","file":"../*.scan"}),
        json!({"action":"workspace_diagnostics","file":"*.scan","limit":0})] {
        assert!(run(&tool, input).unwrap_err().to_string().contains("LSP_USAGE"));
    }
    assert!(tool.registry.status().is_empty());
}
