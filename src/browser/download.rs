//! Explicit capture of one click-triggered Chromium download.

use super::cdp::{Cdp, DownloadRecord};
use super::interaction::{self, References};
use super::{output, policy, required};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::tools::ToolOutput;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::io::Read as _;
use std::path::{Component, Path};

const MAX_DOWNLOAD_BYTES: u64 = 100 * 1024 * 1024;
const MAX_DOWNLOAD_BYTES_F64: f64 = 100.0 * 1024.0 * 1024.0;
const MAX_WAIT_EVENTS: usize = 8192;

fn error(message: impl Into<String>) -> Error {
    Error::tool("browser", message)
}

fn lexical_output_path(path: &str) -> Result<()> {
    if path.is_empty()
        || path.len() > 4096
        || path.contains('\0')
        || path.contains('\\')
        || path.chars().any(char::is_control)
    {
        return Err(error(
            "download output_path must be a nonempty control-free relative path using slash separators",
        ));
    }
    let path = Path::new(path);
    if path.is_absolute()
        || path.file_name().is_none()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
    {
        return Err(error(
            "download output_path must name a relative workspace file without parent traversal",
        ));
    }
    Ok(())
}

pub(super) fn validate(args: &Value) -> Result<()> {
    let object = args
        .as_object()
        .ok_or_else(|| error("download arguments must be an object"))?;
    for field in object.keys() {
        if !matches!(
            field.as_str(),
            "action" | "tab" | "selector" | "output_path" | "timeout_ms"
        ) {
            return Err(error(format!("unsupported download parameter: {field}")));
        }
    }
    let selector = required(args, "selector")?;
    if selector.is_empty() || selector.len() > 4096 || selector.contains('\0') {
        return Err(error(
            "download selector must be nonempty, NUL-free and at most 4096 bytes",
        ));
    }
    if let Some(path) = args.get("output_path") {
        lexical_output_path(
            path.as_str()
                .ok_or_else(|| error("download output_path must be a string"))?,
        )?;
    }
    Ok(())
}

fn safe_filename(value: &str) -> String {
    let leaf = value.rsplit(['/', '\\']).next().unwrap_or_default();
    let mut result = String::new();
    for ch in leaf.chars().take(180) {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | ' ') {
            result.push(ch);
        } else {
            result.push('_');
        }
    }
    let trimmed = result.trim_matches(['.', ' ']).to_string();
    if trimmed.is_empty() || matches!(trimmed.as_str(), "." | "..") {
        format!("download-{}", uuid::Uuid::new_v4().simple())
    } else {
        trimmed
    }
}

fn first_new(cdp: &Cdp, before: &BTreeSet<String>) -> Result<Option<DownloadRecord>> {
    let fresh = cdp.downloads_since(before);
    if fresh.len() > 1 {
        return Err(error(
            "one download action started multiple transfers; none was published",
        ));
    }
    Ok(fresh.into_iter().next())
}

#[expect(clippy::cast_precision_loss)]
fn read_completed(path: &Path, progress: &DownloadRecord) -> Result<Vec<u8>> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|failure| error(format!("completed download file is unavailable: {failure}")))?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > MAX_DOWNLOAD_BYTES
    {
        return Err(error(
            "download result must be a regular file no larger than 100 MiB",
        ));
    }
    if progress.received_bytes > MAX_DOWNLOAD_BYTES_F64
        || progress.total_bytes > MAX_DOWNLOAD_BYTES_F64
    {
        return Err(error("download exceeded the 100 MiB capture budget"));
    }
    let mut file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_DOWNLOAD_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_DOWNLOAD_BYTES {
        return Err(error("download grew beyond the 100 MiB capture budget"));
    }
    if progress.received_bytes > 0.0 && (progress.received_bytes - bytes.len() as f64).abs() > 0.5 {
        return Err(error(
            "completed download byte count did not match Chromium progress",
        ));
    }
    Ok(bytes)
}

struct DownloadRequest<'a> {
    cwd: &'a Path,
    tab: &'a str,
    refs: Option<&'a References>,
    selector: &'a str,
    explicit: Option<&'a crate::artifact_output::OutputTarget>,
    allowlist: Option<&'a [String]>,
}

async fn capture_download(
    owner: &AgentCx,
    cdp: &mut Cdp,
    directory: &Path,
    req: &DownloadRequest<'_>,
) -> Result<(
    DownloadRecord,
    DownloadRecord,
    Vec<u8>,
    crate::artifact_output::OutputTarget,
)> {
    let before = cdp.download_ids();
    let click_args = json!({"action":"click","selector":req.selector});
    interaction::execute(owner, cdp, req.tab, req.refs, &click_args).await?;

    let mut start = None;
    for _ in 0..MAX_WAIT_EVENTS {
        if let Some(record) = first_new(cdp, &before)? {
            start = Some(record);
            break;
        }
        cdp.pump_event(owner).await?;
    }
    let start = start.ok_or_else(|| error("click produced no download event"))?;
    if let Err(failure) = policy::check_navigation(&start.url, req.allowlist) {
        let _ = cdp
            .browser_command(owner, "Browser.cancelDownload", json!({"guid":start.guid}))
            .await;
        return Err(failure);
    }

    let mut final_record = None;
    for _ in 0..MAX_WAIT_EVENTS {
        let record = cdp
            .download_record(&start.guid)
            .ok_or_else(|| error("download tracking state disappeared"))?;
        if record.received_bytes > MAX_DOWNLOAD_BYTES_F64
            || record.total_bytes > MAX_DOWNLOAD_BYTES_F64
        {
            let _ = cdp
                .browser_command(owner, "Browser.cancelDownload", json!({"guid":start.guid}))
                .await;
            return Err(error("download exceeded the 100 MiB capture budget"));
        }
        match record.state.as_str() {
            "completed" => {
                final_record = Some(record);
                break;
            }
            "canceled" => return Err(error("Chromium canceled the download")),
            "inProgress" => cdp.pump_event(owner).await?,
            _ => return Err(error("Chromium reported an unknown download state")),
        }
    }
    let final_record = final_record
        .ok_or_else(|| error("download did not complete within the CDP event budget"))?;
    owner
        .checkpoint()
        .map_err(|_| error("download cancelled before reading completed bytes"))?;
    let bytes = read_completed(&directory.join(&start.guid), &final_record)?;
    let target = if let Some(target) = req.explicit {
        (*target).clone()
    } else {
        let filename = safe_filename(&start.suggested_filename);
        crate::artifact_output::resolve_new(req.cwd, &format!("downloads/{filename}"), "browser")?
    };
    Ok((start, final_record, bytes, target))
}

pub(super) async fn execute(
    owner: &AgentCx,
    cdp: &mut Cdp,
    cwd: &Path,
    tab: &str,
    refs: Option<&References>,
    args: &Value,
    allowlist: Option<&[String]>,
) -> Result<ToolOutput> {
    validate(args)?;
    let selector = required(args, "selector")?;
    let explicit = args
        .get("output_path")
        .and_then(Value::as_str)
        .map(|path| crate::artifact_output::resolve_new(cwd, path, "browser"))
        .transpose()?;
    let directory = tempfile::Builder::new()
        .prefix("pi-browser-download-")
        .tempdir()?;
    let download_path = directory
        .path()
        .to_str()
        .ok_or_else(|| error("private download directory path is not UTF-8"))?;
    cdp.browser_command(
        owner,
        "Browser.setDownloadBehavior",
        json!({
            "behavior":"allowAndName",
            "downloadPath":download_path,
            "eventsEnabled":true
        }),
    )
    .await?;

    let req = DownloadRequest {
        cwd,
        tab,
        refs,
        selector,
        explicit: explicit.as_ref(),
        allowlist,
    };
    let capture = capture_download(owner, cdp, directory.path(), &req).await;

    // Ordinary errors attempt to return to deny. If outer cancellation drops
    // this future, Session leaves its policy marker false so the next operation
    // re-applies deny before doing anything else.
    let reset = cdp
        .browser_command(
            owner,
            "Browser.setDownloadBehavior",
            json!({"behavior":"deny","eventsEnabled":true}),
        )
        .await;

    let (start, progress, bytes, target) = capture?;
    reset.map_err(|failure| {
        error(format!(
            "download completed but browser download policy could not be restored: {failure}"
        ))
    })?;
    owner
        .checkpoint()
        .map_err(|_| error("download cancelled before publication"))?;
    crate::artifact_output::publish(&target, &bytes, "browser")?;

    Ok(output(
        format!(
            "Downloaded {} bytes from {} to {}",
            bytes.len(),
            start.url,
            target.path().display()
        ),
        json!({
            "action":"download","tab":tab,"selector":selector,
            "saved_path":target.path().display().to_string(),
            "url":start.url,"suggested_filename":start.suggested_filename,
            "size_bytes":bytes.len(),"received_bytes":progress.received_bytes,
            "backend":"cdp"
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameters_and_filenames_fail_closed() {
        assert!(validate(&json!({"action":"download","selector":"#link"})).is_ok());
        for args in [
            json!({"action":"download"}),
            json!({"action":"download","selector":"#x","files":[]}),
            json!({"action":"download","selector":"#x","output_path":"../escape.bin"}),
            json!({"action":"download","selector":"#x","output_path":"/tmp/escape.bin"}),
            json!({"action":"download","selector":"#x","output_path":"a\\escape.bin"}),
        ] {
            assert!(validate(&args).is_err(), "{args}");
        }
        assert_eq!(safe_filename("../../report final.pdf"), "report final.pdf");
        assert!(!safe_filename("..").is_empty());
        assert!(!safe_filename("bad\nname?.txt").contains('\n'));
        assert!(!safe_filename("bad/name.txt").contains('/'));
    }

    #[test]
    fn completed_file_must_match_progress_and_budget() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("guid");
        std::fs::write(&path, b"payload").unwrap();
        let record = DownloadRecord {
            guid: "guid".into(),
            url: "https://example.com/file".into(),
            suggested_filename: "file".into(),
            state: "completed".into(),
            received_bytes: 7.0,
            total_bytes: 7.0,
        };
        assert_eq!(read_completed(&path, &record).unwrap(), b"payload");
        let mut wrong = record;
        wrong.received_bytes = 8.0;
        assert!(read_completed(&path, &wrong).is_err());
    }
}
