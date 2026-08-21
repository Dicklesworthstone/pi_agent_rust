//! Integration tests for the internal URL scheme router (bd-cv653.6.3).
//!
//! Acceptance coverage:
//! 1. `read pr://<owner/repo>/<n>` returns the issue card (gh backend
//!    stubbed via `ResolveOptions`).
//! 2. `read skill://<name>` returns the skill content, identical to the
//!    resources loader, through the read tool.
//! 3. Merge-conflict fixture: read marks regions; write @theirs resolves;
//!    conflict://* bulk form.
//! 4. Unknown scheme foo:// → error listing available schemes.
//!
//! Logging: structured JSONL per tests/common/logging.rs, v2-validated,
//! recorded as artifacts.

mod common;

use common::TestHarness;
use common::logging::validate_jsonl_v2_only;
use pi::tools::{Tool, ToolOutput};
use serde_json::json;
use std::path::{Path, PathBuf};

fn first_text(output: &ToolOutput) -> &str {
    output
        .content
        .iter()
        .find_map(|block| match block {
            pi::model::ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .unwrap_or("")
}

fn finish_case(harness: &TestHarness, case: &str) {
    harness
        .log()
        .info("verify", format!("case '{case}' assertions passed"));
    let path = harness.temp_path(format!("{case}.jsonl"));
    harness
        .write_jsonl_logs(&path)
        .expect("write JSONL test logs");
    let payload = std::fs::read_to_string(&path).expect("read JSONL test logs");
    let errors = validate_jsonl_v2_only(&payload);
    assert!(errors.is_empty(), "JSONL v2 validation errors: {errors:?}");
}

fn block_on_local<F: std::future::Future>(future: F) -> F::Output {
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .expect("failed to build test runtime");
    runtime.block_on(future)
}

fn git(repo: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("git");
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn init_repo_with_conflict(harness: &TestHarness, tag: &str) -> PathBuf {
    let dir = harness.temp_path(tag);
    std::fs::create_dir_all(&dir).expect("dir");
    git(&dir, &["init", "-b", "main"]);
    git(&dir, &["config", "user.email", "t@t"]);
    git(&dir, &["config", "user.name", "T"]);
    std::fs::write(dir.join("f.txt"), "line\n").expect("write");
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "init"]);
    git(&dir, &["checkout", "-b", "side"]);
    std::fs::write(dir.join("f.txt"), "side\n").expect("side");
    git(&dir, &["commit", "-am", "side"]);
    git(&dir, &["checkout", "main"]);
    std::fs::write(dir.join("f.txt"), "main\n").expect("main");
    git(&dir, &["commit", "-am", "main"]);
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(&dir)
        .args(["merge", "side"])
        .output()
        .expect("merge");
    assert!(!out.status.success(), "merge must conflict");
    dir
}

#[test]
fn read_skill_via_tool_matches_loader() {
    let case = "read_skill_via_tool_matches_loader";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let name = format!("pi-url-skill-{}", std::process::id());

    // Author a managed skill (dead-last tier), then read it back through
    // the tool and compare with the loader.
    pi::skills_managed::create(&name, "url router test skill", "body payload").expect("create");
    let tool = pi::tools::ReadTool::new(&root);
    let out = block_on_local(tool.execute("t1", json!({"path": format!("skill://{name}")}), None))
        .expect("read skill");
    let text = first_text(&out);
    harness.log().info("verify", format!("skill read: {text}"));
    assert!(text.contains("url router test skill"), "{text}");
    assert!(text.contains("body payload"), "{text}");
    assert!(!out.is_error, "{text}");

    let skills = pi::resources::load_skills(pi::resources::LoadSkillsOptions {
        cwd: root,
        agent_dir: pi::config::Config::global_dir(),
        skill_paths: Vec::new(),
        include_defaults: true,
    });
    let loaded = skills
        .skills
        .iter()
        .find(|skill| skill.name == name)
        .expect("loader sees it");
    let loader_content = std::fs::read_to_string(&loaded.file_path).expect("loader file");
    for line in loader_content.lines().take(4) {
        assert!(text.contains(line.trim()), "loader line missing: {line}");
    }
    pi::skills_managed::delete(&name).expect("cleanup");
    finish_case(&harness, case);
}

#[test]
fn conflict_read_write_bulk_and_resolution() {
    let case = "conflict_read_write_bulk_and_resolution";
    let harness = TestHarness::new(case);
    let repo = init_repo_with_conflict(&harness, "tool");
    let tool = pi::tools::ReadTool::new(&repo);

    let out = block_on_local(tool.execute("t1", json!({"path": "conflict://0"}), None))
        .expect("read conflict");
    let text = first_text(&out);
    harness
        .log()
        .info("verify", format!("conflict doc: {text}"));
    assert!(text.contains("--- ours ---"), "{text}");
    assert!(text.contains("--- theirs ---"), "{text}");

    let bulk =
        block_on_local(tool.execute("t2", json!({"path": "conflict://*"}), None)).expect("bulk");
    assert!(first_text(&bulk).contains("conflict 0"));

    let theirs = block_on_local(tool.execute("t3", json!({"path": "conflict://0 @theirs"}), None))
        .expect("theirs");
    assert!(first_text(&theirs).contains("side"));

    // Write the resolution: @theirs wins.
    let region = pi::url_router::write_conflict_resolution(&repo, 0, "theirs").expect("resolve");
    assert_eq!(region.file, "f.txt");
    let content = std::fs::read_to_string(repo.join("f.txt")).expect("read");
    harness
        .log()
        .info("verify", format!("resolved file: {content}"));
    assert_eq!(content.trim(), "side");
    assert!(
        pi::url_router::conflict_regions(&repo)
            .expect("regions")
            .is_empty(),
        "no conflicts remain after resolution"
    );
    finish_case(&harness, case);
}

#[test]
fn unknown_scheme_errors_with_registered_list() {
    let case = "unknown_scheme_errors_with_registered_list";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let tool = pi::tools::ReadTool::new(&root);
    let err = block_on_local(tool.execute("t1", json!({"path": "foo://bar"}), None))
        .expect_err("foo:// must be refused");
    let text = err.to_string();
    harness
        .log()
        .info("verify", format!("unknown scheme: {text}"));
    assert!(text.contains("PI_URL_UNKNOWN_SCHEME"), "{text}");
    assert!(text.contains("skill://"), "{text}");
    assert!(text.contains("conflict://"), "{text}");
    finish_case(&harness, case);
}

#[cfg(unix)]
#[test]
fn pr_view_via_stubbed_gh_backend() {
    use std::os::unix::fs::PermissionsExt;
    let case = "pr_view_via_stubbed_gh_backend";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");

    let stub = root.join("gh");
    std::fs::write(
        &stub,
        "#!/bin/sh\necho 'STUB ISSUE CARD #1428: router works'\n",
    )
    .expect("write stub");
    let mut perms = std::fs::metadata(&stub).expect("stat").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&stub, perms).expect("chmod");

    // Probe: some endpoint-security setups stall exec of fresh unsigned
    // scripts (mirrors the github tool's stub probe).
    let probe = std::process::Command::new(&stub).output();
    match probe {
        Ok(out) if out.status.success() => {}
        _ => {
            harness
                .log()
                .info("skip", "host stalls exec of fresh scripts".to_string());
            return;
        }
    }

    let options = pi::url_router::ResolveOptions {
        gh_binary: Some(stub.to_string_lossy().into_owned()),
    };
    let doc =
        pi::url_router::resolve_with("pr://owner/repo/1428", &root, &options).expect("resolve pr");
    harness.log().info(
        "verify",
        format!(
            "pr doc: {} (backend {:?})",
            doc.content.trim(),
            doc.metadata
        ),
    );
    assert!(
        doc.content.contains("STUB ISSUE CARD #1428"),
        "{}",
        doc.content
    );
    assert_eq!(doc.scheme, "pr");
    finish_case(&harness, case);
}

#[test]
fn pagination_contract_matches_file_reads() {
    let case = "pagination_contract_matches_file_reads";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    // Seed a scratch document with 10 numbered lines.
    let mut content = String::new();
    for n in 1..=10 {
        use std::fmt::Write as _;
        let _ = writeln!(content, "line {n}");
    }
    pi::url_router::write_local("paged", &content).expect("seed");
    let tool = pi::tools::ReadTool::new(&root);
    let out = block_on_local(tool.execute(
        "t1",
        json!({"path": "local://paged", "offset": 4, "limit": 3}),
        None,
    ))
    .expect("paged read");
    let text = first_text(&out);
    harness
        .log()
        .info("verify", format!("paged window: {text}"));
    assert!(text.contains("line 4"), "{text}");
    assert!(text.contains("line 6"), "{text}");
    assert!(
        !text.contains("line 7\n"),
        "window must end at the limit: {text}"
    );
    assert!(text.contains("4→"), "1-based line numbers: {text}");
    let err =
        block_on_local(tool.execute("t2", json!({"path": "local://paged", "offset": 99}), None))
            .expect_err("beyond end");
    assert!(err.to_string().contains("beyond end of document"), "{err}");
    finish_case(&harness, case);
}
