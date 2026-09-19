//! Publish dependency SARIF to a new root-level file without overwrite races.

use super::{AgentCx, Path, Result, Value, tool_error};
use std::io::Write as _;
use std::time::Instant;

const MAX_REPORT_BYTES: usize = 16 * 1024 * 1024;

#[allow(clippy::case_sensitive_file_extension_comparisons)]
pub(super) fn validate_name(name: &str) -> Result<()> {
    if name.len() > 240
        || name.len() <= ".sarif".len()
        || !name.ends_with(".sarif")
        || name.starts_with('.')
        || name.contains(['/', '\\', ':'])
        || name.chars().any(char::is_control)
    {
        return Err(tool_error(
            "dependency sarifOut must be a new root-level filename ending .sarif, at most 240 bytes",
        ));
    }
    Ok(())
}

struct BoundedWriter<W> {
    inner: W,
    remaining: usize,
}
impl<W: std::io::Write> std::io::Write for BoundedWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.remaining {
            return Err(std::io::Error::other("dependency SARIF exceeds 16 MiB"));
        }
        let written = self.inner.write(bytes)?;
        self.remaining -= written;
        Ok(written)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
struct Stage<'a> {
    root: &'a rustix::fd::OwnedFd,
    name: String,
}

#[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
impl Drop for Stage<'_> {
    fn drop(&mut self) {
        let _ = rustix::fs::unlinkat(self.root, self.name.as_str(), rustix::fs::AtFlags::empty());
    }
}

#[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
pub(super) fn publish(
    cwd: &Path,
    name: &str,
    report: &Value,
    owner: &AgentCx,
    deadline: Instant,
) -> Result<()> {
    use rustix::fs::{AtFlags, Mode, OFlags};
    validate_name(name)?;
    let root = rustix::fs::open(
        cwd,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| tool_error("cannot open dependency report directory"))?;
    let stage = format!(".pi-dependency-{}.tmp", uuid::Uuid::new_v4());
    let descriptor = rustix::fs::openat(
        &root,
        stage.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::from_bits_truncate(0o600),
    )
    .map_err(|_| tool_error("cannot create private dependency report staging file"))?;
    let _stage = Stage {
        root: &root,
        name: stage.clone(),
    };
    let mut file = std::fs::File::from(descriptor);
    let mut writer = BoundedWriter {
        inner: &mut file,
        remaining: MAX_REPORT_BYTES,
    };
    serde_json::to_writer_pretty(&mut writer, report).map_err(|_| {
        tool_error(
            "dependency report serialization failed or exceeded 16 MiB; destination unchanged",
        )
    })?;
    writer
        .flush()
        .map_err(|_| tool_error("dependency report staging write failed; destination unchanged"))?;
    file.sync_all()
        .map_err(|_| tool_error("dependency report staging sync failed; destination unchanged"))?;
    owner
        .checkpoint()
        .map_err(|_| tool_error("dependency audit cancelled before report publication"))?;
    if Instant::now() >= deadline {
        return Err(tool_error(
            "dependency audit deadline elapsed before report publication",
        ));
    }
    // Hard-link publication is create-only even if another writer or a symlink
    // appears after preflight. Only this owned staging filename is removed.
    rustix::fs::linkat(&root, stage.as_str(), &root, name, AtFlags::empty()).map_err(|_| {
        tool_error(
            "dependency report destination exists or cannot be published; no overwrite performed",
        )
    })?;
    // The payload is synced before publication. Directory-entry durability is
    // filesystem-dependent; do not promise crash-durable rename semantics.
    Ok(())
}

#[cfg(not(all(unix, not(any(target_os = "espidf", target_os = "redox")))))]
pub(super) fn publish(
    _cwd: &Path,
    _name: &str,
    _report: &Value,
    _owner: &AgentCx,
    _deadline: Instant,
) -> Result<()> {
    Err(tool_error(
        "confined dependency SARIF export requires supported Unix descriptor APIs",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn report_names_cannot_address_source_files_or_parent_directories() {
        assert!(validate_name("dependency-review.sarif").is_ok());
        for name in [
            "source.rs",
            "../report.sarif",
            "dir/report.sarif",
            "x\\report.sarif",
            "/report.sarif",
            ".sarif",
            ".hidden.sarif",
            "C:report.sarif",
            "bad\n.sarif",
        ] {
            assert!(validate_name(name).is_err(), "{name:?}");
        }
    }
    #[test]
    fn report_serializer_observes_its_exact_byte_cap() {
        let mut writer = BoundedWriter {
            inner: Vec::new(),
            remaining: 3,
        };
        writer.write_all(b"abc").unwrap();
        assert!(writer.write_all(b"d").is_err());
        assert_eq!(writer.inner, b"abc");
    }
    #[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
    #[test]
    fn real_report_publication_preserves_existing_files_and_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let owner = AgentCx::for_current_or_request();
        let report = serde_json::json!({"version":"2.1.0","runs":[]});
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        publish(dir.path(), "audit.sarif", &report, &owner, deadline).unwrap();
        let bytes = std::fs::read(dir.path().join("audit.sarif")).unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), report);
        assert!(publish(dir.path(), "audit.sarif", &Value::Null, &owner, deadline).is_err());
        assert_eq!(
            std::fs::read(dir.path().join("audit.sarif")).unwrap(),
            bytes
        );
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("marker"), "preserve-me").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("marker"),
            dir.path().join("linked.sarif"),
        )
        .unwrap();
        assert!(publish(dir.path(), "linked.sarif", &report, &owner, deadline).is_err());
        assert_eq!(
            std::fs::read_to_string(outside.path().join("marker")).unwrap(),
            "preserve-me"
        );
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            2,
            "staging paths must be gone"
        );
    }
    #[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
    #[test]
    fn elapsed_deadline_never_publishes_a_report() {
        let dir = tempfile::tempdir().unwrap();
        let owner = AgentCx::for_current_or_request();
        assert!(
            publish(
                dir.path(),
                "audit.sarif",
                &Value::Null,
                &owner,
                Instant::now()
            )
            .is_err()
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
