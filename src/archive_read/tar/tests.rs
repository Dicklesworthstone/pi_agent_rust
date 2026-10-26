use super::*;
use std::fs::File;
use std::io::{Cursor, Write as _};

fn seal(header: &mut [u8; BLOCK]) {
    header[148..156].fill(b' ');
    let sum: u64 = header.iter().map(|byte| u64::from(*byte)).sum();
    header[148..154].copy_from_slice(format!("{sum:06o}").as_bytes());
    header[154] = 0;
}

fn header(name: &str, size: u64, kind: u8) -> [u8; BLOCK] {
    assert!(name.len() <= 100);
    let mut header = [0; BLOCK];
    header[..name.len()].copy_from_slice(name.as_bytes());
    header[100..108].copy_from_slice(b"0000644\0");
    header[124..135].copy_from_slice(format!("{size:011o}").as_bytes());
    header[156] = kind;
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    seal(&mut header);
    header
}

fn append(bytes: &mut Vec<u8>, header: &[u8; BLOCK], content: &[u8]) {
    bytes.extend_from_slice(header);
    bytes.extend_from_slice(content);
    bytes.resize(bytes.len().next_multiple_of(BLOCK), 0);
}

fn archive(items: &[(&str, &[u8], u8)]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for (name, content, kind) in items {
        append(&mut bytes, &header(name, content.len() as u64, *kind), content);
    }
    bytes.resize(bytes.len() + BLOCK * 2, 0);
    bytes
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

fn error(bytes: &[u8], member: &str) -> String {
    read(Cursor::new(bytes), member, false).unwrap_err().to_string()
}

#[test]
fn single_colon_tar_paths_include_source_package_aliases_not_arbitrary_gzip() {
    for path in ["source.tar", "source.TAR.GZ", "source.tgz", "lib.crate"] {
        let parsed = crate::archive_read::parse_path(&format!("{path}:src/main.rs"))
            .unwrap().unwrap();
        assert_eq!(parsed.archive_path, path);
        assert_eq!(parsed.member, "src/main.rs");
    }
    for ordinary in ["log.gz", "file.tar.xz", "folder.tar/file.txt", "https://host/release.tar"] {
        assert!(crate::archive_read::parse_path(ordinary).unwrap().is_none());
    }
    let windows = crate::archive_read::parse_path(r"C:\work\src.tar.gz:src\main.rs")
        .unwrap().unwrap();
    assert_eq!(windows.archive_path, r"C:\work\src.tar.gz");
    assert_eq!(windows.member, "src/main.rs");
}

#[test]
fn lists_implied_and_explicit_directories_and_reads_utf8_without_extraction() {
    let bytes = archive(&[("./", b"", b'5'), ("src/main.rs", "café\n".as_bytes(), b'0'),
        ("README", b"hello", b'0'), ("empty/", b"", b'5')]);
    for compressed in [false, true] {
        let input = if compressed { gzip(&bytes) } else { bytes.clone() };
        let root = read(Cursor::new(&input), "", compressed).unwrap();
        assert_eq!(root.content, "README (5 bytes)\nempty/\nsrc/\n");
        assert_eq!(root.entry_count, 3);
        assert!(root.is_directory);
        let directory = read(Cursor::new(&input), "src", compressed).unwrap();
        assert_eq!(directory.content, "main.rs (6 bytes)\n");
        assert!(directory.is_directory);
        assert_eq!(read(Cursor::new(&input), "src/main.rs", compressed).unwrap().content, "café\n");
        assert_eq!(read(Cursor::new(&input), "empty", compressed).unwrap().content,
            "[Empty archive directory]\n");
    }
}

#[test]
fn posix_prefix_gnu_and_v7_names_have_their_own_layouts() {
    let mut posix = header("file", 2, b'0');
    posix[345..351].copy_from_slice(b"nested");
    seal(&mut posix);
    let mut bytes = Vec::new();
    append(&mut bytes, &posix, b"ok");
    bytes.resize(bytes.len() + BLOCK * 2, 0);
    assert_eq!(read(Cursor::new(bytes), "nested/file", false).unwrap().content, "ok");
    for gnu in [true, false] {
        let mut legacy = header("file", 2, 0);
        legacy[257..].fill(0);
        if gnu { legacy[257..265].copy_from_slice(b"ustar  \0"); }
        seal(&mut legacy);
        assert!(recognizes(&legacy));
        let mut bytes = Vec::new();
        append(&mut bytes, &legacy, b"ok");
        bytes.resize(bytes.len() + BLOCK * 2, 0);
        assert_eq!(read(Cursor::new(bytes), "file", false).unwrap().content, "ok");
    }
}

#[test]
fn late_bad_headers_duplicates_and_conflicts_invalidate_an_early_match() {
    let duplicate = archive(&[("found", b"first", b'0'), ("./found", b"second", b'0')]);
    assert!(error(&duplicate, "found").contains("duplicate"));
    let conflict = archive(&[("dir/child", b"first", b'0'), ("dir", b"file", b'0')]);
    assert!(error(&conflict, "dir/child").contains("conflict"));
    let mut corrupt = archive(&[("found", b"first", b'0'), ("later", b"second", b'0')]);
    corrupt[BLOCK * 2] ^= 1;
    assert!(error(&corrupt, "found").contains("checksum"));
}

#[test]
fn rejects_links_devices_sparse_and_unsafe_names_even_when_unselected() {
    for kind in [b'1', b'2', b'3', b'4', b'6', b'S', b'7', b'K'] {
        let bytes = archive(&[("found", b"ok", b'0'), ("other", b"", kind)]);
        assert!(error(&bytes, "found").contains("Unsupported TAR member type"));
    }
    for name in ["../outside", "/absolute", r"C:\outside", "a\nb", "a/../../b"] {
        let bytes = archive(&[(name, b"bad", b'0')]);
        assert!(read(Cursor::new(bytes), "", false).is_err(), "{name:?}");
    }
}

#[test]
fn truncated_payload_padding_terminator_and_nonzero_tail_are_never_success() {
    let bytes = archive(&[("found", b"payload", b'0')]);
    for length in [0, 100, BLOCK, BLOCK + 3, BLOCK + 7, BLOCK * 2, BLOCK * 3] {
        assert!(read(Cursor::new(&bytes[..length]), "found", false).is_err(), "{length}");
    }
    let mut trailing = bytes.clone();
    trailing.extend_from_slice(&header("hidden", 0, b'0'));
    assert!(error(&trailing, "found").contains("after TAR end"));
    let mut trailing = bytes;
    trailing.push(0);
    assert!(error(&trailing, "found").contains("Truncated TAR block"));
}

#[test]
fn gzip_footer_and_all_concatenated_members_are_validated() {
    let bytes = archive(&[("found", b"payload", b'0')]);
    let mut combined = gzip(&bytes[..700]);
    combined.extend(gzip(&bytes[700..]));
    assert_eq!(read(Cursor::new(combined), "found", true).unwrap().content, "payload");
    let mut corrupt = gzip(&bytes);
    let crc = corrupt.len() - 8;
    corrupt[crc] ^= 1;
    assert!(read(Cursor::new(corrupt), "found", true).is_err());
    let mut appended = gzip(&bytes);
    appended.extend(gzip(&archive(&[("hidden", b"other", b'0')])));
    assert!(read(Cursor::new(appended), "found", true).is_err());
    let mut garbage = gzip(&bytes);
    garbage.extend_from_slice(b"not another gzip member");
    assert!(read(Cursor::new(garbage), "found", true).is_err());
}

#[test]
fn declared_limits_binary_content_and_numeric_overflow_fail_explicitly() {
    assert!(error(&header("large", MAX_MEMBER_BYTES + 1, b'0'), "large").contains("16 MiB"));
    assert!(error(&header("large", MAX_TOTAL_MEMBER_BYTES + 1, b'0'), "").contains("64 MiB"));
    assert!(error(&archive(&[("binary", b"a\0b", b'0')]), "binary").contains("Binary"));
    assert!(error(&archive(&[("invalid", b"\xff", b'0')]), "invalid").contains("UTF-8"));
    assert!(error(&archive(&[]), "missing").contains("not found"));
    let mut positive = [0; 12];
    positive[0] = 0x80;
    positive[11] = 42;
    assert_eq!(number(&positive).unwrap(), 42);
    positive[0] = 0xff;
    assert!(number(&positive).is_err());
    positive.fill(0xff);
    positive[0] = 0x80;
    assert!(number(&positive).is_err());
    assert!(number(b"00008000").is_err());
    let mut oversized = Vec::new();
    for index in 0..=MAX_ENTRIES {
        oversized.extend(header(&format!("f{index}"), 0, b'0'));
    }
    assert!(error(&oversized, "").contains("4096-header"));
}

#[test]
fn expanded_padding_and_read_operations_are_bounded_without_large_allocations() {
    let mut stream = Stream { inner: io::repeat(0), bytes: MAX_ARCHIVE_BYTES, operations: 5 };
    assert!(stream.read(&mut [0; 1]).unwrap_err().to_string().contains("expanded-stream"));
    let mut stream = Stream { inner: io::repeat(0), bytes: 0, operations: 1 };
    assert_eq!(stream.read(&mut [0; 1]).unwrap(), 1);
    assert!(stream.read(&mut [0; 1]).unwrap_err().to_string().contains("operation"));
}

#[test]
fn admitted_descriptor_selects_tar_and_gzip_and_never_creates_members() {
    let temp = tempfile::tempdir().unwrap();
    let bytes = archive(&[("nested/member", b"original", b'0')]);
    for (name, data) in [("source.tar", bytes.clone()), ("source.tgz", gzip(&bytes))] {
        let path = temp.path().join(name);
        std::fs::write(&path, &data).unwrap();
        assert_eq!(crate::archive_read::read_open_file(File::open(&path).unwrap(), "nested/member")
            .unwrap().content, "original");
        assert_eq!(std::fs::read(&path).unwrap(), data);
    }
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 2);
}

#[cfg(unix)]
#[test]
fn pathname_replacement_cannot_redirect_the_open_tar() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("source.tgz");
    std::fs::write(&path, gzip(&archive(&[("member", b"original", b'0')]))).unwrap();
    let file = File::open(&path).unwrap();
    std::fs::rename(&path, temp.path().join("original.tgz")).unwrap();
    std::fs::write(&path, gzip(&archive(&[("member", b"replacement", b'0')]))).unwrap();
    assert_eq!(crate::archive_read::read_open_file(file, "member").unwrap().content, "original");
}

#[test]
fn public_read_tool_routes_tar_members_through_pagination_and_hashline_admission() {
    use crate::tools::{ReadTool, Tool};
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("source.tgz"),
        gzip(&archive(&[("src/file.txt", b"first\nsecond\nthird\n", b'0')]))).unwrap();
    let tool = ReadTool::new(temp.path());
    asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap().block_on(async {
        let output = tool.execute("tar-page", serde_json::json!({
            "path":"source.tgz:src/file.txt", "offset":2, "limit":1
        }), None).await.unwrap();
        assert!(!output.is_error);
        let text = serde_json::to_string(&output.content).unwrap();
        assert!(text.contains("second"));
        assert!(!text.contains("first"));
        assert_eq!(output.details.as_ref().unwrap()["member"], "src/file.txt");
        let rejected = tool.execute("tar-hashline", serde_json::json!({
            "path":"source.tgz:src/file.txt", "hashline":true
        }), None).await.unwrap_err();
        assert!(rejected.to_string().contains("ARCHIVE_HASHLINE"));
    });
}

fn pax_record(key: &str, value: &[u8]) -> Vec<u8> {
    let mut length = key.len() + value.len() + 4;
    loop {
        let actual = length.to_string().len() + 1 + key.len() + 1 + value.len() + 1;
        if actual == length { break; }
        length = actual;
    }
    let mut bytes = format!("{length} {key}=").into_bytes();
    bytes.extend_from_slice(value);
    bytes.push(b'\n');
    bytes
}

fn extension(bytes: &mut Vec<u8>, kind: u8, fields: &[(&str, &[u8])]) {
    let data: Vec<u8> = fields.iter().flat_map(|(key, value)| pax_record(key, value)).collect();
    append(bytes, &header("PaxHeader", data.len() as u64, kind), &data);
}

#[test]
fn pax_utf8_path_and_size_override_only_the_next_regular_member() {
    let name = format!("release/{}notes.txt", "évidence/".repeat(20));
    let mut bytes = Vec::new();
    extension(&mut bytes, b'x', &[("path", name.as_bytes()), ("size", b"2"), ("mtime", b"0.25")]);
    append(&mut bytes, &header("placeholder", 0, b'0'), b"ok");
    append(&mut bytes, &header("ordinary", 3, b'0'), b"yes");
    bytes.resize(bytes.len() + BLOCK * 2, 0);
    let result = read(Cursor::new(&bytes), &name, false).unwrap();
    assert_eq!(result.content, "ok");
    assert_eq!(result.entry_count, 2, "extension headers are not visible members");
    assert_eq!(read(Cursor::new(&bytes), "ordinary", false).unwrap().content, "yes");
    assert!(error(&bytes, "placeholder").contains("not found"));
}

#[test]
fn pax_global_values_local_deletion_and_global_deletion_follow_header_scope() {
    let mut bytes = Vec::new();
    extension(&mut bytes, b'g', &[("size", b"2"), ("comment", b"git archive commit")]);
    append(&mut bytes, &header("inherited", 0, b'0'), b"ab");
    extension(&mut bytes, b'x', &[("size", b"")]);
    append(&mut bytes, &header("local-reset", 3, b'0'), b"xyz");
    append(&mut bytes, &header("inherited-again", 0, b'0'), b"cd");
    extension(&mut bytes, b'g', &[("size", b"")]);
    append(&mut bytes, &header("global-reset", 1, b'0'), b"q");
    bytes.resize(bytes.len() + BLOCK * 2, 0);
    for (name, content) in [("inherited", "ab"), ("local-reset", "xyz"),
        ("inherited-again", "cd"), ("global-reset", "q")] {
        assert_eq!(read(Cursor::new(&bytes), name, false).unwrap().content, content);
    }
}

#[test]
fn gnu_long_names_do_not_leak_into_the_next_entry() {
    let name = format!("{}file", "nested/".repeat(30));
    let mut long = name.as_bytes().to_vec();
    long.push(0);
    let mut bytes = Vec::new();
    append(&mut bytes, &header("././@LongLink", long.len() as u64, b'L'), &long);
    append(&mut bytes, &header("truncated", 2, b'0'), b"ok");
    append(&mut bytes, &header("next", 3, b'0'), b"yes");
    bytes.resize(bytes.len() + BLOCK * 2, 0);
    assert_eq!(read(Cursor::new(&bytes), &name, false).unwrap().content, "ok");
    assert_eq!(read(Cursor::new(&bytes), "next", false).unwrap().content, "yes");
}

#[test]
fn malformed_duplicate_or_unsupported_pax_records_are_not_ignored() {
    for data in [b"0 path=x\n".to_vec(), b"999 path=x\n".to_vec(),
        b"11 path=x".to_vec(), b"999999999999999999999999 path=x\n".to_vec(),
        [pax_record("path", b"one"), pax_record("path", b"two")].concat(),
        pax_record("size", b"-1"), pax_record("size", b"+1"),
        pax_record("path", b"\xff"), pax_record("GNU.sparse.map", b"0,100"),
        pax_record("SCHILY.filetype", b"sparse"), pax_record("linkpath", b"target"),
        pax_record("hdrcharset", b"BINARY")] {
        assert!(pax(&data).is_err(), "{data:?}");
    }
    let too_many: Vec<u8> = (0..=MAX_PAX_RECORDS)
        .flat_map(|index| pax_record(&format!("SCHILY.xattr.key{index}"), b"v")).collect();
    assert!(pax(&too_many).is_err());
}

#[test]
fn orphan_ambiguous_and_oversized_extensions_fail_before_publication() {
    for kind in [b'x', b'L'] {
        let data = if kind == b'x' { pax_record("path", b"file") } else { b"file\0".to_vec() };
        let mut bytes = Vec::new();
        append(&mut bytes, &header("metadata", data.len() as u64, kind), &data);
        bytes.resize(bytes.len() + BLOCK * 2, 0);
        assert!(error(&bytes, "").contains("no following member"));
    }
    let mut bytes = Vec::new();
    extension(&mut bytes, b'x', &[("path", b"one")]);
    extension(&mut bytes, b'x', &[("path", b"two")]);
    assert!(error(&bytes, "").contains("consecutive"));
    assert!(error(&header("metadata", MAX_EXTENSION_BYTES + 1, b'x'), "").contains("64 KiB"));
    assert!(error(&header("metadata", MAX_MEMBER_NAME_BYTES as u64 + 2, b'L'), "").contains("path limit"));
    let mut bytes = Vec::new();
    append(&mut bytes, &header("metadata", 4, b'L'), b"file");
    assert!(error(&bytes, "").contains("NUL terminator"));
}

#[test]
fn extended_names_and_sizes_cannot_bypass_archive_admission() {
    for path in [b"../outside".as_slice(), b"/absolute", b"C:\\outside", b"nul\0name"] {
        assert!(pax(&pax_record("path", path)).is_err());
    }
    let mut bytes = Vec::new();
    append(&mut bytes, &header("safe", 2, b'0'), b"ok");
    extension(&mut bytes, b'x', &[("path", b"./safe")]);
    append(&mut bytes, &header("other", 2, b'0'), b"no");
    bytes.resize(bytes.len() + BLOCK * 2, 0);
    assert!(error(&bytes, "safe").contains("duplicate"));
    let mut oversized = Vec::new();
    extension(&mut oversized, b'x', &[("size", (MAX_TOTAL_MEMBER_BYTES + 1).to_string().as_bytes())]);
    oversized.extend(header("large", 0, b'0'));
    assert!(error(&oversized, "").contains("64 MiB"));
}

#[test]
fn standard_python_tarfile_producers_are_native_reader_fixtures() {
    use base64::Engine as _;
    // Produced with Python tarfile in PAX/GNU/USTAR modes and gzip(mtime=0).
    // Independently read by Python and GNU tar; these embedded bytes do not
    // depend on this module's test header writer or a subprocess at test time.
    for (encoded, path) in [
        ("H4sIAAAAAAAC/+3TTQqCQBjG8Vl7Ck+g46i5kgoS2gTRDQadSEgLtfBKnaOL9YVBC1dhFP1/m3l4NwPvy+O4jjtZ6nZudGYqMQj50PdKFUXPfJ97Uilf2K34gEPd6Or6vfhPXhTZe91s4spsja6Nez4d88yU6XelVTKdLRKnyCwvsIsmL0wsHRVaAu/pzj7uNj1AuJV6FAS9/Zde+Np/X6pAClvS/8Gt86purNqkuzKjTQAAAAAAAAAAAAAAAL/nAkBepQkAKAAA",
            format!("release/{}README.md", "évidence/".repeat(15))),
        ("H4sIAAAAAAAC/+3TMQ6CMBSA4c6cohcQSqCwaiIbLt6AQDVEaAnF+6shMXFgxEj8v+W1XV//MAqjfenstWztTaxDzZamSlT2Ps/vea5TIUvxBXc/VaOU4k+NpjOVN1H3/AK72vWDs8ZO27iei8PxVIR9I/DL+39FnaXpYv8q1p/9J3GilZCK/ld3aUc/Bd7UzjYBOQAAAAAAAAAAAAAAAGzOA22a8c4AKAAA",
            format!("release/{}README.md", "long-component/".repeat(12))),
        ("H4sIAAAAAAAC/+3QsQqDMBDG8Zt9ij5BjZq6F+rYpW8QaqSFWiVJ37/RRXBWUfj/hvuOG+7gehNeab9GeVTX2706t7WoqNR6zGieSmWXqR/mRZHrTE5KNvDzwbh4fum9zn6s8XZ6iOxS83Y+JN4+u2+dCAAAAAAAAAAAAAAAAADgKP6au+CIACgAAA==",
            format!("release/{}README.md", "path/".repeat(20))),
    ] {
        let bytes = base64::engine::general_purpose::STANDARD.decode(encoded).unwrap();
        assert_eq!(read(Cursor::new(bytes), &path, true).unwrap().content, "first\nsecond\n");
    }
}
