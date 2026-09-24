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

fn validate_download_guid(guid: &str) -> Result<()> {
    // CDP is a protocol boundary, not a source of trusted filesystem paths.
    // In particular, Path::join would discard the private directory for an
    // absolute GUID, and allowAndName does not make a received string safe.
    let mut components = Path::new(guid).components();
    if guid.is_empty()
        || guid.len() > 128
        || guid.contains('/')
        || guid.contains('\\')
        || guid.chars().any(char::is_control)
        || !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
    {
        return Err(error("download GUID must be a single bounded filename"));
    }
    Ok(())
}

fn open_completed(directory: &Path, guid: &str) -> Result<std::fs::File> {
    validate_download_guid(guid)?;
    #[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
    {
        use rustix::fs::{Mode, OFlags};
        let directory = rustix::fs::open(
            directory,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|failure| error(format!("cannot pin private download directory: {failure}")))?;
        // NONBLOCK prevents a raced-in FIFO from hanging before we can inspect
        // the descriptor. NOFOLLOW rejects a substituted symlink atomically.
        let file = rustix::fs::openat(
            &directory,
            guid,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(|failure| error(format!("cannot safely open completed download: {failure}")))?;
        Ok(std::fs::File::from(file))
    }
    #[cfg(not(all(unix, not(any(target_os = "espidf", target_os = "redox")))))]
    {
        let path = directory.join(guid);
        let metadata = std::fs::symlink_metadata(&path).map_err(|failure| {
            error(format!("completed download file is unavailable: {failure}"))
        })?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(error("download result must be a regular file"));
        }
        Ok(std::fs::File::open(path)?)
    }
}

#[expect(clippy::cast_precision_loss)]
fn read_completed(directory: &Path, progress: &DownloadRecord) -> Result<Vec<u8>> {
    if progress.state != "completed" {
        return Err(error("download has not completed"));
    }
    if [progress.received_bytes, progress.total_bytes]
        .into_iter()
        .any(|value| !value.is_finite() || !(0.0..=MAX_DOWNLOAD_BYTES_F64).contains(&value))
    {
        return Err(error(
            "download progress is outside the 100 MiB capture budget",
        ));
    }
    let mut file = open_completed(directory, &progress.guid)?;
    // Validate the object actually opened, not a pathname checked before open.
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_DOWNLOAD_BYTES {
        return Err(error(
            "download result must be a regular file no larger than 100 MiB",
        ));
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_DOWNLOAD_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_DOWNLOAD_BYTES {
        return Err(error("download grew beyond the 100 MiB capture budget"));
    }
    // All allowed byte counts are exactly representable in f64. Zero is a
    // real completed byte count, not permission to skip the integrity check.
    if (progress.received_bytes - bytes.len() as f64).abs() > 0.0 {
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
    validate_download_guid(&start.guid)?;
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
    let bytes = read_completed(directory, &final_record)?;
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

    fn completed_record(guid: &str, received_bytes: f64) -> DownloadRecord {
        DownloadRecord {
            guid: guid.into(),
            url: "https://example.com/file".into(),
            suggested_filename: "file".into(),
            state: "completed".into(),
            received_bytes,
            total_bytes: received_bytes,
        }
    }

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
        std::fs::write(dir.path().join("guid"), b"payload").unwrap();
        let record = completed_record("guid", 7.0);
        assert_eq!(read_completed(dir.path(), &record).unwrap(), b"payload");
        for received in [
            0.0,
            7.5,
            8.0,
            -1.0,
            f64::NAN,
            f64::INFINITY,
            MAX_DOWNLOAD_BYTES_F64 + 1.0,
        ] {
            let mut wrong = record.clone();
            wrong.received_bytes = received;
            assert!(read_completed(dir.path(), &wrong).is_err(), "{received}");
        }
        let mut pending = record.clone();
        pending.state = "inProgress".into();
        assert!(read_completed(dir.path(), &pending).is_err());
        let mut invalid_total = record;
        invalid_total.total_bytes = f64::NAN;
        assert!(read_completed(dir.path(), &invalid_total).is_err());
    }

    #[test]
    fn download_guids_cannot_escape_private_staging() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.bin");
        std::fs::write(&secret, b"secret").unwrap();
        let record = completed_record(secret.to_str().unwrap(), 6.0);
        assert!(read_completed(dir.path(), &record).is_err());
        for guid in [
            "",
            ".",
            "..",
            "../secret.bin",
            "nested/../../secret.bin",
            "nested/file",
            "file/",
            "nested\\file",
            "C:\\secret.bin",
            "bad\0guid",
            "bad\nguid",
        ] {
            let record = completed_record(guid, 6.0);
            assert!(read_completed(dir.path(), &record).is_err(), "{guid:?}");
        }
        assert!(validate_download_guid(&"x".repeat(129)).is_err());
        for guid in [
            "guid",
            "opaque-id_42",
            "opaque.name",
            "12345678-abcd-1234-abcd-123456789abc",
        ] {
            assert!(validate_download_guid(guid).is_ok(), "{guid}");
        }
    }

    #[test]
    fn genuinely_empty_completed_file_is_read_without_bypassing_counts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("guid"), b"").unwrap();
        let record = completed_record("guid", 0.0);
        assert!(read_completed(dir.path(), &record).unwrap().is_empty());
    }

    #[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
    #[test]
    fn completed_file_rejects_linked_private_directory() {
        use std::os::unix::fs::symlink;
        let parent = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("guid"), b"secret").unwrap();
        let linked = parent.path().join("staging");
        symlink(outside.path(), &linked).unwrap();
        assert!(read_completed(&linked, &completed_record("guid", 6.0)).is_err());
    }

    #[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
    #[test]
    fn completed_file_rejects_symlink_and_fifo_leaves() {
        use rustix::fs::Mode;
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.bin");
        std::fs::write(&secret, b"secret").unwrap();
        symlink(&secret, dir.path().join("linked")).unwrap();
        assert!(read_completed(dir.path(), &completed_record("linked", 6.0)).is_err());
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            dir.path().join("fifo"),
            Mode::RUSR | Mode::WUSR,
        )
        .unwrap();
        assert!(read_completed(dir.path(), &completed_record("fifo", 0.0)).is_err());
    }
}
