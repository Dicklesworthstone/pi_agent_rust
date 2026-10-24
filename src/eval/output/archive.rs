//! Opt-in, disk-backed console and complete native cell-result archives.
//!
//! No final path is exposed until the complete spool has been synced and
//! published through the existing confined create-only artifact writer.

use crate::agent_cx::AgentCx;
use crate::artifact_output::{self, OutputTarget};
use crate::error::{Error, Result};
use crate::model::{ContentBlock, TextContent};
use crate::tools::ToolOutput;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const MAX_BYTES: u64 = 64 * 1024 * 1024;
const END_RESERVE: u64 = 4096;
const SCHEMA: &str = "pi.eval.output_artifact.v1";

#[cfg(test)]
mod result_tests;

fn error(code: &str, message: impl std::fmt::Display) -> Error {
    Error::tool("eval", format!("{code}: {message}"))
}

pub(in crate::eval) struct Archive {
    target: OutputTarget,
    spool: BufWriter<File>,
    owner: AgentCx,
    bytes: u64,
    sequence: u64,
    stdout_bytes: u64,
    stderr_bytes: u64,
    result_written: bool,
    failed: bool,
    limit: u64,
}

impl Archive {
    /// Explicit null or a wrong type must not silently disable a requested file.
    pub(in crate::eval) fn requested_path(input: &Value) -> Result<Option<String>> {
        let Some(value) = input.get("output_path") else {
            return Ok(None);
        };
        let path = value
            .as_str()
            .ok_or_else(|| error("EVAL_ARTIFACT_PATH", "output_path must be a string"))?;
        if Path::new(path)
            .extension()
            .and_then(|extension| extension.to_str())
            != Some("jsonl")
        {
            return Err(error(
                "EVAL_ARTIFACT_PATH",
                "output_path must name a new workspace-relative .jsonl file",
            ));
        }
        Ok(Some(path.to_owned()))
    }

    pub(in crate::eval) async fn prepare(
        cwd: PathBuf,
        path: String,
        kernel: &'static str,
    ) -> Result<Self> {
        let owner = AgentCx::for_current_or_request();
        asupersync::runtime::spawn_blocking(move || {
            let _current = owner.cx().clone().set_current_restricted();
            Self::prepare_sync(&cwd, &path, kernel, owner)
        })
        .await
    }

    fn prepare_sync(cwd: &Path, path: &str, kernel: &str, owner: AgentCx) -> Result<Self> {
        owner
            .checkpoint()
            .map_err(|_| error("EVAL_CANCELLED", "archive preparation cancelled"))?;
        let target = artifact_output::resolve_new(cwd, path, "eval")
            .map_err(|failure| error("EVAL_ARTIFACT_PATH", failure))?;
        if target.path().to_str().is_none() {
            return Err(error(
                "EVAL_ARTIFACT_PATH",
                "archive destination must be representable as UTF-8",
            ));
        }
        let file = tempfile::tempfile().map_err(|failure| error("EVAL_ARTIFACT_IO", failure))?;
        let mut archive = Self {
            target,
            spool: BufWriter::with_capacity(64 * 1024, file),
            owner,
            bytes: 0,
            sequence: 0,
            stdout_bytes: 0,
            stderr_bytes: 0,
            result_written: false,
            failed: false,
            limit: MAX_BYTES,
        };
        archive.write_record(
            &json!({"schema":SCHEMA, "kind":"start", "kernel":kernel}),
            false,
        )?;
        Ok(archive)
    }

    fn write_record<T: Serialize + ?Sized>(&mut self, record: &T, ending: bool) -> Result<()> {
        if self.failed {
            return Err(error(
                "EVAL_ARTIFACT_INCOMPLETE",
                "output archive has already failed",
            ));
        }
        let limit = if ending {
            self.limit
        } else {
            self.limit.saturating_sub(END_RESERVE)
        };
        // Serialize directly into the bounded spool. Rich images, large final
        // expressions and JSON escaping must not allocate another full result
        // before discovering that its encoded representation exceeds the cap.
        let mut writer = RecordWriter {
            spool: &mut self.spool,
            bytes: &mut self.bytes,
            owner: &self.owner,
            limit,
            exceeded: false,
        };
        let encoded = serde_json::to_writer(&mut writer, record)
            .map_err(std::io::Error::other)
            .and_then(|()| writer.write_all(b"\n"));
        if let Err(failure) = encoded {
            self.failed = true;
            if self.owner.checkpoint().is_err() {
                return Err(error("EVAL_CANCELLED", "output archive cancelled"));
            }
            if writer.exceeded {
                return Err(error(
                    "EVAL_ARTIFACT_LIMIT",
                    "output archive exceeds its 64 MiB encoded budget",
                ));
            }
            return Err(error("EVAL_ARTIFACT_IO", failure));
        }
        Ok(())
    }

    pub(super) fn record(&mut self, stream: &Value) -> Result<()> {
        if self.result_written {
            self.failed = true;
            return Err(error(
                "EVAL_ARTIFACT_INCOMPLETE",
                "console frame arrived after the completed result",
            ));
        }
        self.write_record(stream, false)?;
        self.sequence = stream["sequence"].as_u64().unwrap_or(self.sequence);
        let bytes = stream["text"].as_str().map_or(0, str::len) as u64;
        match stream["name"].as_str() {
            Some("stdout") => self.stdout_bytes = self.stdout_bytes.saturating_add(bytes),
            Some("stderr") => self.stderr_bytes = self.stderr_bytes.saturating_add(bytes),
            _ => {}
        }
        Ok(())
    }

    fn record_result(&mut self, output: &ToolOutput) -> Result<()> {
        if self.result_written {
            self.failed = true;
            return Err(error(
                "EVAL_ARTIFACT_INCOMPLETE",
                "duplicate completed cell result",
            ));
        }
        self.write_record(
            &ResultRecord {
                kind: "result",
                output,
            },
            false,
        )?;
        self.result_written = true;
        Ok(())
    }

    fn publish_sync(mut self, execution_failed: bool) -> Result<Value> {
        self.write_record(
            &json!({
                "kind":"end", "sequence":self.sequence,
                "status":if execution_failed { "error" } else { "success" },
                "stdoutBytes":self.stdout_bytes, "stderrBytes":self.stderr_bytes,
                "resultIncluded":self.result_written,
            }),
            true,
        )?;
        self.spool
            .flush()
            .map_err(|failure| error("EVAL_ARTIFACT_IO", failure))?;
        self.spool
            .seek(SeekFrom::Start(0))
            .map_err(|failure| error("EVAL_ARTIFACT_IO", failure))?;
        let mut source = HashingReader {
            file: self.spool.get_mut(),
            digest: Sha256::new(),
        };
        let bytes =
            artifact_output::publish_from_reader(&self.target, &mut source, self.bytes, "eval")
                .map_err(|failure| error("EVAL_ARTIFACT_PUBLISH", failure))?;
        // Hash the bytes actually read into the final staging file, not only
        // what the producer intended to write. Receipt is created after publish.
        let digest = crate::package_manager::hex_encode(&source.digest.finalize());
        Ok(json!({
            "schema":SCHEMA, "status":"written", "format":"jsonl",
            "path":self.target.path(), "bytes":bytes, "sha256":digest,
            "complete":true, "resultIncluded":self.result_written,
        }))
    }

    async fn publish(mut self, output: ToolOutput) -> (Result<Value>, ToolOutput) {
        let owner = self.owner.clone();
        asupersync::runtime::spawn_blocking(move || {
            let _current = owner.cx().clone().set_current_restricted();
            let receipt = self
                .record_result(&output)
                .and_then(|()| self.publish_sync(output.is_error));
            // Move, do not clone, the native result across the blocking work.
            // A failed archive never erases the already-completed cell output.
            (receipt, output)
        })
        .await
    }
}

#[derive(Serialize)]
struct ResultRecord<'a> {
    kind: &'static str,
    output: &'a ToolOutput,
}

struct RecordWriter<'a> {
    spool: &'a mut BufWriter<File>,
    bytes: &'a mut u64,
    owner: &'a AgentCx,
    limit: u64,
    exceeded: bool,
}

impl Write for RecordWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.owner
            .checkpoint()
            .map_err(|_| std::io::Error::other("output archive cancelled"))?;
        let amount = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        let Some(next) = self
            .bytes
            .checked_add(amount)
            .filter(|next| *next <= self.limit)
        else {
            self.exceeded = true;
            return Err(std::io::Error::other("output archive byte limit"));
        };
        self.spool.write_all(bytes)?;
        *self.bytes = next;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.spool.flush()
    }
}

struct HashingReader<'a> {
    file: &'a mut File,
    digest: Sha256,
}

impl Read for HashingReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let count = self.file.read(buffer)?;
        self.digest.update(&buffer[..count]);
        Ok(count)
    }
}

impl super::Capture {
    pub(in crate::eval) fn attach_archive(&mut self, archive: Option<Archive>) {
        self.archive = archive;
    }

    /// A completed backend result may be an ordinary code exception; archive
    /// its complete native output with an error trailer. A stopped/incomplete run
    /// must never acquire a misleading complete-file receipt.
    pub(in crate::eval) async fn finish_archive(
        &mut self,
        output: &mut ToolOutput,
        interrupted: bool,
    ) {
        let Some(archive) = self.archive.take() else {
            return;
        };
        let execution_failed = output.is_error;
        let owner = archive.owner.clone();
        let (metadata, notice) = if interrupted {
            (
                json!({"schema":SCHEMA, "status":"not_published", "complete":false}),
                "EVAL_ARTIFACT_NOT_PUBLISHED: output archive is incomplete after interruption"
                    .to_string(),
            )
        } else {
            // Exclude the receipt/notice that will be added after publication.
            // The archived result is exactly the native result at this boundary,
            // including rich content, structured details and its error flag.
            let completed = ToolOutput {
                content: std::mem::take(&mut output.content),
                details: output.details.take(),
                is_error: output.is_error,
            };
            let (receipt, completed) = archive.publish(completed).await;
            *output = completed;
            match receipt {
                Ok(receipt) => {
                    let path = receipt["path"].as_str().unwrap_or_default();
                    let mut notice = format!(
                        "Output archive saved to {} (JSONL; complete received console frames and native cell result, including rich displays)",
                        serde_json::to_string(path).unwrap_or_default()
                    );
                    // Publication is a commit point, not something a dropped
                    // or cancelled caller can roll back. Retain its actual
                    // receipt, but never turn a late cancellation into success.
                    if owner.checkpoint().is_err() {
                        output.is_error = true;
                        notice.push_str("; EVAL_CANCELLED: owner cancelled after publication; the written archive and completed cell effects were not rolled back");
                    }
                    (receipt, notice)
                }
                Err(failure) => {
                    output.is_error = true;
                    (
                        json!({"schema":SCHEMA, "status":"failed", "complete":false,
                        "executionFailed":execution_failed, "error":failure.to_string()}),
                        format!(
                            "{failure}; requested output archive was not published; completed cell effects were not rolled back"
                        ),
                    )
                }
            }
        };
        if output.details.is_none() {
            output.details = Some(json!({}));
        }
        if let Some(Value::Object(details)) = output.details.as_mut() {
            details.insert("outputArtifact".into(), metadata);
        }
        output
            .content
            .push(ContentBlock::Text(TextContent::new(notice)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::EvalTool;
    use crate::tools::{Tool, ToolUpdate};

    fn run<F: std::future::Future>(future: F) -> F::Output {
        asupersync::runtime::RuntimeBuilder::current_thread()
            .blocking_threads(1, 8)
            .build()
            .unwrap()
            .block_on(future)
    }

    fn text(output: &ToolOutput) -> String {
        output
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn records(path: &Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn console(records: &[Value], name: &str) -> String {
        records
            .iter()
            .filter(|record| record["name"].as_str() == Some(name))
            .map(|record| record["text"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn spool_preserves_frame_order_unicode_and_a_digest_of_the_published_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let mut archive =
            Archive::prepare_sync(dir.path(), "run.jsonl", "js", AgentCx::for_testing()).unwrap();
        archive
            .record(&json!({"sequence":1,"name":"stdout","text":"a\n界\0"}))
            .unwrap();
        archive
            .record(&json!({"sequence":2,"name":"stderr","text":"warning"}))
            .unwrap();
        assert!(!dir.path().join("run.jsonl").exists());
        let receipt = archive.publish_sync(false).unwrap();
        let bytes = std::fs::read(dir.path().join("run.jsonl")).unwrap();
        assert_eq!(receipt["bytes"], bytes.len());
        assert_eq!(
            receipt["sha256"],
            crate::package_manager::hex_encode(&Sha256::digest(&bytes))
        );
        assert_eq!(receipt["complete"], true);
        let rows = records(&dir.path().join("run.jsonl"));
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0]["schema"], SCHEMA);
        assert_eq!(rows[0]["kernel"], "js");
        assert_eq!(rows[1]["sequence"], 1);
        assert_eq!(rows[2]["sequence"], 2);
        assert_eq!(console(&rows, "stdout"), "a\n界\0");
        assert_eq!(rows[3]["stdoutBytes"], 6);
        assert_eq!(rows[3]["stderrBytes"], 7);
        assert_eq!(rows[3]["status"], "success");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(dir.path().join("run.jsonl"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn archive_limit_is_sticky_and_cannot_publish_a_successful_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let mut archive =
            Archive::prepare_sync(dir.path(), "run.jsonl", "python", AgentCx::for_testing())
                .unwrap();
        archive.limit = archive.bytes + END_RESERVE + 10;
        let failure = archive
            .record(&json!({"sequence":1,"name":"stdout","text":"too much"}))
            .unwrap_err();
        assert!(failure.to_string().contains("EVAL_ARTIFACT_LIMIT"));
        assert!(
            archive
                .publish_sync(false)
                .unwrap_err()
                .to_string()
                .contains("EVAL_ARTIFACT_INCOMPLETE")
        );
        assert!(!dir.path().join("run.jsonl").exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn cancelled_archive_owner_never_publishes_a_complete_file() {
        let dir = tempfile::tempdir().unwrap();
        let owner = AgentCx::for_testing();
        let mut archive =
            Archive::prepare_sync(dir.path(), "run.jsonl", "js", owner.clone()).unwrap();
        archive
            .record(&json!({"sequence":1,"name":"stdout","text":"progress"}))
            .unwrap();
        owner.set_cancel_requested(true);
        assert!(
            archive
                .publish_sync(false)
                .unwrap_err()
                .to_string()
                .contains("EVAL_CANCELLED")
        );
        assert!(!dir.path().join("run.jsonl").exists());
    }

    #[test]
    fn quiet_cell_archive_has_an_explicit_zero_frame_completion() {
        let dir = tempfile::tempdir().unwrap();
        let archive =
            Archive::prepare_sync(dir.path(), "quiet.jsonl", "js", AgentCx::for_testing()).unwrap();
        let receipt = archive.publish_sync(false).unwrap();
        let rows = records(&dir.path().join("quiet.jsonl"));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["kind"], "start");
        assert_eq!(
            rows[1],
            json!({"kind":"end", "sequence":0, "status":"success",
            "stdoutBytes":0, "stderrBytes":0, "resultIncluded":false})
        );
        assert_eq!(receipt["complete"], true);
        assert_eq!(
            receipt["bytes"],
            std::fs::metadata(dir.path().join("quiet.jsonl"))
                .unwrap()
                .len()
        );
    }

    #[test]
    fn cancellation_during_finalization_preserves_completed_result_but_refuses_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let owner = AgentCx::for_testing();
        let archive =
            Archive::prepare_sync(dir.path(), "cancelled.jsonl", "js", owner.clone()).unwrap();
        let mut capture = super::super::Capture::new("js", None);
        capture.attach_archive(Some(archive));
        capture
            .accept(
                &json!({"id":1, "stream":{"sequence":1,"name":"stdout","text":"finished"}}),
                1,
            )
            .unwrap();
        let mut output = ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new("42\n"))],
            details: Some(json!({"kernel":"js", "cell":1})),
            is_error: false,
        };
        owner.set_cancel_requested(true);
        run(capture.finish_archive(&mut output, false));
        assert!(output.is_error);
        assert!(text(&output).starts_with("42\n"));
        assert!(text(&output).contains("EVAL_CANCELLED"));
        let receipt = &output.details.as_ref().unwrap()["outputArtifact"];
        assert_eq!(receipt["status"], "failed");
        assert_eq!(receipt["executionFailed"], false);
        assert_eq!(receipt["complete"], false);
        assert!(!dir.path().join("cancelled.jsonl").exists());
    }

    #[test]
    fn javascript_archive_contains_the_console_middle_omitted_from_the_result() {
        let dir = tempfile::tempdir().unwrap();
        let tool = EvalTool::new(dir.path());
        let result = run(tool.execute(
            "archive",
            json!({
                "kernel":"js", "output_path":"logs/run.jsonl",
                "code":"console.log('BEGIN-' + '界'.repeat(500000) + '-END');"
            }),
            None,
        ))
        .unwrap();
        assert!(!result.is_error, "{}", text(&result));
        assert!(text(&result).contains("EVAL_OUTPUT_TRUNCATED"));
        assert!(text(&result).len() < 512 * 1024 + 1024);
        let receipt = &result.details.as_ref().unwrap()["outputArtifact"];
        assert_eq!(receipt["status"], "written");
        let path = Path::new(receipt["path"].as_str().unwrap());
        let rows = records(path);
        assert_eq!(
            console(&rows, "stdout"),
            format!("BEGIN-{}-END\n", "界".repeat(500000))
        );
        assert_eq!(rows.last().unwrap()["stdoutBytes"], 1_500_011);
        assert_eq!(rows.last().unwrap()["status"], "success");
        assert_eq!(
            receipt["sha256"],
            crate::package_manager::hex_encode(&Sha256::digest(std::fs::read(path).unwrap()))
        );
    }

    #[test]
    fn python_uses_the_same_console_archive_and_keeps_error_status() {
        if !std::process::Command::new("python3")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
        {
            eprintln!("Skipping: python3 unavailable");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let tool = EvalTool::new(dir.path());
        let result = run(tool.execute("archive", json!({
            "output_path":"python.jsonl",
            "code":"import sys\nprint('out')\nprint('err', file=sys.stderr)\nkeep = 42\nraise ValueError('expected')"
        }), None)).unwrap();
        assert!(result.is_error);
        assert!(text(&result).contains("ValueError: expected"));
        assert_eq!(
            result.details.as_ref().unwrap()["outputArtifact"]["status"],
            "written"
        );
        let rows = records(&dir.path().join("python.jsonl"));
        assert_eq!(console(&rows, "stdout"), "out\n");
        assert_eq!(console(&rows, "stderr"), "err\n");
        assert_eq!(rows.last().unwrap()["status"], "error");
        let next = run(tool.execute("state", json!({"code":"keep"}), None)).unwrap();
        assert_eq!(text(&next).trim(), "42");
        assert!(
            next.details
                .as_ref()
                .unwrap()
                .get("outputArtifact")
                .is_none()
        );
    }

    #[test]
    fn invalid_archive_destination_precedes_reset_and_execution() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("existing.jsonl"), "keep-existing").unwrap();
        let tool = EvalTool::new(dir.path());
        run(tool.execute(
            "seed",
            json!({"kernel":"js","code":"globalThis.keep = 42"}),
            None,
        ))
        .unwrap();
        for path in [
            Value::Null,
            json!(true),
            json!("../escape.jsonl"),
            json!("existing.jsonl"),
            json!("plain.txt"),
        ] {
            let failure = run(tool.execute(
                "bad",
                json!({
                    "kernel":"js", "code":"globalThis.keep = 99", "reset":true, "output_path":path
                }),
                None,
            ))
            .unwrap_err();
            assert!(
                failure.to_string().contains("EVAL_ARTIFACT_PATH"),
                "{failure}"
            );
            let next =
                run(tool.execute("state", json!({"kernel":"js","code":"keep"}), None)).unwrap();
            assert_eq!(text(&next).trim(), "42");
        }
        assert_eq!(
            std::fs::read(dir.path().join("existing.jsonl")).unwrap(),
            b"keep-existing"
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn destination_race_fails_the_requested_archive_without_reexecuting_or_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raced.jsonl");
        let raced_path = path.clone();
        let tool = EvalTool::new(dir.path());
        let result = run(tool.execute("race", json!({
            "kernel":"js", "output_path":"raced.jsonl",
            "code":"globalThis.keep = 7; console.log('progress'); await sleep(100); globalThis.keep += 1;"
        }), Some(Box::new(move |_: ToolUpdate| { std::fs::write(&raced_path, "other-writer").unwrap(); })))).unwrap();
        assert!(result.is_error);
        let archive = &result.details.as_ref().unwrap()["outputArtifact"];
        assert_eq!(archive["status"], "failed");
        assert_eq!(archive["complete"], false);
        assert_eq!(archive["executionFailed"], false);
        assert_eq!(std::fs::read(path).unwrap(), b"other-writer");
        let next = run(tool.execute("state", json!({"kernel":"js","code":"keep"}), None)).unwrap();
        assert_eq!(text(&next).trim(), "8");
    }

    #[test]
    fn interrupted_eval_does_not_publish_a_partial_archive_as_complete() {
        let dir = tempfile::tempdir().unwrap();
        let tool = EvalTool::new(dir.path());
        let owner = AgentCx::for_testing();
        let stop = owner.clone();
        let result = run(owner.with_current(tool.execute(
            "cancel",
            json!({
                "kernel":"js", "output_path":"cancelled.jsonl", "timeout_secs":60,
                "code":"console.log('cancel-progress'); await sleep(60000);"
            }),
            Some(Box::new(move |_: ToolUpdate| {
                stop.set_cancel_requested(true);
            })),
        )))
        .unwrap();
        assert!(result.is_error);
        assert!(text(&result).contains("EVAL_CANCELLED"));
        assert!(text(&result).contains("cancel-progress"));
        assert_eq!(
            result.details.as_ref().unwrap()["outputArtifact"]["status"],
            "not_published"
        );
        assert!(!dir.path().join("cancelled.jsonl").exists());
    }
}
