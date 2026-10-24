//! Skill instructions, supporting files, and bounded directory inventories.
//!
//! Loaded skills own their namespaces. Supporting paths are relative to the
//! selected skill's base directory, never the caller's working directory.
//! Unknown skills and rejected paths never fall back to ordinary file reads.

use super::{ResolvedDoc, URL_ROUTER_SCHEMA, documents};
use crate::error::{Error, Result};
use crate::resources::Skill;
use serde_json::{Value, json};
use std::ffi::OsString;
use std::io::Read as _;
use std::path::Path;

const MAX_REFERENCE_BYTES: usize = 4096;
const MAX_PATH_COMPONENTS: usize = 64;
const MAX_CATALOG_ENTRIES: usize = 128;
const MAX_DIRECTORY_ENTRIES: usize = 256;
const MAX_DIRECTORY_SCAN: usize = 4096;

fn error(code: &str, message: impl std::fmt::Display) -> Error {
    Error::tool("read", format!("{code}: {message}"))
}

fn invalid(message: impl std::fmt::Display) -> Error {
    error("PI_URL_RESOURCE_INVALID", message)
}

const fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn decode_reference(reference: &str) -> Result<String> {
    if reference.len() > MAX_REFERENCE_BYTES {
        return Err(error(
            "PI_URL_SKILL_PATH",
            "skill reference exceeds 4096 bytes",
        ));
    }
    let mut decoded = Vec::with_capacity(reference.len());
    let mut bytes = reference.bytes();
    while let Some(byte) = bytes.next() {
        if byte != b'%' {
            decoded.push(byte);
            continue;
        }
        let high = bytes.next().and_then(hex_digit);
        let low = bytes.next().and_then(hex_digit);
        let (Some(high), Some(low)) = (high, low) else {
            return Err(error("PI_URL_SKILL_PATH", "invalid percent escape"));
        };
        decoded.push(high * 16 + low);
    }
    let decoded = String::from_utf8(decoded)
        .map_err(|_| error("PI_URL_SKILL_PATH", "skill reference is not valid UTF-8"))?;
    let without_trailing_slash = decoded.strip_suffix('/').unwrap_or(&decoded);
    if decoded.starts_with('/')
        || decoded.contains('\\')
        || decoded.chars().any(char::is_control)
        || decoded.split('/').count() > MAX_PATH_COMPONENTS
        || without_trailing_slash
            .split('/')
            .any(|part| part.is_empty() || part == ".." || part.contains(':'))
    {
        return Err(error(
            "PI_URL_SKILL_PATH",
            "absolute, parent, empty, over-deep or platform-prefixed skill paths are not allowed",
        ));
    }
    Ok(decoded)
}

fn encode_path(path: &str) -> String {
    use std::fmt::Write as _;

    let mut encoded = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
            encoded.push(char::from(byte));
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

fn matching_skill<'a, 'b>(skills: &'a [Skill], reference: &'b str) -> Option<(&'a Skill, &'b str)> {
    skills
        .iter()
        .filter_map(|skill| {
            if reference == skill.name {
                Some((skill, ""))
            } else {
                reference
                    .strip_prefix(skill.name.as_str())
                    .and_then(|rest| rest.strip_prefix('/'))
                    .map(|rest| (skill, rest))
            }
        })
        // An exact collision namespace wins over treating its name as a
        // supporting file under a bare skill named after the namespace.
        .max_by_key(|(skill, _)| skill.name.len())
}

fn document(reference: &str, content: String, content_type: &str, metadata: Value) -> ResolvedDoc {
    ResolvedDoc {
        schema: URL_ROUTER_SCHEMA.to_string(),
        scheme: "skill".to_string(),
        reference: reference.to_string(),
        content,
        content_type: content_type.to_string(),
        metadata,
        line_addressable: true,
    }
}

fn catalog(reference: &str, mut skills: Vec<Skill>) -> Result<ResolvedDoc> {
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    let total = skills.len();
    let truncated = total > MAX_CATALOG_ENTRIES;
    let entries: Vec<_> = skills
        .iter()
        .take(MAX_CATALOG_ENTRIES)
        .map(|skill| {
            json!({
                "name": skill.name,
                "description": skill.description.chars().take(240).collect::<String>(),
                "url": format!("skill://{}", encode_path(&skill.name)),
            })
        })
        .collect();
    Ok(document(
        reference,
        serde_json::to_string_pretty(&json!({
            "skills": entries, "total": total, "truncated": truncated,
        }))?,
        "application/json",
        json!({"kind":"catalog", "total":total, "truncated":truncated}),
    ))
}

pub(super) fn resolve(reference: &str, cwd: &Path) -> Result<ResolvedDoc> {
    let decoded = if reference.is_empty() {
        None
    } else {
        Some(decode_reference(reference)?)
    };
    // Keep existing instruction-file diagnostics and behavior for bare names.
    // Decode once here; the original document handler does no percent decoding.
    if let Some(name) = decoded.as_deref().filter(|name| !name.contains('/')) {
        return documents::resolve(&format!("skill://{name}"), cwd);
    }
    let loaded = crate::resources::load_skills(crate::resources::LoadSkillsOptions {
        cwd: cwd.to_path_buf(),
        agent_dir: crate::config::Config::global_dir(),
        skill_paths: Vec::new(),
        include_defaults: true,
    });
    let Some(decoded) = decoded else {
        return catalog(reference, loaded.skills);
    };
    let Some((skill, relative)) = matching_skill(&loaded.skills, &decoded) else {
        // Preserve malformed-resource diagnostics instead of misreporting an
        // unreadable SKILL.md as an absent supporting file. Never substitute
        // an instruction document for the requested supporting path.
        let name = decoded.split('/').next().unwrap_or(&decoded);
        return match documents::resolve(&format!("skill://{name}"), cwd) {
            Err(error) => Err(error),
            Ok(_) => Err(error(
                "PI_URL_SKILL_CHANGED",
                "skill catalog changed while resolving this path; retry the read",
            )),
        };
    };
    if relative.is_empty() {
        return documents::resolve(&format!("skill://{}", skill.name), cwd);
    }
    resolve_resource(reference, skill, relative)
}

fn resolve_resource(reference: &str, skill: &Skill, relative: &str) -> Result<ResolvedDoc> {
    let root = std::fs::canonicalize(&skill.base_dir)
        .map_err(|error| invalid(format!("cannot resolve skill base directory: {error}")))?;
    let target = std::fs::canonicalize(root.join(relative))
        .map_err(|error| invalid(format!("cannot resolve skill supporting path: {error}")))?;
    if !target.starts_with(&root) {
        return Err(error(
            "PI_URL_SKILL_SCOPE",
            "skill path resolves outside its base directory",
        ));
    }
    let metadata = std::fs::metadata(&target)
        .map_err(|error| invalid(format!("cannot inspect skill supporting path: {error}")))?;
    if metadata.is_dir() {
        return directory(reference, skill, relative, &root, &target);
    }
    if !metadata.is_file() || relative.ends_with('/') {
        return Err(invalid(
            "skill path must address a regular file or directory",
        ));
    }
    let content = read_file_with(&root, &target, || {})?;
    let content_type = match target.extension().and_then(|extension| extension.to_str()) {
        Some("md" | "markdown") => "text/markdown",
        Some("json") => "application/json",
        _ => "text/plain",
    };
    Ok(document(
        reference,
        content,
        content_type,
        json!({
            "kind":"file", "skill":skill.name, "source":skill.source,
            "path":target.display().to_string(),
        }),
    ))
}

#[cfg(unix)]
fn open_contained(root: &Path, target: &Path) -> std::io::Result<std::fs::File> {
    use rustix::fs::{Mode, OFlags, open, openat};
    use std::path::Component;

    let flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
    let descriptor =
        open(root, flags | OFlags::DIRECTORY, Mode::empty()).map_err(std::io::Error::from)?;
    let mut file = std::fs::File::from(descriptor);
    let relative = target.strip_prefix(root).map_err(std::io::Error::other)?;
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err(std::io::Error::other("non-relative skill path"));
        };
        let flags = if components.peek().is_some() {
            flags | OFlags::DIRECTORY
        } else {
            flags
        };
        let descriptor = openat(&file, name, flags, Mode::empty()).map_err(std::io::Error::from)?;
        file = std::fs::File::from(descriptor);
    }
    Ok(file)
}

#[cfg(not(unix))]
fn open_contained(_root: &Path, target: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::open(target)
}

fn checked_open(
    root: &Path,
    target: &Path,
    directory: bool,
    before_open: impl FnOnce(),
) -> Result<std::fs::File> {
    let expected = std::fs::metadata(target)
        .map_err(|error| invalid(format!("cannot inspect skill path: {error}")))?;
    before_open();
    let file = open_contained(root, target)
        .map_err(|error| invalid(format!("cannot open contained skill path: {error}")))?;
    let opened = file
        .metadata()
        .map_err(|error| invalid(format!("cannot inspect opened skill path: {error}")))?;
    let correct_kind = if directory {
        expected.is_dir() && opened.is_dir()
    } else {
        expected.is_file() && opened.is_file()
    };
    if !correct_kind {
        return Err(invalid("skill path changed file type while opening"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if expected.dev() != opened.dev() || expected.ino() != opened.ino() {
            return Err(invalid("skill path changed while opening; retry the read"));
        }
    }
    #[cfg(windows)]
    {
        // Stable metadata checks, not experimental windows_by_handle methods.
        // This is not the descriptor-relative Unix containment guarantee.
        use std::os::windows::fs::MetadataExt as _;
        if expected.file_attributes() != opened.file_attributes()
            || expected.creation_time() != opened.creation_time()
            || expected.file_size() != opened.file_size()
            || expected.last_write_time() != opened.last_write_time()
        {
            return Err(invalid("skill path changed while opening; retry the read"));
        }
    }
    let current = std::fs::canonicalize(target)
        .map_err(|error| invalid(format!("cannot recheck skill path: {error}")))?;
    if current != target || !current.starts_with(root) {
        return Err(error(
            "PI_URL_SKILL_SCOPE",
            "skill path changed containment",
        ));
    }
    Ok(file)
}

fn read_file_with(root: &Path, target: &Path, before_open: impl FnOnce()) -> Result<String> {
    let file = checked_open(root, target, false, before_open)?;
    let max_bytes = u64::try_from(crate::theme::MAX_RESOURCE_FILE_BYTES)
        .map_err(|_| invalid("skill resource byte limit is out of range"))?;
    let metadata = file
        .metadata()
        .map_err(|error| invalid(format!("cannot inspect skill file size: {error}")))?;
    if metadata.len() > max_bytes {
        return Err(invalid(format!(
            "skill file exceeds the {max_bytes}-byte resource limit"
        )));
    }
    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| invalid(format!("cannot read skill file: {error}")))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max_bytes {
        return Err(invalid(format!(
            "skill file exceeds the {max_bytes}-byte resource limit"
        )));
    }
    String::from_utf8(bytes).map_err(|_| invalid("skill supporting file is not valid UTF-8"))
}

#[cfg(all(
    unix,
    not(any(target_os = "espidf", target_os = "horizon", target_os = "redox"))
))]
fn directory_names(root: &Path, target: &Path) -> Result<(Vec<OsString>, bool)> {
    use std::os::unix::ffi::OsStrExt as _;

    let file = checked_open(root, target, true, || {})?;
    let listing = rustix::fs::Dir::read_from(&file)
        .map_err(|error| invalid(format!("cannot list pinned skill directory: {error}")))?;
    let mut names = Vec::new();
    for item in listing {
        let item = item.map_err(|error| invalid(format!("cannot list skill entry: {error}")))?;
        let name = item.file_name().to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        if names.len() == MAX_DIRECTORY_SCAN {
            return Ok((names, true));
        }
        names.push(std::ffi::OsStr::from_bytes(name).to_os_string());
    }
    Ok((names, false))
}

#[cfg(not(all(
    unix,
    not(any(target_os = "espidf", target_os = "horizon", target_os = "redox"))
)))]
fn directory_names(_root: &Path, target: &Path) -> Result<(Vec<OsString>, bool)> {
    let listing = std::fs::read_dir(target)
        .map_err(|error| invalid(format!("cannot list skill directory: {error}")))?;
    let mut names = Vec::new();
    for item in listing {
        let item = item.map_err(|error| invalid(format!("cannot list skill entry: {error}")))?;
        if names.len() == MAX_DIRECTORY_SCAN {
            return Ok((names, true));
        }
        names.push(item.file_name());
    }
    Ok((names, false))
}

fn directory(
    reference: &str,
    skill: &Skill,
    relative: &str,
    root: &Path,
    target: &Path,
) -> Result<ResolvedDoc> {
    let (names, scan_truncated) = directory_names(root, target)?;
    let mut entries = Vec::new();
    for name in names {
        let path = target.join(&name);
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| invalid(format!("cannot inspect skill entry: {error}")))?;
        let kind = if metadata.is_symlink() {
            "symlink"
        } else if metadata.is_dir() {
            "directory"
        } else if metadata.is_file() {
            "file"
        } else {
            "special"
        };
        let target_is_contained = std::fs::canonicalize(&path)
            .ok()
            .filter(|path| path.starts_with(root))
            .and_then(|path| std::fs::metadata(path).ok())
            .is_some_and(|metadata| metadata.is_file() || metadata.is_dir());
        let addressable_name = name
            .to_str()
            .filter(|name| !name.contains(['\\', ':']) && !name.chars().any(char::is_control));
        let url = addressable_name
            .filter(|_| target_is_contained)
            .map(|name| {
                let relative = relative.trim_end_matches('/');
                let path = if relative == "." {
                    format!("{}/{name}", skill.name)
                } else {
                    format!("{}/{relative}/{name}", skill.name)
                };
                format!("skill://{}", encode_path(&path))
            });
        entries.push(json!({
            "name":name.to_string_lossy(), "kind":kind,
            "readable":url.is_some(), "url":url,
        }));
    }
    entries.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    let scanned = entries.len();
    let truncated = scan_truncated || scanned > MAX_DIRECTORY_ENTRIES;
    entries.truncate(MAX_DIRECTORY_ENTRIES);
    let metadata = json!({
        "kind":"directory", "skill":skill.name, "source":skill.source,
        "path":target.display().to_string(), "scanned":scanned, "truncated":truncated,
    });
    Ok(document(
        reference,
        serde_json::to_string_pretty(&json!({
            "entries":entries, "scanned":scanned, "truncated":truncated,
        }))?,
        "application/json",
        metadata,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> (tempfile::TempDir, Skill) {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("skill");
        std::fs::create_dir(&base).unwrap();
        let instructions = base.join("SKILL.md");
        std::fs::write(&instructions, "instructions\n").unwrap();
        let skill = Skill {
            name: name.to_string(),
            description: "supporting resource fixture".to_string(),
            file_path: instructions,
            base_dir: base,
            source: "test".to_string(),
            disable_model_invocation: false,
        };
        (root, skill)
    }

    #[test]
    fn supporting_files_use_the_skill_base_and_preserve_utf8() {
        let (_root, skill) = fixture("review");
        std::fs::create_dir(skill.base_dir.join("references")).unwrap();
        std::fs::write(
            skill.base_dir.join("references/café notes.md"),
            "one\ntwo\n",
        )
        .unwrap();
        let decoded = decode_reference("review/references/caf%C3%A9%20notes.md").unwrap();
        let skills = vec![skill];
        let (skill, relative) = matching_skill(&skills, &decoded).unwrap();
        let doc = resolve_resource(&decoded, skill, relative).unwrap();
        assert_eq!(doc.content, "one\ntwo\n");
        assert_eq!(doc.content_type, "text/markdown");
        assert_eq!(doc.metadata["skill"], "review");
    }

    #[test]
    fn exact_namespace_ownership_wins_over_bare_skill_paths() {
        let (_bare_root, bare) = fixture("plugin");
        let (_namespaced_root, namespaced) = fixture("plugin/review");
        let skills = vec![bare, namespaced];
        let (selected, relative) = matching_skill(&skills, "plugin/review/rules.md").unwrap();
        assert_eq!(selected.name, "plugin/review");
        assert_eq!(relative, "rules.md");
        assert_eq!(matching_skill(&skills, "plugin/review").unwrap().1, "");
        assert!(matching_skill(&skills, "plugin-other/rules.md").is_none());
    }

    #[test]
    fn encoded_traversal_and_platform_paths_are_rejected() {
        for reference in [
            "review/../secret",
            "review/%2E%2E/secret",
            "review/a/../secret",
            "/review/file",
            "review/a//file",
            "review//",
            "review/C%3A/file",
            "review/%5Csecret",
            "review/%00",
            "review/%FF",
            "review/%G0",
        ] {
            assert!(decode_reference(reference).is_err(), "{reference}");
        }
        assert!(decode_reference(&format!("review/{}", "x/".repeat(65))).is_err());
        assert!(decode_reference(&"x".repeat(MAX_REFERENCE_BYTES + 1)).is_err());
        assert_eq!(decode_reference("review/.").unwrap(), "review/.");
        assert_eq!(decode_reference("review/a+b").unwrap(), "review/a+b");
        assert_eq!(
            decode_reference("review/%252E%252E").unwrap(),
            "review/%2E%2E"
        );
    }

    #[test]
    fn directory_inventory_is_sorted_linkable_and_bounded() {
        let (_root, skill) = fixture("review");
        for index in 0..=MAX_DIRECTORY_ENTRIES {
            std::fs::write(skill.base_dir.join(format!("file-{index:04}.txt")), "x").unwrap();
        }
        let doc = resolve_resource("review/.", &skill, ".").unwrap();
        let content: Value = serde_json::from_str(&doc.content).unwrap();
        let entries = content["entries"].as_array().unwrap();
        assert_eq!(entries.len(), MAX_DIRECTORY_ENTRIES);
        assert_eq!(content["truncated"], true);
        assert_eq!(entries[0]["name"], "SKILL.md");
        assert_eq!(entries[0]["url"], "skill://review/SKILL.md");
        assert!(entries.iter().all(|entry| entry["readable"] == true));
    }

    #[test]
    fn catalog_bounds_descriptions_and_provides_encoded_urls() {
        let (_root, mut skill) = fixture("plugin/review");
        skill.description = "x".repeat(500);
        let doc = catalog("", vec![skill]).unwrap();
        let content: Value = serde_json::from_str(&doc.content).unwrap();
        assert_eq!(content["skills"][0]["url"], "skill://plugin/review");
        assert_eq!(
            content["skills"][0]["description"].as_str().unwrap().len(),
            240
        );
        assert_eq!(content["truncated"], false);
        assert_eq!(
            encode_path("review/café #1.md"),
            "review/caf%C3%A9%20%231.md"
        );
    }

    #[test]
    fn binary_and_oversized_supporting_files_fail_explicitly() {
        let (_root, skill) = fixture("review");
        std::fs::write(skill.base_dir.join("binary"), [0xff, 0xfe]).unwrap();
        assert!(
            resolve_resource("review/binary", &skill, "binary")
                .unwrap_err()
                .to_string()
                .contains("UTF-8")
        );
        let file = std::fs::File::create(skill.base_dir.join("huge")).unwrap();
        file.set_len(u64::try_from(crate::theme::MAX_RESOURCE_FILE_BYTES).unwrap() + 1)
            .unwrap();
        assert!(
            resolve_resource("review/huge", &skill, "huge")
                .unwrap_err()
                .to_string()
                .contains("resource limit")
        );
    }

    #[cfg(unix)]
    #[test]
    fn internal_symlinks_work_but_external_targets_are_not_readable() {
        use std::os::unix::fs::symlink;

        let (root, skill) = fixture("review");
        std::fs::write(root.path().join("outside"), "private outside bytes").unwrap();
        std::fs::write(skill.base_dir.join("inside"), "inside bytes").unwrap();
        symlink("inside", skill.base_dir.join("internal-link")).unwrap();
        symlink(
            root.path().join("outside"),
            skill.base_dir.join("external-link"),
        )
        .unwrap();
        symlink(root.path(), skill.base_dir.join("external-directory")).unwrap();
        assert_eq!(
            resolve_resource("review/internal-link", &skill, "internal-link")
                .unwrap()
                .content,
            "inside bytes"
        );
        for name in ["external-link", "external-directory"] {
            assert!(
                resolve_resource(&format!("review/{name}"), &skill, name)
                    .unwrap_err()
                    .to_string()
                    .contains("PI_URL_SKILL_SCOPE")
            );
        }
        let doc = resolve_resource("review/.", &skill, ".").unwrap();
        let content: Value = serde_json::from_str(&doc.content).unwrap();
        let entries = content["entries"].as_array().unwrap();
        for name in ["external-link", "external-directory"] {
            let entry = entries.iter().find(|entry| entry["name"] == name).unwrap();
            assert_eq!(entry["readable"], false);
            assert!(entry["url"].is_null());
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlink_and_fifo_swaps_cannot_redirect_or_block_supporting_reads() {
        use std::os::unix::fs::symlink;

        let (root, skill) = fixture("review");
        let base = std::fs::canonicalize(&skill.base_dir).unwrap();
        let target = base.join("target");
        let outside = root.path().join("outside");
        std::fs::write(&outside, "private outside bytes").unwrap();
        std::fs::write(&target, "authorized bytes").unwrap();
        assert!(
            read_file_with(&base, &target, || {
                std::fs::remove_file(&target).unwrap();
                symlink(&outside, &target).unwrap();
            })
            .is_err()
        );
        std::fs::remove_file(&target).unwrap();
        std::fs::write(&target, "authorized bytes").unwrap();
        assert!(
            read_file_with(&base, &target, || {
                std::fs::remove_file(&target).unwrap();
                rustix::fs::mkfifoat(
                    rustix::fs::CWD,
                    &target,
                    rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
                )
                .unwrap();
            })
            .is_err()
        );
    }

    #[test]
    fn public_read_tool_resolves_supporting_files_with_normal_pagination() {
        use crate::tools::Tool as _;

        let root = tempfile::tempdir().unwrap();
        let name = format!("uri-fixture-{}", uuid::Uuid::new_v4());
        let base = root
            .path()
            .join(crate::config::Config::project_dir())
            .join("skills")
            .join(&name);
        std::fs::create_dir_all(base.join("references")).unwrap();
        std::fs::write(
            base.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: supporting files\n---\ninstructions\n"),
        )
        .unwrap();
        std::fs::write(base.join("references/check.md"), "first\nsecond\nthird\n").unwrap();
        let reader = crate::tools::ReadTool::new(root.path());
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let output = runtime
            .block_on(reader.execute(
                "skill-support-read",
                json!({
                    "path":format!("skill://{name}/references/check.md"),
                    "offset":2, "limit":1,
                }),
                None,
            ))
            .unwrap();
        assert!(!output.is_error);
        assert!(output.content.iter().any(|block| matches!(
            block, crate::model::ContentBlock::Text(text) if text.text.contains("second")
        )));
        assert!(!output.content.iter().any(|block| matches!(
            block, crate::model::ContentBlock::Text(text) if text.text.contains("first")
        )));
        let bare = super::super::resolve(&format!("skill://{name}"), root.path()).unwrap();
        assert!(bare.content.contains("instructions"));
        let directory = super::super::resolve_with(
            &format!("skill://{name}/references"),
            root.path(),
            &super::super::ResolveOptions::default(),
        )
        .unwrap();
        assert_eq!(directory.metadata["kind"], "directory");
    }
}
