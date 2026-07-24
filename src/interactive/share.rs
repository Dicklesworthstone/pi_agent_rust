use asupersync::sync::OwnedMutexGuard;
use chrono::Utc;
use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;
use url::Url;

use super::{AgentState, Cmd, PiApp, PiMsg};

#[cfg(feature = "clipboard")]
use arboard::Clipboard as ArboardClipboard;

pub(super) async fn run_command_output(
    program: &str,
    args: &[OsString],
    cwd: &Path,
    abort_signal: &crate::agent::AbortSignal,
) -> std::io::Result<std::process::Output> {
    use asupersync::time::{sleep, wall_now};
    use std::process::{Command, Stdio};
    use std::sync::mpsc as std_mpsc;
    use std::time::Duration;

    let mut child = Command::new(program);
    child
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::tools::isolate_command_process_group(&mut child);
    let child = child.spawn()?;
    let pid = child.id();

    let (tx, rx) = std_mpsc::channel();
    let _handle = std::thread::Builder::new()
        .name("share".into())
        .spawn(move || {
            let result = child.wait_with_output();
            let _ = tx.send(result);
        });

    let tick = Duration::from_millis(10);
    loop {
        if abort_signal.is_aborted() {
            crate::tools::kill_process_group_tree(Some(pid));
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "command aborted",
            ));
        }

        match rx.try_recv() {
            Ok(result) => return result,
            Err(std_mpsc::TryRecvError::Empty) => {
                sleep(wall_now(), tick).await;
            }
            Err(std_mpsc::TryRecvError::Disconnected) => {
                return Err(std::io::Error::other("command output channel disconnected"));
            }
        }
    }
}

pub(super) fn parse_gist_url_and_id(output: &str) -> Option<(String, String)> {
    for raw in output.split_whitespace() {
        let candidate_url = raw.trim_matches(|c: char| matches!(c, '"' | '\'' | ',' | ';'));
        let Ok(url) = Url::parse(candidate_url) else {
            continue;
        };
        let Some(host) = url.host_str() else {
            continue;
        };
        if host != "gist.github.com" {
            continue;
        }
        let Some(segments) = url.path_segments().map(|segments| {
            segments
                .filter(|segment| !segment.is_empty())
                .collect::<Vec<_>>()
        }) else {
            continue;
        };

        // Canonical gist links are exactly `/owner/<gist-id>`.
        // Avoid false positives like profile URLs (`/owner`).
        if segments.len() != 2 {
            continue;
        }

        let gist_id = segments[1];
        if gist_id.is_empty() {
            continue;
        }
        return Some((candidate_url.to_string(), gist_id.to_string()));
    }
    None
}

pub(super) fn format_command_output(output: &std::process::Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => "(no output)".to_string(),
        (false, true) => format!("stdout:\n{stdout}"),
        (true, false) => format!("stderr:\n{stderr}"),
        (false, false) => format!("stdout:\n{stdout}\n\nstderr:\n{stderr}"),
    }
}

/// Build a gist description from the optional session name and current time.
pub(super) fn share_gist_description(session_name: Option<&str>) -> String {
    session_name.map_or_else(
        || format!("Pi session {}", Utc::now().format("%Y-%m-%dT%H:%M:%SZ")),
        |name| format!("Pi session: {name}"),
    )
}

/// Check whether `/share` args request a public gist.
pub(super) fn parse_share_is_public(args: &str) -> bool {
    args.split_whitespace()
        .any(|w| w.eq_ignore_ascii_case("public"))
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;

    // ── parse_gist_url_and_id ───────────────────────────────────────────

    #[test]
    fn parse_gist_url_simple() {
        let (url, id) = parse_gist_url_and_id("https://gist.github.com/user/abc123def456").unwrap();
        assert_eq!(url, "https://gist.github.com/user/abc123def456");
        assert_eq!(id, "abc123def456");
    }

    #[test]
    fn parse_gist_url_from_gh_output() {
        let output = "- Creating gist...\nhttps://gist.github.com/octocat/12345abcde\n";
        let (url, id) = parse_gist_url_and_id(output).unwrap();
        assert_eq!(url, "https://gist.github.com/octocat/12345abcde");
        assert_eq!(id, "12345abcde");
    }

    #[test]
    fn parse_gist_url_ignores_non_gist_urls() {
        assert!(parse_gist_url_and_id("https://github.com/user/repo").is_none());
        assert!(parse_gist_url_and_id("https://example.com/gist").is_none());
    }

    #[test]
    fn parse_gist_url_empty_input() {
        assert!(parse_gist_url_and_id("").is_none());
    }

    #[test]
    fn parse_gist_url_no_urls() {
        assert!(parse_gist_url_and_id("just some plain text").is_none());
    }

    #[test]
    fn parse_gist_url_strips_quotes() {
        let (url, id) = parse_gist_url_and_id("\"https://gist.github.com/user/deadbeef\"").unwrap();
        assert_eq!(url, "https://gist.github.com/user/deadbeef");
        assert_eq!(id, "deadbeef");
    }

    #[test]
    fn parse_gist_url_trailing_punctuation() {
        let (_, id) =
            parse_gist_url_and_id("Created: https://gist.github.com/user/aaa111,").unwrap();
        assert_eq!(id, "aaa111");
    }

    #[test]
    fn parse_gist_url_ignores_profile_links() {
        assert!(parse_gist_url_and_id("https://gist.github.com/octocat").is_none());
        assert!(parse_gist_url_and_id("https://gist.github.com/octocat/").is_none());
    }

    #[test]
    fn parse_gist_url_ignores_non_canonical_paths() {
        assert!(parse_gist_url_and_id("https://gist.github.com/octocat/aaa111/raw").is_none());
    }

    // ── format_command_output ───────────────────────────────────────────

    #[test]
    fn format_output_both_empty() {
        let output = std::process::Output {
            status: std::process::ExitStatus::default(),
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        assert_eq!(format_command_output(&output), "(no output)");
    }

    #[test]
    fn format_output_only_stdout() {
        let output = std::process::Output {
            status: std::process::ExitStatus::default(),
            stdout: b"hello world".to_vec(),
            stderr: Vec::new(),
        };
        assert_eq!(format_command_output(&output), "stdout:\nhello world");
    }

    #[test]
    fn format_output_only_stderr() {
        let output = std::process::Output {
            status: std::process::ExitStatus::default(),
            stdout: Vec::new(),
            stderr: b"error msg".to_vec(),
        };
        assert_eq!(format_command_output(&output), "stderr:\nerror msg");
    }

    #[test]
    fn format_output_both_present() {
        let output = std::process::Output {
            status: std::process::ExitStatus::default(),
            stdout: b"out".to_vec(),
            stderr: b"err".to_vec(),
        };
        assert_eq!(
            format_command_output(&output),
            "stdout:\nout\n\nstderr:\nerr"
        );
    }

    #[test]
    fn format_output_trims_whitespace() {
        let output = std::process::Output {
            status: std::process::ExitStatus::default(),
            stdout: b"  trimmed  \n".to_vec(),
            stderr: Vec::new(),
        };
        assert_eq!(format_command_output(&output), "stdout:\ntrimmed");
    }

    // ── share_gist_description ──────────────────────────────────────────

    #[test]
    fn gist_description_with_name() {
        assert_eq!(
            share_gist_description(Some("my-session")),
            "Pi session: my-session"
        );
    }

    #[test]
    fn gist_description_without_name() {
        let desc = share_gist_description(None);
        assert!(desc.starts_with("Pi session "));
        assert!(desc.contains('T'));
    }

    // ── parse_share_is_public ───────────────────────────────────────────

    #[test]
    fn parse_share_is_public_true() {
        assert!(parse_share_is_public("public"));
        assert!(parse_share_is_public("PUBLIC"));
        assert!(parse_share_is_public("Public"));
    }

    #[test]
    fn parse_share_is_public_false() {
        assert!(!parse_share_is_public(""));
        assert!(!parse_share_is_public("private"));
    }

    #[test]
    fn parse_share_is_public_with_extra_args() {
        assert!(parse_share_is_public("some-flag public other"));
        assert!(!parse_share_is_public("some-flag other"));
    }
}

impl PiApp {
    #[allow(clippy::too_many_lines)]
    pub(super) fn handle_slash_share(&mut self, args: &str) -> Option<Cmd> {
        if self.agent_state != AgentState::Idle {
            self.status_message = Some("Cannot share while processing".to_string());
            return None;
        }

        let is_public = parse_share_is_public(args);

        self.agent_state = AgentState::Processing;
        self.status_message = Some("Sharing session... (Esc to cancel)".to_string());

        let (abort_handle, abort_signal) = crate::agent::AbortHandle::new();
        self.abort_handle = Some(abort_handle);

        let event_tx = self.event_tx.clone();
        let runtime_handle = self.runtime_handle.clone();
        let session = Arc::clone(&self.session);
        let cwd = self.cwd.clone();
        let gh_path_override = self.config.gh_path.clone();

        runtime_handle.spawn(async move {
            let gh = gh_path_override
                .as_ref()
                .filter(|value| !value.trim().is_empty())
                .cloned()
                .unwrap_or_else(|| "gh".to_string());

            let auth_args = vec![OsString::from("auth"), OsString::from("status")];
            match run_command_output(&gh, &auth_args, &cwd, &abort_signal).await {
                Ok(output) => {
                    if !output.status.success() {
                        let details = format_command_output(&output);
                        let message = format!(
                            "`gh` is not authenticated.\n\
                             Run `gh auth login` to authenticate, then retry `/share`.\n\n\
                             {details}"
                        );
                        let _ = crate::interactive::enqueue_pi_event(
                            &event_tx,
                            &asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request),
                            PiMsg::AgentError(message),
                        )
                        .await;
                        return;
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    let message = "GitHub CLI `gh` not found.\n\
                             Install it from https://cli.github.com, then run `gh auth login`."
                        .to_string();
                    let _ = crate::interactive::enqueue_pi_event(
                        &event_tx,
                        &asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request),
                        PiMsg::AgentError(message),
                    )
                    .await;
                    return;
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {
                    let _ = crate::interactive::enqueue_pi_event(
                        &event_tx,
                        &asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request),
                        PiMsg::System("Share cancelled".to_string()),
                    )
                    .await;
                    return;
                }
                Err(err) => {
                    let _ = crate::interactive::enqueue_pi_event(
                        &event_tx,
                        &asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request),
                        PiMsg::AgentError(format!("Failed to run `gh auth status`: {err}")),
                    )
                    .await;
                    return;
                }
            }

            if abort_signal.is_aborted() {
                let _ = crate::interactive::enqueue_pi_event(
                    &event_tx,
                    &asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request),
                    PiMsg::System("Share cancelled".to_string()),
                )
                .await;
                return;
            }

            let cx = asupersync::Cx::for_request();
            // Owned guard: `MutexGuard` is `!Send` (asupersync 0.3.9) and this
            // future is handed to `RuntimeHandle::spawn`, which requires `Send`.
            let (html, session_name) = match OwnedMutexGuard::lock(Arc::clone(&session), &cx).await
            {
                Ok(guard) => (guard.to_html(), guard.get_name()),
                Err(err) => {
                    let _ = crate::interactive::enqueue_pi_event(
                        &event_tx,
                        &cx,
                        PiMsg::AgentError(format!("Failed to lock session: {err}")),
                    )
                    .await;
                    return;
                }
            };

            if abort_signal.is_aborted() {
                let _ = crate::interactive::enqueue_pi_event(
                    &event_tx,
                    &asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request),
                    PiMsg::System("Share cancelled".to_string()),
                )
                .await;
                return;
            }

            let gist_desc = share_gist_description(session_name.as_deref());

            let temp_file = match tempfile::Builder::new()
                .prefix("pi-share-")
                .suffix(".html")
                .tempfile()
            {
                Ok(file) => file,
                Err(err) => {
                    let _ = crate::interactive::enqueue_pi_event(
                        &event_tx,
                        &asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request),
                        PiMsg::AgentError(format!("Failed to create temp file: {err}")),
                    )
                    .await;
                    return;
                }
            };
            let temp_path = temp_file.into_temp_path();
            if let Err(err) = asupersync::fs::write(&temp_path, html.as_bytes()).await {
                let _ = crate::interactive::enqueue_pi_event(
                    &event_tx,
                    &asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request),
                    PiMsg::AgentError(format!("Failed to write temp file: {err}")),
                )
                .await;
                return;
            }

            let gist_args = vec![
                OsString::from("gist"),
                OsString::from("create"),
                OsString::from(format!("--public={is_public}")),
                OsString::from("--desc"),
                OsString::from(&gist_desc),
                temp_path.as_os_str().to_os_string(),
            ];
            let output = match run_command_output(&gh, &gist_args, &cwd, &abort_signal).await {
                Ok(output) => output,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    let message = "GitHub CLI `gh` not found.\n\
                             Install it from https://cli.github.com, then run `gh auth login`."
                        .to_string();
                    let _ = crate::interactive::enqueue_pi_event(
                        &event_tx,
                        &asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request),
                        PiMsg::AgentError(message),
                    )
                    .await;
                    return;
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {
                    let _ = crate::interactive::enqueue_pi_event(
                        &event_tx,
                        &asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request),
                        PiMsg::System("Share cancelled".to_string()),
                    )
                    .await;
                    return;
                }
                Err(err) => {
                    let _ = crate::interactive::enqueue_pi_event(
                        &event_tx,
                        &asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request),
                        PiMsg::AgentError(format!("Failed to run `gh gist create`: {err}")),
                    )
                    .await;
                    return;
                }
            };

            if !output.status.success() {
                let details = format_command_output(&output);
                let _ = crate::interactive::enqueue_pi_event(
                    &event_tx,
                    &asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request),
                    PiMsg::AgentError(format!("`gh gist create` failed.\n\n{details}")),
                )
                .await;
                return;
            }

            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let Some((gist_url, gist_id)) = parse_gist_url_and_id(&stdout) else {
                let details = format_command_output(&output);
                let _ = crate::interactive::enqueue_pi_event(
                    &event_tx,
                    &asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request),
                    PiMsg::AgentError(format!(
                        "Failed to parse gist URL from `gh gist create` output.\n\n{details}"
                    )),
                )
                .await;
                return;
            };

            let share_url = crate::session::get_share_viewer_url(&gist_id);
            drop(temp_path);

            // Copy viewer URL to clipboard (best-effort).
            #[cfg(feature = "clipboard")]
            {
                if let Ok(mut clipboard) = ArboardClipboard::new() {
                    let _ = clipboard.set_text(share_url.clone());
                }
            }

            let privacy = if is_public { "public" } else { "private" };
            let message =
                format!("Created {privacy} gist\nShare URL: {share_url}\nGist: {gist_url}");
            let _ = crate::interactive::enqueue_pi_event(
                &event_tx,
                &asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request),
                PiMsg::System(message),
            )
            .await;
        });
        None
    }
}
