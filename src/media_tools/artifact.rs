//! Publish complete media artifacts without replacing existing user files.

use super::transport;
use crate::agent_cx::AgentCx;
use crate::artifact_output::OutputTarget;
use crate::error::{Error, Result};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(super) struct PreparedArtifact {
    target: OutputTarget,
    automatic_extension: bool,
}

/// Pin the destination before credential resolution or billable provider work.
/// Automatic names receive their final format after the response; neither
/// preparation nor adding the extension creates files or reopens the root.
pub(super) fn preflight(
    cwd: &Path,
    requested: Option<&str>,
    prefix: &str,
    tool: &str,
) -> Result<PreparedArtifact> {
    if requested.is_some_and(|path| path.trim().is_empty() || path.len() > 4096) {
        return Err(Error::tool(
            tool,
            "output_path must be nonempty and at most 4096 bytes",
        ));
    }
    let path = requested.map_or_else(
        || format!("{prefix}_{}", uuid::Uuid::new_v4().simple()),
        ToString::to_string,
    );
    Ok(PreparedArtifact {
        target: crate::artifact_output::resolve_new(cwd, &path, tool)?,
        automatic_extension: requested.is_none(),
    })
}

pub(super) fn publish(
    prepared: &PreparedArtifact,
    extension: &str,
    bytes: &[u8],
    owner: Option<&AgentCx>,
    tool: &str,
) -> Result<PathBuf> {
    if let Some(owner) = owner {
        transport::check_owner(tool, owner)?;
    }
    if bytes.is_empty() {
        return Err(Error::tool(
            tool,
            "refusing to publish an empty media artifact",
        ));
    }
    let target = if prepared.automatic_extension {
        prepared.target.with_extension(extension, tool)?
    } else {
        prepared.target.clone()
    };
    let path = target.path();
    let actual_extension = path.extension().and_then(|ext| ext.to_str()).unwrap_or("");
    let matches = actual_extension.eq_ignore_ascii_case(extension)
        || (extension == "jpg" && actual_extension.eq_ignore_ascii_case("jpeg"));
    if !matches {
        return Err(Error::tool(
            tool,
            format!(
                "provider returned {extension} data, but output_path has a different extension; use .{extension} or omit output_path"
            ),
        ));
    }
    if let Some(owner) = owner {
        transport::check_owner(tool, owner)?;
    }
    crate::artifact_output::publish(&target, bytes, tool)?;
    Ok(target.path().to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_never_overwrites_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let prepared = preflight(
            dir.path(),
            Some("result.png"),
            "images/result",
            "generate_image",
        )
        .unwrap();
        let path = publish(&prepared, "png", b"original", None, "generate_image").unwrap();
        assert!(
            preflight(
                dir.path(),
                Some("result.png"),
                "images/result",
                "generate_image"
            )
            .is_err()
        );
        assert!(publish(&prepared, "png", b"replacement", None, "generate_image").is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"original");
    }

    #[test]
    fn wrong_extension_and_empty_bytes_do_not_create_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let image = preflight(dir.path(), Some("result.png"), "image", "generate_image").unwrap();
        assert!(publish(&image, "jpg", b"jpeg", None, "generate_image").is_err());
        let audio = preflight(dir.path(), Some("result.wav"), "audio", "tts").unwrap();
        assert!(publish(&audio, "wav", b"", None, "tts").is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn requested_paths_cannot_escape_workspace_or_use_linked_parents() {
        let dir = tempfile::tempdir().unwrap();
        for path in [
            "../escape.png",
            "/tmp/escape.png",
            "a/../../escape.png",
            "a\\escape.png",
        ] {
            assert!(
                preflight(dir.path(), Some(path), "images/generated", "generate_image").is_err(),
                "{path}"
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let outside = tempfile::tempdir().unwrap();
            symlink(outside.path(), dir.path().join("linked")).unwrap();
            assert!(
                preflight(
                    dir.path(),
                    Some("linked/result.png"),
                    "images/generated",
                    "generate_image"
                )
                .is_err()
            );
        }
    }

    #[test]
    fn generated_filenames_use_the_received_format() {
        let dir = tempfile::tempdir().unwrap();
        let prepared = preflight(dir.path(), None, "images/generated", "generate_image").unwrap();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        let path = publish(&prepared, "jpg", b"jpeg", None, "generate_image").unwrap();
        assert_eq!(path.extension().unwrap(), "jpg");
        assert_eq!(std::fs::read(path).unwrap(), b"jpeg");
    }

    #[test]
    fn automatic_format_selection_does_not_replace_a_raced_in_final_name() {
        let dir = tempfile::tempdir().unwrap();
        let prepared = preflight(dir.path(), None, "images/generated", "generate_image").unwrap();
        let final_path = prepared.target.path().with_extension("webp");
        std::fs::create_dir_all(final_path.parent().unwrap()).unwrap();
        std::fs::write(&final_path, b"keep").unwrap();

        assert!(publish(&prepared, "webp", b"replacement", None, "generate_image").is_err());
        assert_eq!(std::fs::read(&final_path).unwrap(), b"keep");
        assert_eq!(
            std::fs::read_dir(final_path.parent().unwrap())
                .unwrap()
                .count(),
            1
        );
    }

    #[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
    #[test]
    fn prepared_media_targets_reject_a_replaced_workspace() {
        for requested in [Some("nested/result.png"), None] {
            let parent = tempfile::tempdir().unwrap();
            let root = parent.path().join("workspace");
            let moved = parent.path().join("original-workspace");
            std::fs::create_dir(&root).unwrap();
            let prepared =
                preflight(&root, requested, "images/generated", "generate_image").unwrap();
            assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);

            // Represents a workspace replacement while the provider request
            // is in flight, after the destination was admitted.
            std::fs::rename(&root, &moved).unwrap();
            std::fs::create_dir(&root).unwrap();
            let failure =
                publish(&prepared, "png", b"private payload", None, "generate_image").unwrap_err();
            assert!(failure.to_string().contains("workspace root changed"));
            assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
            assert_eq!(std::fs::read_dir(&moved).unwrap().count(), 0);
        }
    }
}
