//! Sessions pinned to the top of the `/resume` list (OMP `/pin`).
//!
//! The pinned session ids live in `<agent dir>/pinned-sessions.json` as a
//! JSON array. A missing or unreadable file means nothing is pinned, and a
//! pin whose session no longer exists changes nothing.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

const PINS_FILE: &str = "pinned-sessions.json";

fn pins_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join(PINS_FILE)
}

/// The pinned session ids.
pub fn load_pinned(agent_dir: &Path) -> HashSet<String> {
    std::fs::read_to_string(pins_path(agent_dir))
        .ok()
        .and_then(|text| serde_json::from_str::<Vec<String>>(&text).ok())
        .map(|ids| ids.into_iter().collect())
        .unwrap_or_default()
}

/// Pin `session_id`, or unpin it if it was pinned; returns whether it is
/// pinned now. The file is replaced atomically (write a temp file, rename).
pub fn toggle_pin(agent_dir: &Path, session_id: &str) -> std::io::Result<bool> {
    use std::io::Write as _;
    std::fs::create_dir_all(agent_dir)?;
    let mut pinned = load_pinned(agent_dir);
    if !pinned.remove(session_id) {
        pinned.insert(session_id.to_string());
    }
    let mut ids: Vec<&String> = pinned.iter().collect();
    ids.sort();
    let json = serde_json::to_string_pretty(&ids).map_err(std::io::Error::other)?;
    let path = pins_path(agent_dir);
    let tmp = agent_dir.join(format!("{PINS_FILE}.{}.tmp", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(&tmp)?.write_all(json.as_bytes())?;
    std::fs::rename(&tmp, &path)?;
    Ok(pinned.contains(session_id))
}

/// Stable partition: pinned sessions first, each group keeping the caller's
/// order (recency).
pub fn sort_pinned_first<T, S: std::hash::BuildHasher>(
    items: Vec<T>,
    pinned: &HashSet<String, S>,
    id: impl Fn(&T) -> &str,
) -> Vec<T> {
    if pinned.is_empty() {
        return items;
    }
    let (mut top, rest): (Vec<T>, Vec<T>) = items
        .into_iter()
        .partition(|item| pinned.contains(id(item)));
    top.extend(rest);
    top
}

/// The `/resume` picker rows for `cwd`, newest first with pinned sessions
/// on top and marked, as `(label, session path)`.
///
/// Listed as the classic picker lists them: under `session_dir`
/// (`--session-dir`) when set, without files deleted since they were
/// indexed, one row per session. Read each time, so sessions saved and pins
/// set during this run show up.
pub fn resume_entries(
    cwd: &Path,
    session_dir: Option<&Path>,
    pins_dir: &Path,
) -> Vec<(String, String)> {
    let pinned = load_pinned(pins_dir);
    sort_pinned_first(
        crate::session_picker::list_sessions_for_project(cwd, session_dir),
        &pinned,
        |meta| meta.id.as_str(),
    )
    .into_iter()
    .map(|meta| {
        let pin = if pinned.contains(&meta.id) {
            "📌 "
        } else {
            ""
        };
        let label = match &meta.name {
            Some(name) => format!("{pin}{name} · {} msgs", meta.message_count),
            None => format!("{pin}{} · {} msgs", meta.id, meta.message_count),
        };
        (label, meta.path)
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_toggle_persist_and_sort_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(load_pinned(dir.path()).is_empty());
        assert!(toggle_pin(dir.path(), "b").expect("pin b"));
        assert!(toggle_pin(dir.path(), "d").expect("pin d"));
        assert_eq!(
            load_pinned(dir.path()),
            HashSet::from([String::from("b"), String::from("d")])
        );
        assert!(!toggle_pin(dir.path(), "d").expect("unpin d"));
        let pinned = load_pinned(dir.path());
        assert_eq!(pinned, HashSet::from([String::from("b")]));

        let sessions = vec!["a", "b", "c", "gone"];
        assert_eq!(
            sort_pinned_first(sessions, &pinned, |s| *s),
            ["b", "a", "c", "gone"]
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(pins_path(dir.path()))
                .expect("stat")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        // A corrupt file reads as nothing pinned rather than failing.
        std::fs::write(pins_path(dir.path()), "not json").expect("write");
        assert!(load_pinned(dir.path()).is_empty());
    }

    /// The FTUI list read the default sessions directory, so sessions saved
    /// under `--session-dir` never showed up, and it kept deleted files.
    #[test]
    fn resume_entries_read_the_session_dir_and_drop_deleted_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base_dir = tmp.path().join("sessions");
        let cwd = tmp.path().join("repo");
        let project_dir = base_dir.join(crate::session::encode_cwd(&cwd));
        std::fs::create_dir_all(&project_dir).expect("project sessions");
        let mut header = crate::session::SessionHeader::new();
        header.id = "kept".to_string();
        header.cwd = cwd.display().to_string();
        let path = project_dir.join("kept.jsonl");
        std::fs::write(
            &path,
            format!(
                "{}\n{{\"type\":\"message\"}}\n",
                serde_json::to_string(&header).expect("header")
            ),
        )
        .expect("write session");

        let rows = resume_entries(&cwd, Some(&base_dir), tmp.path());
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].1, path.display().to_string());

        std::fs::remove_file(&path).expect("delete session");
        let rows = resume_entries(&cwd, Some(&base_dir), tmp.path());
        assert!(rows.is_empty(), "{rows:?}"); // ubs:ignore[rust.panic.assert-macros] -- test assertion
    }
}
