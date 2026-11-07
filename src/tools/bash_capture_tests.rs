//! Foreground capture integrity, independently of how much output is displayed.

use super::*;

async fn writable_spill_state(path: &Path, prefix: &[u8]) -> BashOutputState {
    std::fs::write(path, prefix).expect("create retained artifact");
    let file = asupersync::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .await
        .expect("open retained artifact");
    let mut state = BashOutputState::new(DEFAULT_MAX_BYTES * 2);
    state.temp_file_path = Some(path.to_path_buf());
    state.temp_file = Some(file);
    state
}

fn progress_details(state: &BashOutputState) -> serde_json::Value {
    let observed = std::sync::Mutex::new(None);
    emit_bash_update(
        state,
        Some(&|update| {
            *observed.lock().expect("progress lock") = update.details;
        }),
    )
    .expect("emit progress");
    observed
        .into_inner()
        .expect("progress lock")
        .expect("progress details")
}

#[test]
fn hard_limit_retains_partial_artifact_without_claiming_full_output() {
    asupersync::test_utils::run_test(|| async {
        let root = tempfile::tempdir().expect("test directory");
        let path = root.path().join("capped-output.log");
        let prefix = b"retained prefix\n";
        let mut state = writable_spill_state(&path, prefix).await;
        // Exercise the real cap branch using a small artifact instead of
        // producing a gigabyte of output merely to advance this counter.
        state.total_bytes = BASH_FILE_LIMIT_BYTES;
        ingest_bash_chunk(b"final captured output\n".to_vec(), &mut state)
            .await
            .expect("ingest beyond the artifact cap");

        assert!(state.temp_file.is_none(), "the capped writer must close");
        assert_eq!(std::fs::read(&path).expect("retained prefix"), prefix);
        let progress = progress_details(&state);
        assert!(progress.get("fullOutputPath").is_none());
        assert_eq!(progress["capture"]["outputComplete"], true);
        assert_eq!(progress["capture"]["artifactTruncated"], true);
        assert_eq!(
            progress["capture"]["artifactPath"],
            path.display().to_string()
        );

        let result = assemble_bash_run_result(state, Some(0), false, false, None, None);
        let capture = result.capture.as_ref().expect("cap metadata");
        assert!(capture.output_complete);
        assert!(capture.artifact_truncated);
        assert!(capture.artifact_error.is_none());
        assert_eq!(
            capture.artifact_path.as_deref(),
            path.to_str(),
            "the retained prefix must remain discoverable"
        );
        assert!(result.full_output_path.is_none());
        assert!(result.output.contains("final captured output"));
        assert!(result.output.contains("Partial output"));
        assert!(!result.output.contains("Full output:"));
        assert!(result.output.contains(&path.display().to_string()));

        let output = BashTool::new(root.path())
            .bash_result_output("capped-output", result)
            .expect("tool result");
        assert!(
            !output.is_error,
            "bounded retention must preserve a successful process exit"
        );
        let details = output.details.expect("capture details");
        assert!(details.get("fullOutputPath").is_none());
        assert_eq!(details["capture"]["outputComplete"], true);
        assert_eq!(details["capture"]["artifactTruncated"], true);
    });
}

#[test]
fn complete_display_truncation_keeps_full_artifact_without_capture_warning() {
    asupersync::test_utils::run_test(|| async {
        let root = tempfile::tempdir().expect("test directory");
        let path = root.path().join("complete-output.log");
        let mut state = writable_spill_state(&path, b"").await;
        let bytes = vec![b'x'; DEFAULT_MAX_BYTES + 17];
        ingest_bash_chunk(bytes.clone(), &mut state)
            .await
            .expect("ingest complete output");

        let progress = progress_details(&state);
        assert!(progress.get("capture").is_none());
        assert_eq!(progress["fullOutputPath"], path.display().to_string());

        let result = assemble_bash_run_result(state, Some(0), false, false, None, None);
        assert_eq!(std::fs::read(&path).expect("complete artifact"), bytes);
        assert!(result.truncated, "the display is deliberately bounded");
        assert_eq!(result.full_output_path.as_deref(), path.to_str());
        assert!(result.capture.is_none(), "capture itself was complete");
        assert!(result.output.contains("Full output:"));
        assert!(!result.output.contains("Command capture:"));

        let output = BashTool::new(root.path())
            .bash_result_output("complete-output", result)
            .expect("tool result");
        assert!(!output.is_error);
        let details = output.details.expect("truncation details");
        assert_eq!(details["fullOutputPath"], path.display().to_string());
        assert!(details.get("capture").is_none());
    });
}

#[test]
fn failed_spill_preserves_output_and_reports_artifact_error() {
    asupersync::test_utils::run_test(|| async {
        let root = tempfile::tempdir().expect("test directory");
        let path = root.path().join("failed-output.log");
        std::fs::write(&path, b"partial prefix\n").expect("create artifact");
        // A read-only descriptor gives the production write path an actual
        // I/O failure without special devices or platform-specific fixtures.
        let read_only = asupersync::fs::OpenOptions::new()
            .read(true)
            .open(&path)
            .await
            .expect("open read-only artifact");
        let mut state = BashOutputState::new(DEFAULT_MAX_BYTES * 2);
        state.temp_file_path = Some(path);
        state.temp_file = Some(read_only);
        let mut bytes = vec![b'x'; DEFAULT_MAX_BYTES + 17];
        bytes.extend_from_slice(b"\nretained final output\n");
        ingest_bash_chunk(bytes, &mut state)
            .await
            .expect("a spill failure must preserve captured output");

        let progress = progress_details(&state);
        assert!(progress.get("fullOutputPath").is_none());
        assert_eq!(progress["capture"]["outputComplete"], true);
        assert!(
            progress["capture"]["artifactError"]
                .as_str()
                .is_some_and(|message| !message.is_empty())
        );
        assert!(progress["capture"]["artifactPath"].is_null());

        let result = assemble_bash_run_result(state, Some(0), false, false, None, None);
        let capture = result.capture.as_ref().expect("spill failure metadata");
        assert!(capture.output_complete, "both streams can still reach EOF");
        assert!(
            capture
                .artifact_error
                .as_deref()
                .is_some_and(|message| !message.is_empty())
        );
        assert!(capture.artifact_path.is_none());
        assert!(result.full_output_path.is_none());
        assert!(result.output.contains("retained final output"));
        assert!(result.output.contains("Command capture:"));
        assert!(!result.output.contains("Full output:"));

        let output = BashTool::new(root.path())
            .bash_result_output("failed-artifact", result)
            .expect("tool result");
        assert!(
            !output.is_error,
            "artifact retention failure must not change the process exit"
        );
        let details = output.details.expect("capture details");
        assert!(details.get("fullOutputPath").is_none());
        assert!(details["capture"]["artifactError"].is_string());
    });
}

#[test]
fn incomplete_drain_never_advertises_retained_artifact_as_full() {
    asupersync::test_utils::run_test(|| async {
        let root = tempfile::tempdir().expect("test directory");
        let path = root.path().join("undrained-output.log");
        let mut state = writable_spill_state(&path, b"").await;
        ingest_bash_chunk(vec![b'x'; DEFAULT_MAX_BYTES + 17], &mut state)
            .await
            .expect("ingest captured prefix");
        state.output_complete = false;

        let progress = progress_details(&state);
        assert!(progress.get("fullOutputPath").is_none());
        assert_eq!(progress["capture"]["outputComplete"], false);
        assert_eq!(progress["capture"]["artifactTruncated"], false);
        assert_eq!(
            progress["capture"]["artifactPath"],
            path.display().to_string()
        );

        let result = assemble_bash_run_result(state, Some(0), false, false, None, None);
        let capture = result.capture.as_ref().expect("drain metadata");
        assert!(!capture.output_complete);
        assert!(!capture.artifact_truncated, "the artifact cap was not hit");
        assert!(capture.artifact_error.is_none());
        assert_eq!(capture.artifact_path.as_deref(), path.to_str());
        assert!(result.full_output_path.is_none());
        assert!(result.output.contains("Partial output"));
        assert!(result.output.contains("Command capture:"));
        assert!(!result.output.contains("Full output:"));

        let output = BashTool::new(root.path())
            .bash_result_output("undrained-output", result)
            .expect("tool result");
        assert!(output.is_error, "incomplete streams cannot claim success");
        let details = output.details.expect("capture details");
        assert!(details.get("fullOutputPath").is_none());
        assert_eq!(details["capture"]["outputComplete"], false);
    });
}
