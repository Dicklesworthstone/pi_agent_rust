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

/// Frame IDs alone survive navigation. Keep the document and URL, while the
/// active CDP scope additionally pins every ancestor and renderer incarnation.
#[derive(Debug, PartialEq, Eq)]
struct Source {
    frame_id: String,
    loader_id: String,
    url: String,
}

impl Source {
    fn parse(frame: &Value, scoped: bool, allowlist: Option<&[String]>) -> Result<Self> {
        let field = |name: &str, limit: usize| {
            frame[name]
                .as_str()
                .filter(|value| {
                    !value.is_empty() && value.len() <= limit && !value.chars().any(char::is_control)
                })
                .map(str::to_owned)
                .ok_or_else(|| error("download source has an invalid document identity"))
        };
        let source = Self {
            frame_id: field("id", 256)?,
            loader_id: field("loaderId", 256)?,
            url: field("url", 64 * 1024)?,
        };
        // frame_document has already revalidated the scoped ancestry. srcdoc
        // inherits it; an unscoped root cannot invent the same exemption.
        if !scoped || source.url != "about:srcdoc" {
            policy::check_navigation(&source.url, allowlist)
                .map_err(|_| error("download source is blocked by browser URL policy"))?;
        }
        Ok(source)
    }

    async fn read(owner: &AgentCx, cdp: &mut Cdp, allowlist: Option<&[String]>) -> Result<Self> {
        let frame = cdp.frame_document(owner).await?;
        Self::parse(&frame, cdp.frame_selected(), allowlist)
    }

    async fn check(&self, owner: &AgentCx, cdp: &mut Cdp, allowlist: Option<&[String]>) -> Result<()> {
        if Self::read(owner, cdp, allowlist).await? != *self {
            return Err(error("download source navigated or changed; no artifact was published"));
        }
        Ok(())
    }
}

fn admit_record(record: &DownloadRecord, source: &Source) -> Result<()> {
    if record.frame_id != source.frame_id {
        return Err(error("download came from a different frame; no artifact was published"));
    }
    validate_download_guid(&record.guid)
}

fn first_new(cdp: &Cdp, before: &BTreeSet<String>, source: &Source) -> Result<Option<DownloadRecord>> {
    cdp.check_download_events()?;
    let fresh = cdp.downloads_since(before);
    if fresh.len() > 1 {
        return Err(error(
            "one download action started multiple transfers; none was published",
        ));
    }
    let record = fresh.into_iter().next();
    if let Some(record) = &record {
        admit_record(record, source)?;
    }
    Ok(record)
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
    source: &'a Source,
    before: &'a BTreeSet<String>,
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
    req.source.check(owner, cdp, req.allowlist).await?;
    if first_new(cdp, req.before, req.source)?.is_some() {
        return Err(error("a download started before the requested click; input was not sent"));
    }
    let click_args = json!({"action":"click","selector":req.selector});
    interaction::execute(owner, cdp, req.tab, req.refs, &click_args).await?;

    let mut start = None;
    for _ in 0..MAX_WAIT_EVENTS {
        if let Some(record) = first_new(cdp, req.before, req.source)? {
            start = Some(record);
            break;
        }
        cdp.pump_event(owner).await?;
    }
    let start = start.ok_or_else(|| error("click produced no download event"))?;
    policy::check_navigation(&start.url, req.allowlist)
        .map_err(|_| error("download URL is blocked by browser URL policy"))?;

    let mut final_record = None;
    for _ in 0..MAX_WAIT_EVENTS {
        // Recheck the complete set, not only the first GUID. A second download
        // can begin while the accepted one is still streaming.
        let record = first_new(cdp, req.before, req.source)?
            .ok_or_else(|| error("download tracking state disappeared"))?;
        if record.guid != start.guid {
            return Err(error("download transfer identity changed"));
        }
        if record.received_bytes > MAX_DOWNLOAD_BYTES_F64
            || record.total_bytes > MAX_DOWNLOAD_BYTES_F64
        {
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
    req.source.check(owner, cdp, req.allowlist).await?;
    cdp.check_download_events()?;
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

async fn cancel_new(owner: &AgentCx, cdp: &mut Cdp, before: &BTreeSet<String>) {
    // These transfers started on this connection during our temporary capture
    // window. Never cancel a GUID that predated the window. Cleanup remains
    // bounded by the original operation's deadline/cancellation and map limit.
    for record in cdp.downloads_since(before) {
        let _ = cdp
            .browser_command(owner, "Browser.cancelDownload", json!({"guid":record.guid}))
            .await;
    }
}

#[allow(clippy::too_many_lines)]
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
    let source = Source::read(owner, cdp, allowlist).await?;
    cdp.check_download_events()?;
    let before = cdp.download_ids();
    let directory = tempfile::Builder::new()
        .prefix("pi-browser-download-")
        .tempdir()?;
    let download_path = directory
        .path()
        .to_str()
        .ok_or_else(|| error("private download directory path is not UTF-8"))?;
    let req = DownloadRequest {
        cwd,
        tab,
        refs,
        selector,
        explicit: explicit.as_ref(),
        allowlist,
        source: &source,
        before: &before,
    };
    let capture = async {
        // Include enablement in the cleanup scope: an error reply or transport
        // failure need not mean Chromium rejected the policy change.
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
        capture_download(owner, cdp, directory.path(), &req).await
    }
    .await;

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

    let result = async {
        let (start, progress, bytes, target) = capture?;
        reset.map_err(|_| error("download policy could not be restored; no artifact was published"))?;
        source.check(owner, cdp, allowlist).await?;
        if first_new(cdp, &before, &source)?.is_none()
            || cdp.download_record(&progress.guid).as_ref() != Some(&progress)
        {
            return Err(error("download changed before publication; no artifact was published"));
        }
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
                "frame_id":source.frame_id,"backend":"cdp"
            }),
        ))
    }
    .await;
    if result.is_err() {
        cancel_new(owner, cdp, &before).await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completed_record(guid: &str, received_bytes: f64) -> DownloadRecord {
        DownloadRecord {
            guid: guid.into(),
            frame_id: "frame-1".into(),
            url: "https://example.com/file".into(),
            suggested_filename: "file".into(),
            state: "completed".into(),
            received_bytes,
            total_bytes: received_bytes,
        }
    }

    #[test]
    fn download_source_requires_a_complete_allowed_document() {
        let frame = json!({"id":"frame-1","loaderId":"loader-1","url":"https://example.com/"});
        let rules = vec!["example.com".to_string()];
        assert!(Source::parse(&frame, false, Some(&rules)).is_ok());
        for field in ["id", "loaderId", "url"] {
            for value in [Value::Null, json!(""), json!("PRIVATE-CANARY\n")] {
                let mut invalid = frame.clone();
                invalid[field] = value;
                let error = Source::parse(&invalid, false, Some(&rules)).unwrap_err();
                assert!(!error.to_string().contains("PRIVATE-CANARY"));
            }
        }
        let mut inherited = frame;
        inherited["url"] = json!("about:srcdoc");
        assert!(Source::parse(&inherited, true, Some(&rules)).is_ok());
        assert!(Source::parse(&inherited, false, Some(&rules)).is_err());
        inherited["url"] = json!("https://blocked.test/PRIVATE-CANARY");
        let error = Source::parse(&inherited, true, Some(&rules)).unwrap_err();
        assert!(!error.to_string().contains("PRIVATE-CANARY"));
    }

    #[test]
    fn identical_download_urls_do_not_authorize_another_frame() {
        let source = Source::parse(
            &json!({"id":"frame-1","loaderId":"loader-1","url":"https://example.com/"}),
            false,
            None,
        ).unwrap();
        let mut record = completed_record("job", 7.0);
        assert!(admit_record(&record, &source).is_ok());
        record.frame_id = "other-tab-private-frame".into();
        let error = admit_record(&record, &source).unwrap_err();
        assert!(!error.to_string().contains("other-tab-private-frame"));
        record.frame_id = source.frame_id.clone();
        record.guid = "../outside".into();
        assert!(admit_record(&record, &source).is_err());
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
