//! Bounded recognition of managed PE images for the native .NET debugger.
//!
//! A filename ending in .dll is not evidence of managed code. Follow the PE
//! CLR directory to the CLI header and metadata signature using bounded reads.
//! This is a routing hint, not a PE loader, runtime-compatibility check, or an
//! assertion that a library is an application. Never execute a discovery probe.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use super::AdapterSpec;
use crate::error::Result;

const MAX_PE_OFFSET: u64 = 1024 * 1024;
const MAX_OPTIONAL_BYTES: usize = 4096;
const MAX_SECTIONS: usize = 96;
const SECTION_BYTES: usize = 40;
const CLI_HEADER_BYTES: u32 = 72;

pub(super) fn is_managed_image(path: &Path) -> bool {
    // Reject directories/special files before open. These are cooperative
    // filesystem checks, not exclusion of hostile concurrent path replacement.
    if !path.metadata().is_ok_and(|metadata| metadata.is_file()) {
        return false;
    }
    let Ok(mut file) = File::open(path) else { return false; };
    let Ok(metadata) = file.metadata() else { return false; };
    metadata.is_file() && recognize(&mut file, metadata.len()).is_some()
}

pub(in crate::debug) fn require_built_target(adapter: &AdapterSpec, target: Option<&Path>) -> Result<()> {
    if adapter.id == "netcoredbg" && target.is_some_and(is_source_or_project) {
        return Err(super::super::tool_err(
            "DAP_BUILD_REQUIRED",
            "netcoredbg needs a built .NET application DLL or apphost, not source/project input. Build the project first and select its output; Pi does not build or restore it implicitly.",
        ));
    }
    Ok(())
}

fn is_source_or_project(path: &Path) -> bool {
    path.extension().and_then(|value| value.to_str()).is_some_and(|extension| {
        ["cs", "csx", "fs", "fsx", "csproj", "fsproj", "sln", "slnx"]
            .iter().any(|candidate| extension.eq_ignore_ascii_case(candidate))
    })
}

fn read_at<R: Read + Seek>(reader: &mut R, length: u64, offset: u64, out: &mut [u8]) -> Option<()> {
    let count = u64::try_from(out.len()).ok()?;
    if offset.checked_add(count)? > length { return None; }
    reader.seek(SeekFrom::Start(offset)).ok()?;
    reader.read_exact(out).ok()
}

fn u16_at(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?))
}

fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?))
}

// Require one unambiguous section whose on-disk bytes cover the complete
// requested range. Virtual zero-fill is not stored metadata. Reject arithmetic
// overflow and section mappings that extend beyond the actual file.
fn file_offset(sections: &[u8], rva: u32, size: u32, length: u64) -> Option<u64> {
    if rva == 0 || size == 0 { return None; }
    let end = rva.checked_add(size)?;
    let mut found = None;
    for section in sections.chunks_exact(SECTION_BYTES) {
        let base = u32_at(section, 12)?;
        let raw_size = u32_at(section, 16)?;
        let virtual_size = u32_at(section, 8)?.max(raw_size);
        let virtual_end = base.checked_add(virtual_size)?;
        if rva >= virtual_end || end <= base { continue; }
        // Any overlap must completely and uniquely map the requested range.
        let delta = rva.checked_sub(base)?;
        if end > virtual_end || delta.checked_add(size)? > raw_size { return None; }
        let raw = u64::from(u32_at(section, 20)?);
        if raw.checked_add(u64::from(raw_size))? > length { return None; }
        let offset = raw.checked_add(u64::from(delta))?;
        if found.replace(offset).is_some() { return None; }
    }
    found
}

fn recognize<R: Read + Seek>(reader: &mut R, length: u64) -> Option<()> {
    let mut dos = [0_u8; 64];
    read_at(reader, length, 0, &mut dos)?;
    if dos.get(..2)? != b"MZ" { return None; }
    let pe_offset = u64::from(u32_at(&dos, 60)?);
    if !(64..=MAX_PE_OFFSET).contains(&pe_offset) { return None; }
    let mut coff = [0_u8; 24];
    read_at(reader, length, pe_offset, &mut coff)?;
    if coff.get(..4)? != b"PE\0\0" { return None; }
    let count = usize::from(u16_at(&coff, 6)?);
    let optional_size = usize::from(u16_at(&coff, 20)?);
    if !(1..=MAX_SECTIONS).contains(&count) || !(2..=MAX_OPTIONAL_BYTES).contains(&optional_size) {
        return None;
    }
    let optional_offset = pe_offset.checked_add(24)?;
    let mut optional = [0_u8; MAX_OPTIONAL_BYTES];
    let optional = optional.get_mut(..optional_size)?;
    read_at(reader, length, optional_offset, optional)?;
    let (count_offset, directories) = match u16_at(optional, 0)? {
        0x10b => (92, 96),
        0x20b => (108, 112),
        _ => return None,
    };
    if u32_at(optional, count_offset)? < 15 { return None; }
    let cli_rva = u32_at(optional, directories + 14 * 8)?;
    let cli_size = u32_at(optional, directories + 14 * 8 + 4)?;
    if cli_size < CLI_HEADER_BYTES { return None; }
    let sections_offset = optional_offset.checked_add(u64::try_from(optional_size).ok()?)?;
    let mut sections = [0_u8; MAX_SECTIONS * SECTION_BYTES];
    let sections = sections.get_mut(..count.checked_mul(SECTION_BYTES)?)?;
    read_at(reader, length, sections_offset, sections)?;
    let cli_offset = file_offset(sections, cli_rva, cli_size, length)?;
    let mut cli = [0_u8; CLI_HEADER_BYTES as usize];
    read_at(reader, length, cli_offset, &mut cli)?;
    let header_size = u32_at(&cli, 0)?;
    if !(CLI_HEADER_BYTES..=cli_size).contains(&header_size) { return None; }
    let metadata_rva = u32_at(&cli, 8)?;
    let metadata_size = u32_at(&cli, 12)?;
    if metadata_size < 16 { return None; }
    let metadata_offset = file_offset(sections, metadata_rva, metadata_size, length)?;
    let mut signature = [0_u8; 4];
    read_at(reader, length, metadata_offset, &mut signature)?;
    (signature == *b"BSJB").then_some(())
}

#[cfg(test)]
mod tests;
