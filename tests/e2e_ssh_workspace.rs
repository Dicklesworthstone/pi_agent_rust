//! Fixture-sshd e2e for ssh:// workspace tools (bd-cv653.6.5).
//!
//! **Gated live lane**: skipped unless `PI_SSH_E2E=1`. The fixture (a
//! userspace `/usr/sbin/sshd` on 127.0.0.1 plus a scoped OpenSSH client
//! config) is owned by `scripts/e2e/run_ssh_workspace.sh`, which exports
//! `PI_SSH_CLIENT_CONFIG_FILE`, `PI_SSH_ALLOWED_HOSTS`, and
//! `PI_SSH_E2E_WORK`. This target then drives the REAL tool surfaces —
//! read / write / edit / hashline_edit — through the url_router against
//! the fixture, plus the non-allowlisted-host refusal.
//!
//! Scratch files live under `$PI_SSH_E2E_WORK` and are intentionally left
//! in place (repo policy: agents never delete); the OS temp cleaner
//! reclaims them. The crate's `unsafe_code = "forbid"` lint is honored:
//! this target performs no environment mutation.

use std::path::{Path, PathBuf};
mod common;


use asupersync::test_utils;
use pi::model::ContentBlock;
use pi::tools::{EditTool, HashlineEditTool, ReadTool, Tool, WriteTool};

use common::logging::TestLogger;

const BEAD: &str = "bd-cv653.6.5";

fn get_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn url_for(work: &Path, name: &str) -> String {
    format!("ssh://127.0.0.1{}", work.join(name).to_string_lossy())
}

#[test]
fn ssh_workspace_roundtrip_fixture_sshd() {
    if std::env::var("PI_SSH_E2E").as_deref() != Ok("1") {
        eprintln!("[ssh-e2e] skipped: run scripts/e2e/run_ssh_workspace.sh to enable");
        return;
    }
    let work = PathBuf::from(
        std::env::var("PI_SSH_E2E_WORK").expect("PI_SSH_E2E_WORK exported by the runner"),
    );

    test_utils::run_test(|| async move {
        let logger = TestLogger::new();
        logger.info(BEAD, "fixture ready (owned by runner script)");

        // Seed remote-side files (loopback sshd shares this filesystem).
        let hello_remote = work.join("hello.txt");
        std::fs::write(&hello_remote, "alpha\nbeta\n").expect("seed hello");
        let cwd = work.clone();

        // 1) read round-trip through the router.
        logger.info(BEAD, "case=read begin");
        let read_tool = ReadTool::new(&cwd);
        let doc = read_tool
            .execute(
                "t-read",
                serde_json::json!({ "path": url_for(&work, "hello.txt") }),
                None,
            )
            .await
            .expect("read over ssh");
        assert!(get_text(&doc.content).contains("alpha"), "{doc:?}");
        logger.info(BEAD, "case=read ok");

        // 2) write round-trip via the real WriteTool surface.
        logger.info(BEAD, "case=write begin");
        let payload = "written-over-ssh";
        let write_out = WriteTool::new(&cwd)
            .execute(
                "t-write",
                serde_json::json!({
                    "path": url_for(&work, "out.txt"),
                    "content": payload,
                }),
                None,
            )
            .await
            .expect("write over ssh");
        assert!(!write_out.is_error, "{write_out:?}");
        assert_eq!(
            std::fs::read_to_string(work.join("out.txt")).expect("written"),
            payload
        );
        logger.info(BEAD, "case=write ok");

        // 3) edit round-trip via the real EditTool surface.
        logger.info(BEAD, "case=edit begin");
        let edit_out = EditTool::new(&cwd)
            .execute(
                "t-edit",
                serde_json::json!({
                    "path": url_for(&work, "out.txt"),
                    "oldText": "written",
                    "newText": "edited",
                }),
                None,
            )
            .await
            .expect("edit over ssh");
        assert!(get_text(&edit_out.content).contains("Successfully replaced"));
        assert_eq!(
            std::fs::read_to_string(work.join("out.txt")).expect("edited"),
            "edited-over-ssh"
        );
        logger.info(BEAD, "case=edit ok");

        // 4) hashline anchors honored across ssh:// — tags come from a local
        // twin (hashes depend only on line index + line content).
        logger.info(BEAD, "case=hashline begin");
        let twin = work.join("tags_local_twin.txt");
        std::fs::write(&twin, "l0\nl1\nl2\n").expect("twin seed");
        let tags_read = ReadTool::new(&cwd)
            .execute(
                "t-tags",
                serde_json::json!({
                    "path": twin.to_string_lossy(),
                    "hashline": true,
                }),
                None,
            )
            .await
            .expect("tagged local read");
        let anchor = get_text(&tags_read.content)
            .lines()
            .next()
            .and_then(|l| l.split(':').next())
            .map(str::to_string)
            .expect("anchor tag");

        let tags_remote = work.join("tags.txt");
        std::fs::write(&tags_remote, "l0\nl1\nl2\n").expect("remote seed");
        HashlineEditTool::new(&cwd)
            .execute(
                "t-hash",
                serde_json::json!({
                    "path": url_for(&work, "tags.txt"),
                    "edits": [
                        { "op": "append", "pos": anchor, "lines": ["appended"] }
                    ],
                }),
                None,
            )
            .await
            .expect("hashline edit over ssh");
        assert_eq!(
            std::fs::read_to_string(&tags_remote).expect("spliced"),
            "l0\nappended\nl1\nl2\n"
        );
        logger.info(BEAD, "case=hashline ok");

        // 5) remote-change conflict rejected: stale anchor after the file
        // changed underneath us.
        logger.info(BEAD, "case=stale-anchor begin");
        std::fs::write(&tags_remote, "changed underneath\n").expect("remote change");
        let stale = HashlineEditTool::new(&cwd)
            .execute(
                "t-stale",
                serde_json::json!({
                    "path": url_for(&work, "tags.txt"),
                    "edits": [
                        { "op": "replace", "pos": anchor, "lines": ["boom"] }
                    ],
                }),
                None,
            )
            .await;
        let err = stale.expect_err("stale anchor must be rejected");
        assert!(
            err.to_string().contains("Hash validation failed"),
            "unexpected error: {err}"
        );
        logger.info(BEAD, "case=stale-anchor ok");

        // 6) non-allowlisted host → named refusal before any spawn.
        logger.info(BEAD, "case=refusal begin");
        let refused = WriteTool::new(&cwd)
            .execute(
                "t-refuse",
                serde_json::json!({
                    "path": "ssh://unlisted-host-9z7/tmp/pi-e2e-probe",
                    "content": "x",
                }),
                None,
            )
            .await;
        let err = refused.expect_err("unlisted host must be refused");
        assert!(
            err.to_string().contains("PI_SSH_HOST_NOT_ALLOWED"),
            "unexpected error: {err}"
        );
        logger.info(BEAD, "case=refusal ok");

        let artifact_dir = std::path::PathBuf::from(
            std::env::var("E2E_ARTIFACT_DIR")
                .unwrap_or_else(|_| format!("tests/e2e_results/ssh/local")),
        );
        std::fs::create_dir_all(&artifact_dir).expect("artifact dir");
        logger
            .write_jsonl_to_path(artifact_dir.join("ssh_workspace.jsonl"))
            .ok();
        println!("[ssh-e2e] PASS (artifacts: {})", artifact_dir.display());
    });
}

