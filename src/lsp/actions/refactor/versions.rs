//! Track request-time document identity through ordered resource operations.
//!
//! Moving a document does not mint a new version. Creating or overwriting it
//! does not inherit the old occupant's version. No filesystem reads or writes
//! are performed here: when ignoreIfExists depends on an unobserved path, a
//! versioned edit is rejected rather than guessing which document survived.

use super::{DocumentSnapshot, HashMap, PathBuf, Result, Value, tool_err, uri_to_path};

#[derive(Clone, Debug)]
enum Identity {
    /// The document synchronized at this path before the request began.
    Original(PathBuf),
    /// Created during this edit; there is no request-time document version.
    Fresh,
    /// Removed or moved away during this edit.
    Absent,
    /// Unobserved, or a conditional operation has more than one possible result.
    Unknown,
}

fn path(entry: &Value, key: &str) -> Result<PathBuf> {
    entry
        .get(key)
        .and_then(Value::as_str)
        .and_then(uri_to_path)
        .ok_or_else(|| tool_err("LSP_EDIT_MALFORMED", "invalid resource or document URI"))
}

fn ignored_if_present(entry: &Value) -> bool {
    entry
        .pointer("/options/ignoreIfExists")
        .and_then(Value::as_bool)
        == Some(true)
        && entry.pointer("/options/overwrite").and_then(Value::as_bool) != Some(true)
}

pub(super) fn validate(
    raw: &Value,
    requested: &HashMap<PathBuf, DocumentSnapshot>,
    current: &HashMap<PathBuf, DocumentSnapshot>,
) -> Result<()> {
    let Some(changes) = raw.get("documentChanges").and_then(Value::as_array) else {
        return Ok(());
    };
    let mut identities: HashMap<_, _> = requested
        .keys()
        .map(|path| (path.clone(), Identity::Original(path.clone())))
        .collect();
    for change in changes {
        match change.get("kind").and_then(Value::as_str) {
            Some("create") => {
                let target = path(change, "uri")?;
                let identity = if ignored_if_present(change) {
                    match identities.get(&target) {
                        Some(identity @ (Identity::Original(_) | Identity::Fresh)) => {
                            identity.clone()
                        }
                        Some(Identity::Absent) => Identity::Fresh,
                        _ => Identity::Unknown,
                    }
                } else {
                    Identity::Fresh
                };
                identities.insert(target, identity);
            }
            Some("delete") => {
                identities.insert(path(change, "uri")?, Identity::Absent);
            }
            Some("rename") => {
                let old = path(change, "oldUri")?;
                let new = path(change, "newUri")?;
                if old == new {
                    continue;
                }
                if ignored_if_present(change) {
                    match identities.get(&new) {
                        Some(Identity::Original(_) | Identity::Fresh) => continue,
                        Some(Identity::Absent) => {}
                        _ => {
                            identities.insert(old, Identity::Unknown);
                            identities.insert(new, Identity::Unknown);
                            continue;
                        }
                    }
                }
                let identity = identities
                    .insert(old, Identity::Absent)
                    .unwrap_or(Identity::Unknown);
                identities.insert(new, identity);
            }
            // The caller has already validated the complete WorkspaceEdit.
            // Retain a fail-closed guard if that admission order ever changes.
            Some(_) => return Err(tool_err("LSP_EDIT_MALFORMED", "unknown resource operation")),
            None => {
                let Some(document) = change.get("textDocument") else {
                    return Err(tool_err("LSP_EDIT_MALFORMED", "missing textDocument"));
                };
                let Some(version) = document.get("version").filter(|value| !value.is_null()) else {
                    continue;
                };
                let target = path(document, "uri")?;
                let origin = match identities.get(&target) {
                    Some(Identity::Original(origin)) => Some(origin),
                    _ => None,
                };
                let matches = version
                    .as_u64()
                    .filter(|value| *value > 0 && *value <= i32::MAX as u64)
                    .zip(origin)
                    .and_then(|(version, origin)| {
                        requested
                            .get(origin)
                            .zip(current.get(origin))
                            .map(|(before, now)| (version, before, now))
                    })
                    .is_some_and(|(version, before, now)| {
                        before.version == version
                            && now.version == version
                            && before.hash == now.hash
                    });
                if !matches {
                    return Err(tool_err(
                        "LSP_EDIT_CONFLICT",
                        "server edit does not match the requested document version or request-time identity",
                    ));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
