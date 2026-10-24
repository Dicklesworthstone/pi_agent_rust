//! Integration tests for the internal URL scheme router (bd-cv653.6.3).
//!
//! Acceptance coverage:
//! 1. `read pr://<owner/repo>/<n>` returns the issue card (gh backend
//!    stubbed via `ResolveOptions`).
//! 2. `read skill://<name>` returns the skill content, identical to the
//!    resources loader, through the read tool.
//! 3. Merge-conflict fixtures: guarded native writes, typed batches, exact
//!    authored bytes, workspace scope, undo/redo and invalid-input refusal.
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
    git(&dir, &["config", "commit.gpgsign", "false"]);
    git(&dir, &["config", "merge.conflictStyle", "merge"]);
    git(&dir, &["config", "core.autocrlf", "false"]);
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

fn conflict_token(repo: &Path, index: usize) -> String {
    let output = block_on_local(pi::tools::ReadTool::new(repo).execute(
        "inspect-conflict",
        json!({"path": format!("conflict://{index}")}),
        None,
    ))
    .expect("read current conflict token");
    output.details.as_ref().expect("read details")["metadata"]["token"]
        .as_str()
        .expect("snapshot token")
        .to_owned()
}

fn add_conflict_path(repo: &Path, relative: &str, content: &str) {
    use std::io::Write as _;
    let output = std::process::Command::new("git")
        .current_dir(repo)
        .args(["ls-files", "--unmerged", "-z", "--", "f.txt"])
        .output()
        .expect("stages");
    assert!(output.status.success());
    let mut records = Vec::new();
    for record in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let tab = record
            .iter()
            .position(|byte| *byte == b'\t')
            .expect("stage header");
        records.extend_from_slice(&record[..tab]);
        records.push(b'\t');
        records.extend_from_slice(relative.as_bytes());
        records.push(0);
    }
    let mut child = std::process::Command::new("git")
        .current_dir(repo)
        .args(["update-index", "-z", "--index-info"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("fixture index");
    child
        .stdin
        .take()
        .expect("index input")
        .write_all(&records)
        .expect("write stages");
    assert!(child.wait().expect("index exit").success());
    let path = repo.join(relative);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
    std::fs::write(path, content).expect("second conflict image");
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
    assert!(text.contains("--- ours (\"HEAD\") ---"), "{text}");
    assert!(text.contains("--- theirs (\"side\") ---"), "{text}");

    let bulk =
        block_on_local(tool.execute("t2", json!({"path": "conflict://*"}), None)).expect("bulk");
    assert!(first_text(&bulk).contains("conflict 0"));

    let theirs = block_on_local(tool.execute("t3", json!({"path": "conflict://0 @theirs"}), None))
        .expect("theirs");
    assert!(first_text(&theirs).contains("side"));

    let index = std::fs::read(repo.join(".git/index")).expect("index before");
    let written = block_on_local(pi::tools::WriteTool::new(&repo).execute(
        "resolve",
        json!({"path":"conflict://*", "conflicts":[{
            "index":0, "token":conflict_token(&repo, 0), "resolution":{"kind":"theirs"}
        }]}),
        None,
    ))
    .expect("resolve through native write");
    assert_eq!(
        written.details.as_ref().unwrap()["schema"],
        "pi.conflict.write.v1"
    );
    assert_eq!(
        written.details.as_ref().unwrap()["regions"][0]["file"],
        "f.txt"
    );
    assert_eq!(
        std::fs::read(repo.join(".git/index")).expect("index after"),
        index
    );
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
fn conflict_write_preserves_literal_bytes_and_supports_undo_redo() {
    let case = "conflict_write_preserves_literal_bytes_and_supports_undo_redo";
    let harness = TestHarness::new(case);
    let repo = init_repo_with_conflict(&harness, "literal");
    let block = "<<<<<<< ours\r\nours 😀\r\n||||||| base\r\nbase\r\n=======\r\ntheirs 雪\r\n>>>>>>> theirs\r\n";
    let original = format!("\u{feff}prefix\r\n{block}middle\r\n{block}tail");
    std::fs::write(repo.join("f.txt"), &original).expect("exact fixture");
    let recorder = std::sync::Arc::new(pi::undo::FileMutationRecorder::default());
    let tool = pi::tools::WriteTool::new(&repo).with_mutation_recorder(Some(recorder.clone()));
    let replacement = "@theirs\r\n  merged 😀\r\n";
    let expected = format!("\u{feff}prefix\r\n{block}middle\r\n{replacement}tail");
    let token = conflict_token(&repo, 1);
    block_on_local(tool.execute(
        "literal",
        json!({
            "path":format!("conflict://1/{token}"), "content":replacement
        }),
        None,
    ))
    .expect("write authored region");
    assert_eq!(
        std::fs::read(repo.join("f.txt")).unwrap(),
        expected.as_bytes()
    );
    assert_eq!(recorder.stats().undo_depth, 1);
    assert_eq!(recorder.undo(1, false).applied.len(), 1);
    assert_eq!(
        std::fs::read(repo.join("f.txt")).unwrap(),
        original.as_bytes()
    );
    assert_eq!(recorder.redo(1, false).applied.len(), 1);
    assert_eq!(
        std::fs::read(repo.join("f.txt")).unwrap(),
        expected.as_bytes()
    );
    finish_case(&harness, case);
}

#[test]
fn conflict_write_batch_refuses_partial_admission_and_undoes_all_files() {
    let case = "conflict_write_batch_refuses_partial_admission_and_undoes_all_files";
    let harness = TestHarness::new(case);
    let repo = init_repo_with_conflict(&harness, "batch");
    let original = std::fs::read_to_string(repo.join("f.txt")).unwrap();
    add_conflict_path(&repo, "nested/g.txt", &original);
    let index = std::fs::read(repo.join(".git/index")).unwrap();
    let recorder = std::sync::Arc::new(pi::undo::FileMutationRecorder::default());
    let tool = pi::tools::WriteTool::new(&repo).with_mutation_recorder(Some(recorder.clone()));
    let first =
        json!({"index":0, "token":conflict_token(&repo, 0), "resolution":{"kind":"theirs"}});
    let second = json!({"index":1, "token":conflict_token(&repo, 1), "resolution":{"kind":"text","value":"merged\n"}});
    let mut stale = second.clone();
    stale["token"] = json!("0".repeat(64));
    let mut absent_base = second.clone();
    absent_base["resolution"] = json!({"kind":"base"});
    for choices in [
        json!([first.clone(), stale]),
        json!([first.clone(), first.clone()]),
        json!([first, absent_base]),
    ] {
        block_on_local(tool.execute(
            "refused",
            json!({"path":"conflict://*","conflicts":choices}),
            None,
        ))
        .expect_err("one invalid choice must refuse the complete batch");
        assert_eq!(
            std::fs::read_to_string(repo.join("f.txt")).unwrap(),
            original
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("nested/g.txt")).unwrap(),
            original
        );
        assert_eq!(recorder.stats().undo_depth, 0);
    }
    let output = block_on_local(tool.execute(
        "batch",
        json!({
            "path":"conflict://*", "conflicts":[first, second]
        }),
        None,
    ))
    .expect("valid multi-file batch");
    assert_eq!(
        output.details.as_ref().unwrap()["regions"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("f.txt")).unwrap(),
        "side\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("nested/g.txt")).unwrap(),
        "merged\n"
    );
    assert_eq!(
        recorder.stats().undo_depth,
        1,
        "one tool call is one undo unit"
    );
    assert_eq!(recorder.undo(1, false).applied.len(), 1);
    assert_eq!(
        std::fs::read_to_string(repo.join("f.txt")).unwrap(),
        original
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("nested/g.txt")).unwrap(),
        original
    );
    assert_eq!(recorder.redo(1, false).applied.len(), 1);
    assert_eq!(
        std::fs::read_to_string(repo.join("nested/g.txt")).unwrap(),
        "merged\n"
    );
    assert_eq!(std::fs::read(repo.join(".git/index")).unwrap(), index);
    finish_case(&harness, case);
}

#[test]
fn conflict_write_uses_live_workspace_roots_instead_of_the_git_root() {
    let case = "conflict_write_uses_live_workspace_roots_instead_of_the_git_root";
    let harness = TestHarness::new(case);
    let repo = init_repo_with_conflict(&harness, "workspace");
    let original = std::fs::read_to_string(repo.join("f.txt")).unwrap();
    add_conflict_path(&repo, "nested/g.txt", &original);
    let cwd = repo.join("nested");
    let mut workspace = pi::workspace::WorkspaceHandle::single(&cwd);
    let recorder = std::sync::Arc::new(pi::undo::FileMutationRecorder::default());
    let tool = pi::tools::WriteTool::new(&cwd)
        .with_workspace(workspace.clone())
        .with_mutation_recorder(Some(recorder.clone()));
    let request = json!({"path":"conflict://*", "conflicts":[
        {"index":0, "token":conflict_token(&cwd, 0), "resolution":{"kind":"theirs"}},
        {"index":1, "token":conflict_token(&cwd, 1), "resolution":{"kind":"ours"}}
    ]});
    let error = block_on_local(tool.execute("outside", request.clone(), None)).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Cannot write outside the working directory"),
        "{error}"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("nested/g.txt")).unwrap(),
        original
    );
    assert_eq!(recorder.stats().undo_depth, 0);
    workspace.add_root(&repo);
    block_on_local(tool.execute("authorized", request.clone(), None))
        .expect("live additional root");
    assert_eq!(recorder.undo(1, false).applied.len(), 1);
    assert!(workspace.remove_root(&repo));
    block_on_local(tool.execute("revoked", request, None))
        .expect_err("revoked root must not be cached");
    assert_eq!(
        std::fs::read_to_string(repo.join("f.txt")).unwrap(),
        original
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("nested/g.txt")).unwrap(),
        original
    );
    finish_case(&harness, case);
}

#[test]
fn conflict_write_rejects_bad_shapes_before_git_or_filesystem_fallback() {
    let case = "conflict_write_rejects_bad_shapes_before_git_or_filesystem_fallback";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let tool = pi::tools::WriteTool::new(&root);
    let token = "a".repeat(64);
    let choice = json!({"index":0,"token":token,"resolution":{"kind":"ours"}});
    for input in [
        json!({"path":"conflict://0","content":"x"}),
        json!({"path":"conflict://0/ours","content":"x"}),
        json!({"path":format!("conflict://0/{token}/theirs"),"content":"x"}),
        json!({"path":"conflict://*","conflicts":[]}),
        json!({"path":"conflict://*","conflicts":[choice],"content":""}),
        json!({"path":"conflict://*"}),
        json!({"path":"conflict://*","conflicts":[choice],"extra":true}),
        json!({"path":format!("conflict://0/{token}"),"content":"x","extra":true}),
        json!({"path":"conflict://*","conflicts":[{
            "index":0,"token":token,"resolution":{"kind":"ours"},"path":"elsewhere"
        }]}),
        json!({"path":"conflict://*","conflicts":[{
            "index":0,"token":token,"resolution":{"kind":"text","value":"x","extra":true}
        }]}),
        json!({"path":"conflict://*","conflicts":[{"index":0,"token":token,"resolution":{"kind":"shell","value":"ignored"}}]}),
        json!({"path":format!("conflict://0/{token}"),"content":"x".repeat(64*1024+1)}),
    ] {
        let error = block_on_local(tool.execute("invalid", input, None))
            .unwrap_err()
            .to_string();
        assert!(error.contains("PI_CONFLICT_"), "{error}");
        assert!(
            !error.contains("PI_CONFLICT_GIT"),
            "invalid input reached Git: {error}"
        );
    }
    std::fs::write(root.join("ordinary.txt"), "keep").unwrap();
    block_on_local(tool.execute(
        "wrong-path",
        json!({
            "path":"ordinary.txt","content":"overwrite","conflicts":[choice]
        }),
        None,
    ))
    .expect_err("conflict fields on ordinary writes are never ignored");
    assert_eq!(
        std::fs::read_to_string(root.join("ordinary.txt")).unwrap(),
        "keep"
    );
    assert!(!root.join("conflict:").exists());
    finish_case(&harness, case);
}

#[test]
fn write_effects_distinguish_files_conflicts_and_remote_schemes() {
    let case = "write_effects_distinguish_files_conflicts_and_remote_schemes";
    let harness = TestHarness::new(case);
    let tool = pi::tools::WriteTool::new(&harness.temp_path("."));
    let ordinary = tool.effects_for_input(&json!({"path":"f.txt","content":"x"}));
    assert!(ordinary.writes());
    assert!(!ordinary.processes() && !ordinary.networks());
    let conflict = tool.effects_for_input(&json!({"path":"conflict://*","conflicts":[]}));
    assert!(conflict.writes() && conflict.processes() && !conflict.networks());
    let remote = tool.effects_for_input(&json!({"path":"ssh://host/file","content":"x"}));
    assert!(remote.writes() && remote.processes() && remote.networks());
    let malformed = tool.effects_for_input(&json!({"path":7}));
    assert!(malformed.processes() && malformed.networks());
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
