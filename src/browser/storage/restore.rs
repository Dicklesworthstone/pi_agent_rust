//! Restore explicit private state through the same page-bound native transport.
//!
//! Preflight completes before mutations. Restore merges named cookies and keys;
//! it never clears the profile, navigates, retries, or claims application login.

use super::{
    Cookie, Document, MAX_STATE_BYTES, State, StorageEntry, checkpoint, cookies_from_response,
    error, storage_from_response, validate_path,
};
use crate::agent_cx::AgentCx;
use crate::browser::{cdp::Cdp, output, required};
use crate::error::Result;
use crate::tools::ToolOutput;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

#[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
fn read_private(owner: &AgentCx, cwd: &Path, requested: &str) -> Result<Vec<u8>> {
    use rustix::fs::{Mode, OFlags};
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::Component;

    checkpoint(owner)?;
    validate_path(requested)?;
    let root = std::fs::canonicalize(cwd)
        .map_err(|_| error("BROWSER_STATE_FILE: cannot resolve workspace"))?;
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut directory = rustix::fs::open(&root, flags, Mode::empty())
        .map_err(|_| error("BROWSER_STATE_FILE: cannot pin workspace directory"))?;
    let mut components = Path::new(requested)
        .components()
        .filter_map(|component| {
            if let Component::Normal(name) = component {
                Some(name)
            } else {
                None
            }
        })
        .peekable();
    while let Some(name) = components.next() {
        checkpoint(owner)?;
        if components.peek().is_some() {
            directory =
                rustix::fs::openat(&directory, name, flags, Mode::empty()).map_err(|_| {
                    error(
                        "BROWSER_STATE_FILE: state directory is missing, inaccessible or symbolic",
                    )
                })?;
            continue;
        }
        // NONBLOCK prevents a FIFO from suspending the owner before metadata
        // admission. All reads use this no-follow descriptor, never reopen paths.
        let descriptor = rustix::fs::openat(
            &directory,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(|_| {
            error("BROWSER_STATE_FILE: state file is missing, inaccessible or symbolic")
        })?;
        let mut file = std::fs::File::from(descriptor);
        let before = file
            .metadata()
            .map_err(|_| error("BROWSER_STATE_FILE: cannot inspect state file"))?;
        if !before.is_file()
            || before.nlink() != 1
            || before.permissions().mode() & 0o077 != 0
            || before.len() == 0
            || before.len() > MAX_STATE_BYTES as u64
        {
            return Err(error(
                "BROWSER_STATE_FILE: state must be a private, singly linked regular file of 1 byte to 2 MiB; group/world permissions are forbidden",
            ));
        }
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 8192];
        loop {
            checkpoint(owner)?;
            let read = match file.read(&mut buffer) {
                Ok(read) => read,
                Err(failure) if failure.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(error("BROWSER_STATE_FILE: state read failed")),
            };
            if read == 0 {
                break;
            }
            if read > MAX_STATE_BYTES.saturating_sub(bytes.len()) {
                return Err(error("BROWSER_STATE_LIMIT: state file exceeds 2 MiB"));
            }
            bytes.extend_from_slice(&buffer[..read]);
        }
        let after = file
            .metadata()
            .map_err(|_| error("BROWSER_STATE_FILE: cannot recheck state file"))?;
        if after.len() != before.len()
            || after.len() != bytes.len() as u64
            || after.mtime() != before.mtime()
            || after.mtime_nsec() != before.mtime_nsec()
            || after.ctime() != before.ctime()
            || after.ctime_nsec() != before.ctime_nsec()
            || after.nlink() != 1
            || after.permissions().mode() & 0o077 != 0
        {
            return Err(error(
                "BROWSER_STATE_FILE_CHANGED: state file changed during the read",
            ));
        }
        checkpoint(owner)?;
        return Ok(bytes);
    }
    Err(error("BROWSER_STATE_FILE: state_path does not name a file"))
}

#[cfg(not(all(unix, not(any(target_os = "espidf", target_os = "redox")))))]
fn read_private(_owner: &AgentCx, _cwd: &Path, _requested: &str) -> Result<Vec<u8>> {
    Err(error(
        "BROWSER_STATE_PLATFORM: private browser-state persistence requires the native Unix file backend",
    ))
}

fn decode(
    bytes: &[u8],
    document: &Document,
    allowlist: Option<&[String]>,
    now: f64,
) -> Result<State> {
    if bytes.is_empty() || bytes.len() > MAX_STATE_BYTES {
        return Err(error(
            "BROWSER_STATE_LIMIT: state file must contain 1 byte to 2 MiB",
        ));
    }
    let state: State = serde_json::from_slice(bytes)
        .map_err(|_| error("BROWSER_STATE_INVALID: invalid native storage-state JSON; duplicate and unknown fields are not accepted"))?;
    state.validate(document, allowlist)?;
    check_expiry(&state, now)?;
    Ok(state)
}

fn check_expiry(state: &State, now: f64) -> Result<()> {
    if !now.is_finite()
        || now < 0.0
        || state
            .cookies
            .iter()
            .any(|cookie| cookie.expires.is_some_and(|expiry| expiry <= now))
    {
        return Err(error(
            "BROWSER_STATE_EXPIRED: saved cookies have expired or the current time is unavailable; no restore was admitted",
        ));
    }
    Ok(())
}

fn now() -> Result<f64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .map_err(|_| error("BROWSER_STATE_CLOCK: cannot determine cookie expiry"))
}

/// A CookieParam must not convert a host-only cookie to a domain cookie. A URL
/// supplies the host and source port when the Domain attribute was absent.
fn cookie_parameter(cookie: &Cookie, document: &Document) -> Result<Value> {
    let mut value = serde_json::to_value(cookie)
        .map_err(|_| error("BROWSER_STATE_INVALID: cookie could not be serialized"))?;
    if !cookie.domain.starts_with('.') {
        value
            .as_object_mut()
            .expect("serialized cookie object")
            .remove("domain");
        value["url"] = json!(format!("{}/", document.origin));
    }
    Ok(value)
}

fn same_cookie(expected: &Cookie, actual: &Cookie) -> bool {
    expected.name == actual.name
        && expected.value == actual.value
        && expected.domain == actual.domain
        && expected.path == actual.path
        && expected.secure == actual.secure
        && expected.http_only == actual.http_only
        && expected.same_site == actual.same_site
        && expected
            .priority
            .as_ref()
            .is_none_or(|value| actual.priority.as_ref() == Some(value))
        && expected
            .source_scheme
            .as_ref()
            .is_none_or(|value| actual.source_scheme.as_ref() == Some(value))
        && expected
            .source_port
            .is_none_or(|value| actual.source_port == Some(value))
        && match (expected.expires, actual.expires) {
            (None, None) => true,
            (Some(expected), Some(actual)) => (expected - actual).abs() <= 0.001,
            _ => false,
        }
}

fn verify(state: &State, cookies: &[Cookie], entries: &[StorageEntry]) -> Result<()> {
    let actual: BTreeMap<_, _> = cookies
        .iter()
        .map(|cookie| ((&cookie.name, &cookie.domain, &cookie.path), cookie))
        .collect();
    let storage: BTreeMap<_, _> = entries
        .iter()
        .map(|entry| (&entry.name, &entry.value))
        .collect();
    if actual.len() != cookies.len()
        || storage.len() != entries.len()
        || state.cookies.iter().any(|cookie| {
            actual
                .get(&(&cookie.name, &cookie.domain, &cookie.path))
                .is_none_or(|actual| !same_cookie(cookie, actual))
        })
        || state
            .local_storage
            .iter()
            .any(|entry| storage.get(&entry.name) != Some(&&entry.value))
    {
        return Err(error(
            "BROWSER_STATE_VERIFY: browser did not retain the requested cookie and localStorage values/attributes",
        ));
    }
    Ok(())
}

async fn read_current(
    owner: &AgentCx,
    cdp: &mut Cdp,
    document: &Document,
    allowlist: Option<&[String]>,
) -> Result<(Vec<Cookie>, Vec<StorageEntry>)> {
    document.check(owner, cdp, allowlist).await?;
    // Page-scoped Storage commands select this target's storage partition.
    // Never fall back to the default browser context on an error.
    let response = cdp
        .command(owner, "Storage.getCookies", json!({}))
        .await
        .map_err(|_| error("BROWSER_STATE_COOKIE: cookie inspection failed"))?;
    let cookies = cookies_from_response(&response, document)?;
    for cookie in &cookies {
        cookie.validate(document, allowlist)?;
    }
    document.check(owner, cdp, allowlist).await?;
    let response = cdp
        .command(
            owner,
            "DOMStorage.getDOMStorageItems",
            json!({"storageId":document.storage_id()}),
        )
        .await
        .map_err(|_| error("BROWSER_STATE_STORAGE: localStorage inspection failed"))?;
    let entries = storage_from_response(&response)?;
    document.check(owner, cdp, allowlist).await?;
    Ok((cookies, entries))
}

async fn apply(
    owner: &AgentCx,
    cdp: &mut Cdp,
    document: &Document,
    state: &State,
    parameters: Vec<Value>,
    allowlist: Option<&[String]>,
) -> Result<()> {
    document.check(owner, cdp, allowlist).await?;
    check_expiry(state, now()?)?;
    if !parameters.is_empty() {
        cdp.command(owner, "Storage.setCookies", json!({"cookies":parameters}))
            .await
            .map_err(|_| error("BROWSER_STATE_COOKIE: cookie write failed"))?;
    }
    for entry in &state.local_storage {
        document.check(owner, cdp, allowlist).await?;
        cdp.command(
            owner,
            "DOMStorage.setDOMStorageItem",
            json!({"storageId":document.storage_id(), "key":entry.name, "value":entry.value}),
        )
        .await
        .map_err(|_| error("BROWSER_STATE_STORAGE: localStorage write failed"))?;
    }
    let (cookies, entries) = read_current(owner, cdp, document, allowlist).await?;
    verify(state, &cookies, &entries)?;
    check_expiry(state, now()?)?;
    document.check(owner, cdp, allowlist).await
}

pub(super) async fn execute(
    owner: &AgentCx,
    cdp: &mut Cdp,
    cwd: &Path,
    tab: &str,
    allowlist: Option<&[String]>,
    args: &Value,
) -> Result<ToolOutput> {
    let bytes = read_private(owner, cwd, required(args, "state_path")?)?;
    let document = Document::capture(owner, cdp, allowlist).await?;
    let state = decode(&bytes, &document, allowlist, now()?)?;
    let parameters = state
        .cookies
        .iter()
        .map(|cookie| cookie_parameter(cookie, &document))
        .collect::<Result<Vec<_>>>()?;
    // Inspect both stores before mutating either. Unsupported existing cookie
    // scope and unavailable localStorage fail before a cookie can be installed.
    read_current(owner, cdp, &document, allowlist).await?;
    checkpoint(owner)?;
    apply(owner, cdp, &document, &state, parameters, allowlist).await
        .map_err(|_| error("BROWSER_STATE_PARTIAL: restore or read-back verification failed; some cookies or storage keys may already have changed. Nothing was automatically retried or rolled back; inspect the selected origin before continuing"))?;
    Ok(output(
        format!(
            "Restored and read-back verified private browser state for tab {tab}. Existing unrelated cookies and storage keys were not cleared. Reload or navigate explicitly to use the state; application authentication is not verified."
        ),
        json!({"action":"restore_state", "tab":tab, "backend":"cdp", "schema":super::SCHEMA,
            "origin":document.origin, "cookie_count":state.cookies.len(),
            "local_storage_count":state.local_storage.len(), "storage_verified":true,
            "authentication_verified":false, "cleared_existing_state":false}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document() -> Document {
        Document::from_frame(&json!({"id":"frame", "loaderId":"loader", "url":"https://example.com/app", "securityOrigin":"https://example.com"}), None).unwrap()
    }

    fn state() -> State {
        serde_json::from_value(json!({"schema":super::super::SCHEMA,
            "origin":"https://example.com", "cookies":[{
                "name":"auth", "value":"PRIVATE-CANARY", "domain":"example.com", "path":"/",
                "secure":true, "httpOnly":true, "sameSite":"Lax", "expires":1_900_000_000.0
            }], "localStorage":[{"name":"token", "value":"STORAGE-CANARY"}]}))
        .unwrap()
    }

    #[test]
    fn restore_requires_exact_schema_origin_and_nonexpired_cookies() {
        let bytes = state().encode().unwrap();
        assert!(decode(&bytes, &document(), None, 1_800_000_000.0).is_ok());
        for time in [1_900_000_000.0, 2_000_000_000.0, f64::NAN, -1.0] {
            assert!(decode(&bytes, &document(), None, time).is_err());
        }
        for origin in [
            "http://example.com",
            "https://example.com:8443",
            "https://other.test",
        ] {
            let mut changed = state();
            changed.origin = origin.into();
            assert!(
                decode(
                    &changed.encode().unwrap(),
                    &document(),
                    None,
                    1_800_000_000.0
                )
                .is_err()
            );
        }
        for bytes in [b"PRIVATE-CANARY".as_slice(), b"{}", b""] {
            let failure = decode(bytes, &document(), None, 1_800_000_000.0)
                .err()
                .unwrap();
            assert!(!failure.to_string().contains("PRIVATE-CANARY"));
        }
    }

    #[test]
    fn restore_verification_requires_values_attributes_and_session_semantics() {
        let wanted = state();
        assert!(verify(&wanted, &wanted.cookies, &wanted.local_storage).is_ok());
        let mut changed = state();
        changed.cookies[0].value = "wrong".into();
        assert!(verify(&wanted, &changed.cookies, &changed.local_storage).is_err());
        changed = state();
        changed.cookies[0].http_only = false;
        assert!(verify(&wanted, &changed.cookies, &changed.local_storage).is_err());
        changed = state();
        changed.cookies[0].expires = None;
        assert!(verify(&wanted, &changed.cookies, &changed.local_storage).is_err());
        changed = state();
        changed.local_storage[0].value = "wrong".into();
        let failure = verify(&wanted, &changed.cookies, &changed.local_storage).unwrap_err();
        assert!(!failure.to_string().contains("CANARY"));
        assert!(verify(&wanted, &[], &wanted.local_storage).is_err());
    }

    #[test]
    fn verification_allows_unrelated_state_but_rejects_duplicate_identities() {
        let wanted = state();
        let mut actual = state();
        actual.local_storage.push(StorageEntry {
            name: "unrelated".into(),
            value: "keep".into(),
        });
        let mut cookie = actual.cookies[0].clone();
        cookie.name = "other".into();
        actual.cookies.push(cookie);
        assert!(verify(&wanted, &actual.cookies, &actual.local_storage).is_ok());
        actual.local_storage.push(actual.local_storage[0].clone());
        assert!(verify(&wanted, &actual.cookies, &actual.local_storage).is_err());
    }

    #[test]
    fn cookie_parameters_preserve_host_only_and_domain_scopes() {
        let mut cookie = state().cookies.remove(0);
        let host = cookie_parameter(&cookie, &document()).unwrap();
        assert!(host.get("domain").is_none());
        assert_eq!(host["url"], "https://example.com/");
        assert_eq!(host["httpOnly"], true);
        cookie.domain = ".example.com".into();
        cookie.expires = None;
        let domain = cookie_parameter(&cookie, &document()).unwrap();
        assert_eq!(domain["domain"], ".example.com");
        assert!(domain.get("url").is_none());
        assert!(domain.get("expires").is_none());
    }

    #[test]
    fn restored_cookie_cannot_exceed_an_exact_host_allowlist() {
        let doc = Document::from_frame(&json!({"id":"f", "loaderId":"l", "url":"https://app.example.com/", "securityOrigin":"https://app.example.com"}), None).unwrap();
        let mut stored = state();
        stored.origin = doc.origin.clone();
        stored.cookies[0].domain = ".example.com".into();
        let allowlist = vec!["app.example.com".to_owned()];
        assert!(stored.validate(&doc, Some(&allowlist)).is_err());
    }

    #[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
    #[test]
    fn private_state_reader_uses_real_files_and_rejects_escape_links_and_permissions() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let owner = AgentCx::for_current_or_request();
        let root = tempfile::tempdir().unwrap();
        let target =
            crate::artifact_output::resolve_new(root.path(), "private/state.json", "browser")
                .unwrap();
        let bytes = state().encode().unwrap();
        crate::artifact_output::publish(&target, &bytes, "browser").unwrap();
        assert_eq!(
            read_private(&owner, root.path(), "private/state.json").unwrap(),
            bytes
        );
        for path in [
            "../state.json",
            "/state.json",
            "private\\state.json",
            "missing.json",
        ] {
            assert!(read_private(&owner, root.path(), path).is_err());
        }
        symlink(target.path(), root.path().join("link.json")).unwrap();
        symlink(root.path().join("private"), root.path().join("linkdir")).unwrap();
        assert!(read_private(&owner, root.path(), "link.json").is_err());
        assert!(read_private(&owner, root.path(), "linkdir/state.json").is_err());
        std::fs::set_permissions(target.path(), std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_private(&owner, root.path(), "private/state.json").is_err());
        std::fs::set_permissions(target.path(), std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::hard_link(target.path(), root.path().join("hard.json")).unwrap();
        assert!(read_private(&owner, root.path(), "hard.json").is_err());
        assert!(read_private(&owner, root.path(), "private/state.json").is_err());
    }

    #[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
    #[test]
    fn private_reader_rejects_empty_oversized_and_nonregular_files() {
        use std::os::unix::fs::PermissionsExt;
        let owner = AgentCx::for_current_or_request();
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state.json");
        let file = std::fs::File::create(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(read_private(&owner, root.path(), "state.json").is_err());
        file.set_len(MAX_STATE_BYTES as u64 + 1).unwrap();
        assert!(read_private(&owner, root.path(), "state.json").is_err());
        std::fs::create_dir(root.path().join("directory.json")).unwrap();
        assert!(read_private(&owner, root.path(), "directory.json").is_err());
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            root.path().join("pipe.json"),
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .unwrap();
        assert!(read_private(&owner, root.path(), "pipe.json").is_err());
    }

    #[test]
    fn restore_is_publicly_discoverable_and_mock_never_reports_success() {
        use crate::tools::Tool;
        let root = tempfile::tempdir().unwrap();
        let tool = crate::browser::BrowserTool::new(root.path()).with_mock(true);
        let schema = tool.parameters();
        assert!(
            schema["properties"]["action"]["enum"]
                .as_array()
                .unwrap()
                .contains(&json!("restore_state"))
        );
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        assert!(
            runtime
                .block_on(tool.execute(
                    "restore",
                    json!({"action":"restore_state", "tab":"work", "state_path":"state.json"}),
                    None
                ))
                .is_err()
        );
    }
}
