//! Explicit hosted-review targets and bounded, noninteractive forge reads.
//!
//! The CLI owns credentials. Pi passes a numeric request ID and an explicit
//! repository as separate arguments, never checks out remote code, and never
//! lets a pasted hostname retarget an ambient credential to another service.

use super::MAX_REVIEW_DIFF_BYTES;
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::tools::{
    ProcessCleanupMode, ProcessGuard, command_with_default_sigpipe_in_dir,
    isolate_command_process_group,
};
use std::io::Read as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const FETCH_TIMEOUT: Duration = Duration::from_secs(60);
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const GITLAB_TOKENS: [&str; 5] = [
    "GITLAB_TOKEN",
    "GITLAB_ACCESS_TOKEN",
    "OAUTH_TOKEN",
    "JOB_TOKEN",
    "CI_JOB_TOKEN",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Forge {
    Github,
    Gitlab,
}

impl Forge {
    const fn binary(self) -> &'static str {
        match self {
            Self::Github => "gh",
            Self::Gitlab => "glab",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Target {
    forge: Forge,
    host: String,
    project: String,
    number: u64,
}

fn invalid_target() -> Error {
    Error::validation(
        "PI_REVIEW_TARGET: use an explicit HTTPS GitHub pull-request URL or GitLab merge-request URL; credentials, queries, encoded paths and ambiguous path segments are not accepted",
    )
}

fn authority(url: &url::Url) -> Option<String> {
    let url::Host::Domain(host) = url.host()? else {
        return None;
    };
    if host.len() > 253
        || !host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && label
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && label
                    .as_bytes()
                    .last()
                    .is_some_and(u8::is_ascii_alphanumeric)
        })
    {
        return None;
    }
    Some(
        url.port()
            .map_or_else(|| host.to_string(), |port| format!("{host}:{port}")),
    )
}

fn binding(value: &str) -> Option<String> {
    let value = if value.contains("://") {
        value.to_string()
    } else {
        format!("https://{value}")
    };
    let url = url::Url::parse(&value).ok()?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || value
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || matches!(byte, b'\\' | b'%'))
    {
        return None;
    }
    authority(&url)
}

// A leading dot is a valid repository name (`.github`); a leading dash is not
// and could read as an option.
fn valid_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= 255
        && !matches!(segment, "." | "..")
        && segment
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'_' | b'.'))
        && segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

impl Target {
    pub(super) fn parse(raw: &str) -> Result<Option<Self>> {
        if !raw.contains("://") {
            return Ok(None);
        }
        if raw.len() > 4096
            || raw.chars().any(|ch| ch.is_whitespace() || ch.is_control())
            || raw.contains(['\\', '%'])
            || raw.split('/').any(|part| matches!(part, "." | ".."))
        {
            return Err(invalid_target());
        }
        let url = url::Url::parse(raw).map_err(|_| invalid_target())?;
        if url.scheme() != "https"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
        {
            return Err(invalid_target());
        }
        let host = authority(&url).ok_or_else(invalid_target)?;
        let path = url.path().strip_prefix('/').ok_or_else(invalid_target)?;
        let path = path.strip_suffix('/').unwrap_or(path);
        let parts: Vec<&str> = path.split('/').collect();
        let (forge, project, number) = if matches!(
            parts.as_slice(),
            [_, _, "pull", _] | [_, _, "pull", _, "files" | "changes"]
        ) && host != "gitlab.com"
        {
            (Forge::Github, &parts[..2], parts[3])
        } else if let Some(index) = parts.iter().position(|part| *part == "-") {
            if index < 2
                || parts.get(index + 1) != Some(&"merge_requests")
                || !(parts.len() == index + 3
                    || (parts.len() == index + 4 && parts.last() == Some(&"diffs")))
                || host == "github.com"
            {
                return Err(invalid_target());
            }
            (Forge::Gitlab, &parts[..index], parts[index + 2])
        } else {
            return Err(invalid_target());
        };
        if !project.iter().all(|segment| valid_segment(segment))
            || number.is_empty()
            || !number.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(invalid_target());
        }
        let number = number
            .parse::<u64>()
            .ok()
            .filter(|number| *number > 0)
            .ok_or_else(invalid_target)?;
        Ok(Some(Self {
            forge,
            host,
            project: project.join("/"),
            number,
        }))
    }

    pub(super) fn url(&self) -> String {
        let kind = match self.forge {
            Forge::Github => "pull",
            Forge::Gitlab => "-/merge_requests",
        };
        format!(
            "https://{}/{}/{kind}/{}",
            self.host, self.project, self.number
        )
    }

    #[allow(clippy::too_many_lines)] // one isolated CLI environment per supported forge
    fn configure(
        &self,
        command: &mut Command,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<()> {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        command
            .env("PAGER", "")
            .env("NO_COLOR", "1")
            .env("GIT_TERMINAL_PROMPT", "0");
        match self.forge {
            Forge::Github => {
                let configured = lookup("GH_HOST").filter(|value| !value.is_empty());
                let token_host = configured
                    .as_deref()
                    .map_or_else(|| Some("github.com".to_string()), binding);
                if self.host != "github.com" && token_host.as_deref() != Some(&self.host) {
                    return Err(Error::validation(
                        "PI_REVIEW_HOST: self-hosted GitHub review requires GH_HOST to match the URL host",
                    ));
                }
                command
                    .args([
                        "pr",
                        "diff",
                        &self.number.to_string(),
                        "--color=never",
                        "--repo",
                    ])
                    .arg(format!("{}/{}", self.host, self.project))
                    .env("GH_HOST", &self.host)
                    .env_remove("GH_REPO")
                    .env_remove("GH_DEBUG")
                    .env("GH_PAGER", "")
                    .env("GH_PROMPT_DISABLED", "1")
                    .env("GH_NO_UPDATE_NOTIFIER", "1")
                    .env("GH_NO_EXTENSION_UPDATE_NOTIFIER", "1");
                if self.host == "github.com" {
                    command
                        .env_remove("GH_ENTERPRISE_TOKEN")
                        .env_remove("GITHUB_ENTERPRISE_TOKEN");
                }
                // gh uses GH_TOKEN for both github.com and *.ghe.com. Keep it
                // only when its original host binding matches the target.
                if token_host.as_deref() != Some(&self.host)
                    || (self.host != "github.com" && !self.host.ends_with(".ghe.com"))
                {
                    command.env_remove("GH_TOKEN").env_remove("GITHUB_TOKEN");
                }
            }
            Forge::Gitlab => {
                let configured = ["GITLAB_HOST", "GITLAB_URI", "GL_HOST"]
                    .into_iter()
                    .find_map(|name| lookup(name).filter(|value| !value.is_empty()));
                let token_host = configured
                    .as_deref()
                    .map_or_else(|| Some("gitlab.com".to_string()), binding);
                if self.host != "gitlab.com" && token_host.as_deref() != Some(&self.host) {
                    return Err(Error::validation(
                        "PI_REVIEW_HOST: self-hosted GitLab review requires GITLAB_HOST, GITLAB_URI or GL_HOST to match the URL host",
                    ));
                }
                command
                    .args([
                        "mr",
                        "diff",
                        &self.number.to_string(),
                        "--raw",
                        "--color=never",
                        "--repo",
                    ])
                    .arg(format!("https://{}/{}", self.host, self.project))
                    .env("GITLAB_HOST", &self.host)
                    .env("GITLAB_API_HOST", &self.host)
                    .env("GLAB_API_PROTOCOL", "https")
                    .env("API_PROTOCOL", "https")
                    .env_remove("GITLAB_URI")
                    .env_remove("GL_HOST")
                    .env_remove("GITLAB_REPO")
                    .env_remove("GITLAB_SUBFOLDER")
                    .env("GLAB_ENABLE_CI_AUTOLOGIN", "false")
                    .env("GLAB_NO_PROMPT", "1")
                    .env("NO_PROMPT", "1")
                    .env("PROMPT_DISABLED", "1")
                    .env("GLAB_PAGER", "")
                    .env("GLAB_CHECK_UPDATE", "false")
                    .env("CHECK_UPDATE", "false")
                    .env("GLAB_DEBUG", "false")
                    .env("GLAB_DEBUG_HTTP", "false");
                let ci_host = lookup("CI_SERVER_FQDN").or_else(|| lookup("CI_SERVER_URL"));
                // glab sends an environment token to whichever host it targets.
                // Without an explicit binding, a CI job's token belongs to the
                // job's own server, not to gitlab.com.
                let token_owner = match (&configured, &ci_host) {
                    (None, Some(ci_host)) => binding(ci_host),
                    _ => token_host,
                };
                if token_owner.as_deref() != Some(&self.host) {
                    for key in GITLAB_TOKENS.into_iter().chain([
                        "GLAB_CLIENT_CERT",
                        "CLIENT_CERT",
                        "GLAB_CLIENT_KEY",
                        "CLIENT_KEY",
                    ]) {
                        command.env_remove(key);
                    }
                }
                if ci_host
                    .as_deref()
                    .is_some_and(|value| binding(value).as_deref() != Some(&self.host))
                {
                    command.env_remove("JOB_TOKEN").env_remove("CI_JOB_TOKEN");
                }
            }
        }
        Ok(())
    }

    pub(super) fn acquire(&self, cwd: &Path) -> Result<String> {
        let mut command = command_with_default_sigpipe_in_dir(self.forge.binary(), cwd)?;
        command.current_dir(cwd);
        self.configure(&mut command, |name| std::env::var(name).ok())?;
        let diff = run(&mut command, self.forge.binary(), FETCH_TIMEOUT)?;
        validate_diff(&diff)?;
        Ok(diff)
    }
}

struct OwnedProcess(ProcessGuard);

impl Drop for OwnedProcess {
    fn drop(&mut self) {
        let _ = self.0.terminate_and_wait();
    }
}

fn run(command: &mut Command, backend: &str, timeout: Duration) -> Result<String> {
    let owner = AgentCx::for_current_or_request();
    owner.checkpoint().map_err(|_| Error::Aborted)?;
    isolate_command_process_group(command);
    let mut child = OwnedProcess(ProcessGuard::new(
        owner.process().spawn_checked(command).map_err(|error| Error::validation(format!(
            "PI_REVIEW_FORGE: cannot start {backend}: {error}; install the forge CLI and sign in before reviewing",
        )))?,
        ProcessCleanupMode::ProcessGroupTree,
    ));
    let stdout = child
        .0
        .take_stdout()
        .ok_or_else(|| Error::validation("PI_REVIEW_FORGE: stdout unavailable"))?;
    let (tx, rx) = mpsc::sync_channel(1);
    let reader = std::thread::Builder::new()
        .name("pi-review-diff".to_string())
        .spawn(move || {
            let mut bytes = Vec::new();
            let result = stdout
                .take((MAX_REVIEW_DIFF_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
                .map(|_| bytes);
            let _ = tx.send(result);
        })?;
    let started = Instant::now();
    let mut captured = None;
    let mut status = None;
    loop {
        owner.checkpoint().map_err(|_| Error::Aborted)?;
        if captured.is_none() {
            match rx.try_recv() {
                Ok(result) => captured = Some(result?),
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(Error::validation(
                        "PI_REVIEW_FORGE: diff reader stopped before completing",
                    ));
                }
            }
        }
        if captured
            .as_ref()
            .is_some_and(|bytes| bytes.len() > MAX_REVIEW_DIFF_BYTES)
        {
            return Err(Error::validation(
                "PI_REVIEW_BUDGET: hosted diff exceeds 192 KiB; review a narrower local target",
            ));
        }
        if status.is_none() && child.0.try_wait_child()?.is_some() {
            status = Some(child.0.terminate_and_wait()?);
        }
        if captured.is_some()
            && let Some(status) = status
        {
            if !status.success() {
                return Err(Error::validation(format!(
                    "PI_REVIEW_FORGE: {backend} exited with {status}; verify login and target access (GitLab requires a glab version with mr diff --raw)",
                )));
            }
            break;
        }
        if started.elapsed() >= timeout {
            return Err(Error::validation(
                "PI_REVIEW_TIMEOUT: hosted diff acquisition exceeded its deadline",
            ));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    reader
        .join()
        .map_err(|_| Error::validation("PI_REVIEW_FORGE: diff reader panicked"))?;
    String::from_utf8(captured.unwrap_or_default())
        .map_err(|_| Error::validation("PI_REVIEW_DIFF: hosted diff is not UTF-8"))
}

fn invalid_diff() -> Error {
    Error::validation(
        "PI_REVIEW_DIFF: forge returned an empty, malformed or incomplete unified diff; no review verdict is available",
    )
}

fn hunk_range(value: &str, prefix: char) -> Option<(usize, usize)> {
    let value = value.strip_prefix(prefix)?;
    let (start, count) = value.split_once(',').unwrap_or((value, "1"));
    if start.is_empty()
        || count.is_empty()
        || !start
            .bytes()
            .chain(count.bytes())
            .all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let start = start.parse::<usize>().ok()?;
    let count = count.parse::<usize>().ok()?;
    if start == 0 && count != 0 {
        return None;
    }
    start.checked_add(count)?;
    Some((start, count))
}

fn metadata_index(value: &str) -> Option<(&str, &str)> {
    let mut fields = value.split_whitespace();
    let (old, new) = fields.next()?.split_once("..")?;
    if [old, new].iter().any(|hash| {
        !(7..=64).contains(&hash.len()) || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
    }) {
        return None;
    }
    if fields.next().is_some_and(|mode| !valid_mode(mode)) || fields.next().is_some() {
        return None;
    }
    Some((old, new))
}

fn valid_mode(value: &str) -> bool {
    value.len() == 6 && value.bytes().all(|byte| (b'0'..=b'7').contains(&byte))
}

/// A metadata-only record still needs a complete operation. A lone mode or
/// rename header may be a truncated text patch, even alongside valid hunks.
fn complete_metadata(lines: &[&str]) -> bool {
    const FIELDS: [&str; 10] = [
        "old mode ",
        "new mode ",
        "new file mode ",
        "deleted file mode ",
        "rename from ",
        "rename to ",
        "copy from ",
        "copy to ",
        "similarity index ",
        "index ",
    ];
    let field = |prefix: &str| lines.iter().find_map(|line| line.strip_prefix(prefix));
    let binary = |line: &str| {
        line.strip_prefix("Binary files ")
            .and_then(|body| body.strip_suffix(" differ"))
            .and_then(|body| body.split_once(" and "))
            .is_some_and(|(old, new)| !old.is_empty() && !new.is_empty())
    };
    if FIELDS.iter().any(|prefix| {
        lines
            .iter()
            .filter(|line| line.starts_with(*prefix))
            .count()
            > 1
    }) || lines.iter().any(|line| {
        !binary(line)
            && !FIELDS.iter().any(|prefix| {
                line.strip_prefix(*prefix)
                    .is_some_and(|value| !value.is_empty())
            })
    }) || lines.iter().filter(|line| binary(line)).count() > 1
    {
        return false;
    }
    for prefix in [
        "old mode ",
        "new mode ",
        "new file mode ",
        "deleted file mode ",
    ] {
        if field(prefix).is_some_and(|mode| !valid_mode(mode)) {
            return false;
        }
    }
    let mode_change = field("old mode ").is_some();
    let renamed = field("rename from ").is_some();
    let copied = field("copy from ").is_some();
    let created = field("new file mode ").is_some();
    let deleted = field("deleted file mode ").is_some();
    if mode_change != field("new mode ").is_some()
        || renamed != field("rename to ").is_some()
        || copied != field("copy to ").is_some()
        || (renamed && copied)
        || (created && deleted)
        || ((created || deleted) && (mode_change || renamed || copied))
        || field("similarity index ").is_some() != (renamed || copied)
    {
        return false;
    }
    if field("similarity index ").is_some_and(|value| {
        value
            .strip_suffix('%')
            .and_then(|value| value.parse::<u8>().ok())
            .is_none_or(|value| value > 100)
    }) {
        return false;
    }
    let index = match field("index ") {
        Some(value) => match metadata_index(value) {
            Some(ids) => Some(ids),
            None => return false,
        },
        None => None,
    };
    if lines.iter().any(|line| binary(line)) {
        return true;
    }
    if created || deleted {
        let Some((old, new)) = index else {
            return false;
        };
        let empty_blob = |hash: &str| {
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391".starts_with(hash)
                || "473a0f4c3be8a93681a267e3b1e9a7dcda1185436fe141f7749120a303721813"
                    .starts_with(hash)
        };
        return if created {
            old.bytes().all(|byte| byte == b'0') && empty_blob(new)
        } else {
            empty_blob(old) && new.bytes().all(|byte| byte == b'0')
        };
    }
    (mode_change || renamed || copied)
        && index.is_none_or(|(old, new)| old == new)
        && (!(renamed || copied) || field("similarity index ") == Some("100%"))
}

/// The shared parser intentionally tolerates partial hand-authored patches.
/// A forge snapshot must instead prove every declared hunk is present, so an
/// HTML login page, truncated response or old glab summary cannot become SHIP.
fn validate_diff(diff: &str) -> Result<()> {
    if !diff.ends_with('\n') {
        return Err(invalid_diff());
    }
    let body = diff.strip_prefix("diff --git ").ok_or_else(invalid_diff)?;
    for chunk in body.split("\ndiff --git ") {
        let mut lines = chunk.lines();
        let header = lines.next().ok_or_else(invalid_diff)?;
        if !header.starts_with("a/") && !header.starts_with("\"a/") {
            return Err(invalid_diff());
        }
        let lines: Vec<&str> = lines.collect();
        if lines.is_empty() {
            return Err(invalid_diff());
        }
        let first_hunk = lines.iter().position(|line| line.starts_with("@@"));
        let Some(mut index) = first_hunk else {
            if !complete_metadata(&lines) {
                return Err(invalid_diff());
            }
            continue;
        };
        if !lines[..index].iter().any(|line| line.starts_with("--- "))
            || !lines[..index].iter().any(|line| line.starts_with("+++ "))
            || lines[..index]
                .iter()
                .any(|line| line.starts_with("Binary files ") || *line == "GIT binary patch")
        {
            return Err(invalid_diff());
        }
        while index < lines.len() {
            let header = lines[index].strip_prefix("@@ ").ok_or_else(invalid_diff)?;
            let (ranges, _) = header.split_once(" @@").ok_or_else(invalid_diff)?;
            let (old, new) = ranges.split_once(' ').ok_or_else(invalid_diff)?;
            let (_, mut old_left) = hunk_range(old, '-').ok_or_else(invalid_diff)?;
            let (_, mut new_left) = hunk_range(new, '+').ok_or_else(invalid_diff)?;
            if old_left == 0 && new_left == 0 {
                return Err(invalid_diff());
            }
            index += 1;
            while old_left > 0 || new_left > 0 {
                let line = lines.get(index).ok_or_else(invalid_diff)?;
                if *line == "\\ No newline at end of file" {
                    index += 1;
                    continue;
                }
                let (old, new) = match line.as_bytes().first() {
                    Some(b' ') => (1, 1),
                    Some(b'-') => (1, 0),
                    Some(b'+') => (0, 1),
                    _ => return Err(invalid_diff()),
                };
                old_left = old_left.checked_sub(old).ok_or_else(invalid_diff)?;
                new_left = new_left.checked_sub(new).ok_or_else(invalid_diff)?;
                index += 1;
            }
            if lines.get(index) == Some(&"\\ No newline at end of file") {
                index += 1;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\n";

    fn target(raw: &str) -> Result<Target> {
        Target::parse(raw)?.ok_or_else(|| Error::validation("fixture must be a hosted URL"))
    }

    #[test]
    fn hosted_target_accepts_nested_namespaces_and_normalizes_view_urls() -> Result<()> {
        assert_eq!(
            target("https://GITHUB.com/team/repo/pull/0012/files#diff-123")?.url(),
            "https://github.com/team/repo/pull/12"
        );
        assert_eq!(
            target("https://gitlab.com/group/sub-group/repo/-/merge_requests/9/diffs/")?.url(),
            "https://gitlab.com/group/sub-group/repo/-/merge_requests/9"
        );
        // GitHub's current "Files changed" tab, and an organization's
        // `.github` repository.
        assert_eq!(
            target("https://github.com/team/repo/pull/7/changes#diff-ab")?.url(),
            "https://github.com/team/repo/pull/7"
        );
        assert_eq!(
            target("https://github.com/team/.github/pull/3")?.url(),
            "https://github.com/team/.github/pull/3"
        );
        assert!(Target::parse("branch:main")?.is_none());
        assert!(Target::parse("HEAD~3..HEAD")?.is_none());
        Ok(())
    }

    #[test]
    fn hosted_target_rejects_ambiguous_or_unsafe_routes() {
        for raw in [
            "http://github.com/team/repo/pull/1",
            "https://user:password@github.com/team/repo/pull/1",
            "https://github.com/team/repo/pull/1?repo=other",
            "https://github.com/team/repo/../repo/pull/1",
            "https://github.com/team%2Frepo/pull/1",
            "https://github.com/team//repo/pull/1",
            "https://github.com/team/-repo/pull/1",
            "https://github.com/team/repo/pull/1/commits",
            "https://github.com/team/repo/pull/0",
            "https://github.com/team/repo/pull/-1",
            "https://github.com/team/repo/pull/18446744073709551616",
            "https://gitlab.com/team/repo/pull/1",
            "https://github.com/team/repo/-/merge_requests/1",
            "https://127.0.0.1/team/repo/pull/1",
            "https://github.com/team/repo/pull/1\n",
            "mr://team/repo/1",
        ] {
            assert!(Target::parse(raw).is_err(), "{raw:?}");
        }
    }

    #[test]
    fn hosted_custom_hosts_need_an_existing_exact_host_binding() -> Result<()> {
        let github = target("https://code.example/team/repo/pull/1")?;
        let mut command = Command::new("gh");
        assert!(github.configure(&mut command, |_| None).is_err());
        assert!(
            github
                .configure(&mut command, |_| Some("other.example".to_string()))
                .is_err()
        );
        github.configure(&mut command, |key| {
            (key == "GH_HOST").then(|| "https://code.example".to_string())
        })?;
        let gitlab = target("https://code.example:8443/team/sub/repo/-/merge_requests/2")?;
        let mut command = Command::new("glab");
        gitlab.configure(&mut command, |key| {
            (key == "GITLAB_URI").then(|| "https://code.example:8443".to_string())
        })?;
        assert!(
            gitlab
                .configure(&mut command, |key| {
                    (key == "GITLAB_HOST").then(|| "code.example".to_string())
                })
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn hosted_diff_refuses_incomplete_hunks_and_non_diff_responses() {
        assert!(validate_diff(DIFF).is_ok());
        assert!(
            validate_diff(&format!(
                "{DIFF}diff --git a/blob b/blob\nBinary files a/blob and b/blob differ\n"
            ))
            .is_ok()
        );
        assert!(validate_diff("diff --git a/file b/file\n--- a/file\n+++ b/file\n@@ -0,0 +1 @@\n+new\n\\ No newline at end of file\n").is_ok());
        for bad in [
            "",
            "<html>sign in</html>\n",
            "1 file changed, 1 insertion(+)\n",
            "diff --git a/file b/file\n",
            "diff --git a/file b/file\n--- a/file\n+++ b/file\n@@ -1,2 +1,2 @@\n-old\n+new\n",
            "diff --git a/file b/file\n--- a/file\n+++ b/file\n@@ -1 +1 @@\n-old\n+new\n+unexpected\n",
            "diff --git a/file b/file\n--- a/file\n+++ b/file\n@@ -0 +1 @@\n-old\n+new\n",
        ] {
            assert!(validate_diff(bad).is_err(), "{bad:?}");
        }
        assert!(validate_diff(DIFF.trim_end_matches('\n')).is_err());
    }

    #[test]
    fn hosted_metadata_requires_complete_operations_even_beside_valid_text() {
        for metadata in [
            "diff --git a/file b/file\nold mode 100644\nnew mode 100755\n",
            "diff --git a/old b/new\nsimilarity index 100%\nrename from old\nrename to new\n",
            "diff --git a/old b/new\nsimilarity index 100%\ncopy from old\ncopy to new\n",
            "diff --git a/empty b/empty\nnew file mode 100644\nindex 0000000..e69de29\n",
            "diff --git a/empty b/empty\ndeleted file mode 100644\nindex e69de29..0000000\n",
        ] {
            assert!(validate_diff(metadata).is_ok(), "{metadata:?}");
            assert!(
                validate_diff(&format!("{DIFF}{metadata}")).is_ok(),
                "{metadata:?}"
            );
        }
        for truncated in [
            "diff --git a/file b/file\nold mode 100644\n",
            "diff --git a/file b/file\nnew file mode 100644\n",
            "diff --git a/file b/file\nnew file mode 100644\nindex 0000000..1234567\n",
            "diff --git a/file b/file\ndeleted file mode 100644\nindex 1234567..0000000\n",
            "diff --git a/file b/file\nold mode 100644\nnew mode 100755\nindex 1111111..2222222\n",
            "diff --git a/old b/new\nsimilarity index 100%\nrename from old\n",
            "diff --git a/old b/new\nsimilarity index 80%\nrename from old\nrename to new\n",
            "diff --git a/old b/new\nsimilarity index 100%\ncopy to new\n",
            "diff --git a/blob b/blob\nGIT binary patch\n",
        ] {
            assert!(validate_diff(truncated).is_err(), "{truncated:?}");
            assert!(
                validate_diff(&format!("{DIFF}{truncated}")).is_err(),
                "{truncated:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn hosted_process_deadline_reaps_a_stalled_child() {
        let mut command = Command::new("python3");
        command
            .args(["-c", "import time; time.sleep(30)"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let started = Instant::now();
        let error = run(&mut command, "fixture", Duration::from_millis(250));
        assert!(error.is_err_and(|error| error.to_string().contains("PI_REVIEW_TIMEOUT")));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "cleanup must not wait for the 30-second child"
        );
    }

    #[cfg(unix)]
    #[test]
    fn hosted_process_retires_descendants_holding_output_after_root_exit() -> Result<()> {
        let mut command = Command::new("python3");
        command.args([
            "-c",
            "import subprocess, sys; subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)']); sys.stdout.write(sys.argv[1]); sys.stdout.flush()",
            DIFF,
        ]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
        let started = Instant::now();
        assert_eq!(run(&mut command, "fixture", Duration::from_secs(3))?, DIFF);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "an inherited pipe must not outlive its owner"
        );
        Ok(())
    }
}
