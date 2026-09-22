//! Linux descriptor-scoped metadata preservation for atomic replacement.
//!
//! Permission bits alone are not an ACL: a newly created sibling may inherit
//! different named-user grants from its directory. Copy the old inode's
//! bounded xattr snapshot, remove inherited extras, and verify the exact result
//! before syncing or publishing. Any unsupported copy fails without changing
//! the original file. Linux executable capabilities must not survive changed
//! executable bytes; IMA/EVM-signed objects require a separate signing workflow.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs::File;
use std::io;

use rustix::fs::{XattrFlags, fgetxattr, flistxattr, fremovexattr, fsetxattr};

const NAMES_LIMIT: usize = 64 * 1024;
const VALUE_LIMIT: usize = 64 * 1024;
const TOTAL_LIMIT: usize = 1024 * 1024;
const COUNT_LIMIT: usize = 256;
const CAPABILITIES: &[u8] = b"security.capability";

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Snapshot {
    values: BTreeMap<Vec<u8>, Vec<u8>>,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn attribute_name(bytes: &[u8]) -> io::Result<CString> {
    CString::new(bytes).map_err(|_| invalid("invalid extended-attribute name"))
}

impl Snapshot {
    pub(super) fn read(file: &File) -> io::Result<Self> {
        Self::read_bounded(file, VALUE_LIMIT, TOTAL_LIMIT)
    }

    fn read_bounded(file: &File, value_limit: usize, total_limit: usize) -> io::Result<Self> {
        // Fixed nonempty buffers avoid an unbounded size-query/reallocate loop
        // while the inode is being edited. ERANGE and races fail closed.
        let mut names = vec![0_u8; NAMES_LIMIT];
        let length = match flistxattr(file, names.as_mut_slice()).map_err(io::Error::from) {
            Ok(length) => length,
            Err(error) if error.kind() == io::ErrorKind::Unsupported => 0,
            Err(error) => return Err(error),
        };
        if length > names.len() || (length > 0 && names[length - 1] != 0) {
            return Err(invalid("invalid extended-attribute name list"));
        }
        names.truncate(length);
        let mut total = names.len();
        if total > total_limit {
            return Err(invalid("extended-attribute snapshot exceeds byte limit"));
        }
        let mut values = BTreeMap::new();
        let mut buffer = vec![0_u8; value_limit.max(1)];
        for name in names.split_inclusive(|byte| *byte == 0) {
            let name = &name[..name.len() - 1];
            if name.is_empty() || values.len() >= COUNT_LIMIT || values.contains_key(name) {
                return Err(invalid("invalid or oversized extended-attribute name list"));
            }
            let key = attribute_name(name)?;
            let count = fgetxattr(file, key.as_c_str(), buffer.as_mut_slice())?;
            if count > value_limit || count > buffer.len() {
                return Err(invalid("extended-attribute value exceeds byte limit"));
            }
            total = total
                .checked_add(count)
                .ok_or_else(|| invalid("metadata size overflow"))?;
            if total > total_limit {
                return Err(invalid("extended-attribute snapshot exceeds byte limit"));
            }
            values.insert(name.to_vec(), buffer[..count].to_vec());
        }
        Ok(Self { values })
    }

    pub(super) fn verify_source(&self, file: &File) -> io::Result<()> {
        if self != &Self::read(file)? {
            return Err(super::conflict());
        }
        Ok(())
    }

    pub(super) fn install(&self, stage: &File) -> io::Result<()> {
        if self.values.contains_key(b"security.ima".as_slice())
            || self.values.contains_key(b"security.evm".as_slice())
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "integrity-signed files require a metadata-aware signing workflow",
            ));
        }
        let current = Self::read(stage)?;
        // A parent default ACL must not become the replacement's access ACL
        // when the original has none. Apply this before copying the exact ACL.
        for name in current.values.keys() {
            if !self.values.contains_key(name) || name.as_slice() == CAPABILITIES {
                fremovexattr(stage, attribute_name(name)?.as_c_str())?;
            }
        }
        for (name, value) in &self.values {
            if name.as_slice() == CAPABILITIES || current.values.get(name) == Some(value) {
                continue;
            }
            fsetxattr(
                stage,
                attribute_name(name)?.as_c_str(),
                value,
                XattrFlags::empty(),
            )?;
        }
        self.verify_installed(stage)
    }

    pub(super) fn verify_installed(&self, stage: &File) -> io::Result<()> {
        let actual = Self::read(stage)?;
        let expected_count =
            self.values.len() - usize::from(self.values.contains_key(CAPABILITIES));
        if actual.values.len() != expected_count
            || actual.values.iter().any(|(name, value)| {
                name.as_slice() == CAPABILITIES || self.values.get(name) != Some(value)
            })
        {
            return Err(invalid("extended metadata changed during staging"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Phase, STAGE_PREFIX, write, write_with_hook};
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::path::PathBuf;

    fn fixture() -> Option<(tempfile::TempDir, PathBuf)> {
        let temp = tempfile::tempdir().unwrap();
        let path = fs::canonicalize(temp.path()).unwrap().join("target");
        fs::write(&path, b"original").unwrap();
        let file = File::open(&path).unwrap();
        match fsetxattr(&file, c"user.pi_test", b"value", XattrFlags::empty()) {
            Ok(()) => Some((temp, path)),
            Err(error) if io::Error::from(error).kind() == io::ErrorKind::Unsupported => {
                eprintln!("SKIP: test filesystem does not support Linux extended attributes");
                None
            }
            Err(error) => panic!("extended-attribute fixture failed: {error}"),
        }
    }

    #[test]
    fn replacement_preserves_binary_and_empty_extended_attributes() {
        let Some((_temp, path)) = fixture() else {
            return;
        };
        let file = File::open(&path).unwrap();
        fsetxattr(&file, c"user.binary", &[0, 255, 1, 0], XattrFlags::empty()).unwrap();
        fsetxattr(&file, c"user.empty", &[], XattrFlags::empty()).unwrap();
        let expected = Snapshot::read(&file).unwrap();
        write(&json!({"data": "replacement"}), &path).unwrap();
        let actual = Snapshot::read(&File::open(&path).unwrap()).unwrap();
        assert_eq!(actual, expected);
        assert_eq!(fs::read(&path).unwrap(), b"replacement");
    }

    #[test]
    fn replacement_preserves_a_named_user_posix_acl_exactly() {
        use std::os::unix::fs::PermissionsExt as _;
        let Some((_temp, path)) = fixture() else {
            return;
        };
        let file = File::open(&path).unwrap();
        // Linux UAPI posix_acl_xattr: version 2, followed by LE tag/perm/id
        // entries. A named user is explicitly denied while group read remains.
        let mut acl = 2_u32.to_le_bytes().to_vec();
        for (tag, permissions, id) in [
            (1_u16, 6_u16, u32::MAX),
            (2, 0, 65534),
            (4, 4, u32::MAX),
            (16, 4, u32::MAX),
            (32, 0, u32::MAX),
        ] {
            acl.extend_from_slice(&tag.to_le_bytes());
            acl.extend_from_slice(&permissions.to_le_bytes());
            acl.extend_from_slice(&id.to_le_bytes());
        }
        fsetxattr(&file, c"system.posix_acl_access", &acl, XattrFlags::empty()).unwrap();
        let expected = Snapshot::read(&file).unwrap();
        assert_eq!(expected.values[b"system.posix_acl_access".as_slice()], acl);
        write(&json!({"data": "replacement"}), &path).unwrap();
        let new = File::open(&path).unwrap();
        assert_eq!(Snapshot::read(&new).unwrap(), expected);
        assert_eq!(new.metadata().unwrap().permissions().mode() & 0o777, 0o640);
    }

    #[test]
    fn inherited_stage_attributes_are_removed_not_leaked_into_the_target() {
        let Some((temp, path)) = fixture() else {
            return;
        };
        let expected = Snapshot::read(&File::open(&path).unwrap()).unwrap();
        write_with_hook(&path, b"replacement", |phase| {
            if phase == Phase::Metadata {
                let stage = fs::read_dir(temp.path())?
                    .map(|entry| entry.unwrap().path())
                    .find(|path| {
                        path.file_name()
                            .unwrap()
                            .to_string_lossy()
                            .starts_with(STAGE_PREFIX)
                    })
                    .expect("owned stage");
                fsetxattr(
                    &File::open(stage)?,
                    c"user.inherited",
                    b"unexpected",
                    XattrFlags::empty(),
                )?;
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(
            Snapshot::read(&File::open(&path).unwrap()).unwrap(),
            expected
        );
    }

    #[test]
    fn source_attribute_edits_during_staging_are_not_overwritten() {
        let Some((_temp, path)) = fixture() else {
            return;
        };
        let error = write_with_hook(&path, b"replacement", |phase| {
            if phase == Phase::Publish {
                fsetxattr(
                    &File::open(&path)?,
                    c"user.pi_test",
                    b"external",
                    XattrFlags::empty(),
                )?;
            }
            Ok(())
        })
        .unwrap_err();
        assert_eq!(error.details.unwrap()["commit_state"], "not_committed");
        assert_eq!(fs::read(&path).unwrap(), b"original");
        let actual = Snapshot::read(&File::open(&path).unwrap()).unwrap();
        assert_eq!(actual.values[b"user.pi_test".as_slice()], b"external");
    }

    #[test]
    fn snapshot_bounds_fail_closed_without_mutating_source_attributes() {
        let Some((_temp, path)) = fixture() else {
            return;
        };
        let file = File::open(&path).unwrap();
        let expected = Snapshot::read(&file).unwrap();
        assert!(Snapshot::read_bounded(&file, 2, TOTAL_LIMIT).is_err());
        assert!(Snapshot::read_bounded(&file, VALUE_LIMIT, 2).is_err());
        assert_eq!(Snapshot::read(&file).unwrap(), expected);
        assert_eq!(fs::read(&path).unwrap(), b"original");
    }

    #[test]
    fn executable_capabilities_are_not_reinstalled_on_new_bytes() {
        let Some((_temp, path)) = fixture() else {
            return;
        };
        let file = File::open(&path).unwrap();
        let mut snapshot = Snapshot::read(&file).unwrap();
        // No privileged fixture setup needed: if install attempts this write,
        // the deliberately invalid capability bytes make the test fail.
        snapshot
            .values
            .insert(CAPABILITIES.to_vec(), b"must not copy".to_vec());
        snapshot.install(&file).unwrap();
        assert!(
            !Snapshot::read(&file)
                .unwrap()
                .values
                .contains_key(CAPABILITIES)
        );
    }

    #[test]
    fn integrity_signed_metadata_refuses_replacement_instead_of_forging_a_signature() {
        let Some((_temp, path)) = fixture() else {
            return;
        };
        let file = File::open(&path).unwrap();
        let initial = Snapshot::read(&file).unwrap();
        for name in [b"security.ima".as_slice(), b"security.evm".as_slice()] {
            let mut signed = Snapshot {
                values: initial.values.clone(),
            };
            signed.values.insert(name.to_vec(), b"signature".to_vec());
            assert_eq!(
                signed.install(&file).unwrap_err().kind(),
                io::ErrorKind::PermissionDenied
            );
            assert_eq!(Snapshot::read(&file).unwrap(), initial);
        }
    }

    #[test]
    fn stage_attribute_tampering_after_sync_prevents_publication() {
        let Some((temp, path)) = fixture() else {
            return;
        };
        let expected = Snapshot::read(&File::open(&path).unwrap()).unwrap();
        let error = write_with_hook(&path, b"replacement", |phase| {
            if phase == Phase::Publish {
                let stage = fs::read_dir(temp.path())?
                    .map(|entry| entry.unwrap().path())
                    .find(|path| {
                        path.file_name()
                            .unwrap()
                            .to_string_lossy()
                            .starts_with(STAGE_PREFIX)
                    })
                    .expect("owned stage");
                fsetxattr(
                    &File::open(stage)?,
                    c"user.unexpected",
                    b"tampered after sync",
                    XattrFlags::empty(),
                )?;
            }
            Ok(())
        })
        .unwrap_err();
        assert_eq!(error.details.unwrap()["commit_state"], "not_committed");
        assert_eq!(fs::read(&path).unwrap(), b"original");
        assert_eq!(
            Snapshot::read(&File::open(&path).unwrap()).unwrap(),
            expected
        );
    }
}
