use super::*;

#[test]
fn actual_native_files_roundtrip_without_reinterpreting_delimiters() {
    let temp = tempfile::tempdir().unwrap();
    for name in ["ordinary.rs", "space name.rs", "hash#name.rs", "percent%20literal.rs", "plus+name.rs", "日本語😀.rs"] {
        let path = temp.path().join(name);
        std::fs::write(&path, name).unwrap();
        let path = path.canonicalize().unwrap();
        let uri = try_path_to_uri(&path).unwrap();
        assert_eq!(path_to_uri(&path), uri);
        let recovered = uri_to_path(&uri).unwrap();
        assert!(recovered.is_absolute());
        assert_eq!(std::fs::read(&recovered).unwrap(), name.as_bytes());
        assert_eq!(try_path_to_uri(&recovered).unwrap(), uri);
        assert!(!uri.contains(' '));
        assert!(url::Url::parse(&uri).unwrap().query().is_none());
        assert!(url::Url::parse(&uri).unwrap().fragment().is_none());
    }
}

#[test]
fn malformed_uris_never_name_a_relative_or_different_file() {
    for uri in [
        "", "relative.rs", "https://example.com/a", "file:relative", "file:/tmp/a",
        "file://", "file://localhost", "file://C:/file", "file://user:password@host/file",
        "file://localhost:99/tmp/file", "file://local%68ost/tmp/file",
        "file:///tmp/%", "file:///tmp/%0", "file:///tmp/%GG", "file:///tmp/%00",
        "file:///tmp/a%2fb", "file:///tmp/a%2Fb", "file:////host/share/file",
        "file:///tmp/../outside", "file:///tmp/%2e%2e/outside", "file:///tmp/.%2E/outside",
        "file:///tmp/./file", "file:///tmp/%2e/file", "file:///tmp/file?query",
        "file:///tmp/file#fragment", "file:///tmp/raw space", "file:///tmp/raw\nnewline",
        "file:///tmp/raw\0nul", "file:///tmp/raw\\separator", " file:///tmp/file",
        "file://./share/name", "file://../share/name",
    ] {
        assert!(uri_to_path(uri).is_none(), "accepted {uri:?}");
        assert!(normalize_uri(uri).is_none(), "normalized {uri:?}");
    }
}

#[test]
fn unrepresentable_native_paths_return_errors_before_dispatch() {
    for path in [Path::new("relative.rs"), Path::new(""), Path::new("/tmp/../elsewhere")] {
        assert!(try_path_to_uri(path).is_err());
        assert!(path_to_uri(path).is_empty());
    }
}

#[test]
fn pure_windows_decoding_supports_drive_roots_and_unc_paths() {
    for (authority, input, expected) in [
        ("", "/C:/Users/me/code.rs", "C:\\Users\\me\\code.rs"),
        ("localhost", "/c:/Program Files/日本語.rs", "C:\\Program Files\\日本語.rs"),
        ("LOCALHOST", "/d:/", "D:\\"),
        ("build-server", "/share/my project/file.rs", "\\\\build-server\\share\\my project\\file.rs"),
        ("BUILD-SERVER", "/share", "\\\\build-server\\share"),
    ] {
        assert_eq!(windows_path(authority, input.as_bytes()).as_deref(), Some(expected));
    }
    let path = decode_path("/c%3a/Users/me/space%20name.rs").unwrap();
    assert_eq!(windows_path("", &path).as_deref(), Some("C:\\Users\\me\\space name.rs"));
}

#[test]
fn pure_windows_decoding_rejects_lossy_names_devices_and_path_aliases() {
    for input in [
        b"/C:/bad\xff".as_slice(), b"/C:/bad\0", b"/C:/bad\x01",
        b"/not/a/drive", b"/C:relative", b"/C:", b"/1:/file", b"/C|/file",
        b"/C:/file:stream", b"/C:/trailing.", b"/C:/trailing ", b"/C:/NUL.txt",
        b"/C:/nul .txt", b"/C:/COM1", b"/C:/lpt9.log", b"/C:/CONIN$",
        b"/C:/bad\\component", b"/C:/bad?name", b"/C:/bad*name", b"/C:/bad|name",
    ] {
        assert!(windows_path("", input).is_none(), "accepted {input:?}");
    }
    assert!(windows_path("server", b"/").is_none());
    for device in ["COM¹", "LPT².log", "COM³.txt"] {
        assert!(windows_path("", format!("/C:/{device}").as_bytes()).is_none());
    }
    for regular in ["COM10", "NUL-file", "company.rs"] {
        assert!(windows_path("", format!("/C:/{regular}").as_bytes()).is_some());
    }
}

#[cfg(unix)]
#[test]
fn unix_non_utf8_names_do_not_alias_replacement_characters() {
    use std::os::unix::ffi::OsStringExt;

    let temp = tempfile::tempdir().unwrap();
    let name = std::ffi::OsString::from_vec(b"invalid-\xff.rs".to_vec());
    let raw = temp.path().join(name);
    let replacement = temp.path().join("invalid-�.rs");
    std::fs::write(&raw, "raw filename").unwrap();
    std::fs::write(&replacement, "different filename").unwrap();
    let uri = try_path_to_uri(&raw).unwrap();
    assert!(uri.contains("%FF"));
    assert_eq!(uri_to_path(&uri), Some(raw));
    assert_ne!(uri, try_path_to_uri(&replacement).unwrap());
    assert_eq!(std::fs::read(uri_to_path(&uri).unwrap()).unwrap(), b"raw filename");
}

#[cfg(unix)]
#[test]
fn unix_backslashes_queries_and_percent_escapes_remain_literal_names() {
    for (name, encoded) in [("a\\b", "a%5Cb"), ("a?b", "a%3Fb"), ("a%2Fb", "a%252Fb"), ("a+b", "a+b")] {
        let path = PathBuf::from("/tmp").join(name);
        let uri = try_path_to_uri(&path).unwrap();
        assert!(uri.ends_with(encoded), "{uri}");
        assert_eq!(uri_to_path(&uri), Some(path));
    }
}

#[cfg(unix)]
#[test]
fn local_authorities_and_encoded_unreserved_characters_share_an_identity() {
    for alias in ["file://localhost/tmp/a%20b.rs", "FILE://LOCALHOST/tmp/%61%20b.rs", "file:///tmp/a%20b%2ers"] {
        assert_eq!(normalize_uri(alias).as_deref(), Some("file:///tmp/a%20b.rs"));
    }
    assert!(uri_to_path("file://remote-server/share/file.rs").is_none());
    let nul = PathBuf::from("/tmp/embedded\0nul");
    assert!(try_path_to_uri(&nul).is_err());
}

#[cfg(windows)]
#[test]
fn native_windows_drives_verbatim_roots_and_unc_uris_roundtrip() {
    for (native, expected) in [
        (r"C:\Users\me\space name.rs", "file:///C:/Users/me/space%20name.rs"),
        (r"\\?\C:\Users\me\space name.rs", "file:///C:/Users/me/space%20name.rs"),
        (r"\\server\share\space name.rs", "file://server/share/space%20name.rs"),
        (r"\\?\UNC\server\share\space name.rs", "file://server/share/space%20name.rs"),
    ] {
        assert_eq!(try_path_to_uri(Path::new(native)).unwrap(), expected);
        let path = uri_to_path(expected).unwrap();
        assert!(path.is_absolute());
        assert_eq!(try_path_to_uri(&path).unwrap(), expected);
    }
    assert_eq!(normalize_uri("file:///c%3a/Users/me/code.rs").as_deref(), Some("file:///C:/Users/me/code.rs"));
    assert!(uri_to_path("file:///C:/bad%5Ccomponent").is_none());
    assert!(try_path_to_uri(Path::new(r"\\.\pipe\fixture")).is_err());
}

#[cfg(windows)]
#[test]
fn native_windows_unpaired_surrogates_are_never_replaced() {
    use std::os::windows::ffi::OsStringExt;

    let mut name: Vec<_> = "C:\\work\\bad".encode_utf16().collect();
    name.push(0xd800);
    let path = PathBuf::from(std::ffi::OsString::from_wide(&name));
    assert!(try_path_to_uri(&path).is_err());
}
