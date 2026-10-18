use super::*;

#[test]
fn github_read_plans_raw_io_not_a_placeholder() {
    let route = Route::new("https://github.com/team/repo/blob/feat%2Fone/src/a%20b.rs?plain=1#L20",
        SsrfPolicy::BlockPrivateTargets, UrlReadMode::Reader).unwrap();
    assert_eq!(route.current.as_str(), "https://raw.githubusercontent.com/team/repo/feat%2Fone/src/a%20b.rs");
    let out = route.finish("text/plain", b"fn actual_file() {}\n", false).unwrap();
    assert_eq!(out.content, "fn actual_file() {}\n");
    assert_eq!(out.extractor, "github-blob-raw");
    assert_eq!(out.final_url, route.current.as_str());
    assert!(!out.download_truncated);
}

#[test]
fn raw_mode_and_offline_html_conversion_do_not_redirect() {
    let original = "https://github.com/team/repo/blob/main/index.html?plain=1";
    let route = Route::new(original, SsrfPolicy::BlockPrivateTargets, UrlReadMode::Raw).unwrap();
    assert_eq!(route.current.as_str(), original);
    assert_eq!(route.finish("text/html", b"<h1>original</h1>", false).unwrap().content, "<h1>original</h1>");
    let converted = crate::url_read::convert_bytes(original, "text/html", b"<main>supplied source</main>", false).unwrap();
    assert!(converted.content.contains("supplied source"));
    assert!(!converted.content.contains("fetch the raw"));
}

#[test]
fn repository_html_source_is_never_reader_stripped() {
    let route = Route::new("https://github.com/team/repo/blob/main/index.html",
        SsrfPolicy::BlockPrivateTargets, UrlReadMode::Reader).unwrap();
    let html = b"<script>source_only()</script><nav>important markup</nav>";
    let out = route.finish("text/html", html, true).unwrap();
    assert_eq!(out.content.as_bytes(), html);
    assert_eq!(out.kind, UrlContentKind::PlainText);
    assert!(out.download_truncated);
}

#[test]
fn only_exact_github_routes_are_synthesized() {
    for url in ["https://github.com.attacker.invalid/o/r/blob/main/x", "https://github.com:8443/o/r/blob/main/x",
        "https://github.com/o/r/tree/main/x", "https://github.com/o/r/blob/main", "https://github.com/o/r/blob//x",
        "https://github.com/o/r/blob/main/", "https://user:secret@github.com/o/r/blob/main/x",
        "file://github.com/o/r/blob/main/x", "https://github.com/o%2Fx/r/blob/main/x"] {
        assert!(github_blob_to_raw(url).is_none(), "mapped {url}");
    }
    assert!(github_blob_to_raw("https://GITHUB.COM:443/o/r/blob/main/a.txt").is_some());
}

#[test]
fn github_office_documents_use_native_readers_and_reject_partial_downloads() {
    for extension in ["docx", "pptx"] {
        let url = format!("https://github.com/team/repo/blob/main/report.{extension}");
        let route = Route::new(
            &url,
            SsrfPolicy::BlockPrivateTargets,
            UrlReadMode::Reader,
        )
        .unwrap();
        let error = route
            .finish("application/octet-stream", b"partial ZIP", true)
            .expect_err("a partial Office archive must not become raw source text");
        assert!(error.to_string().contains("DOCUMENT_INCOMPLETE"), "{error}");

        let raw = Route::new(&url, SsrfPolicy::BlockPrivateTargets, UrlReadMode::Raw)
            .unwrap()
            .finish("application/octet-stream", b"original bytes", true)
            .unwrap();
        assert_eq!(raw.content, "original bytes");
        assert_eq!(raw.extractor, "raw");
        assert!(raw.download_truncated);
    }
}

#[test]
fn relative_redirects_use_url_reference_semantics() {
    for (base, location, expected) in [
        ("https://a.test", "file", "https://a.test/file"),
        ("https://a.test/a/b?old=1", "?new=2", "https://a.test/a/b?new=2"),
        ("https://a.test/a/b", "../c", "https://a.test/c"),
        ("https://a.test/a/b", "//b.test/c", "https://b.test/c"),
        ("https://a.test/a/b", "#fragment", "https://a.test/a/b"),
    ] { assert_eq!(resolve_redirect(base, location).unwrap(), expected); }
}

#[test]
fn redirect_into_github_uses_the_same_source_resolver() {
    let mut route = Route::new("https://example.com/file", SsrfPolicy::BlockPrivateTargets, UrlReadMode::Reader).unwrap();
    route.follow("https://github.com/o/r/blob/main/data.json").unwrap();
    assert_eq!(route.current.as_str(), "https://raw.githubusercontent.com/o/r/main/data.json");
    assert_eq!(route.redirects, 1);
}

#[test]
fn blocked_redirects_never_become_requests_and_errors_do_not_echo_secrets() {
    for location in ["http://127.1/x?key=do-not-echo", "http://[::ffff:127.0.0.1]/", "http://localhost./",
        "file:///tmp/x", "https://user:do-not-echo@example.com/x", "https://example.com/\nx"] {
        let mut route = Route::new("https://example.com/start", SsrfPolicy::BlockPrivateTargets, UrlReadMode::Reader).unwrap();
        let error = route.follow(location).unwrap_err().to_string();
        assert!(!error.contains("do-not-echo"));
    }
}

#[test]
fn normalized_loops_and_redirect_budget_are_bounded() {
    let mut route = Route::new("https://example.com/start", SsrfPolicy::BlockPrivateTargets, UrlReadMode::Reader).unwrap();
    assert!(route.follow("#again").unwrap_err().to_string().contains("LOOP"));
    let mut route = Route::new("https://example.com/start", SsrfPolicy::BlockPrivateTargets, UrlReadMode::Reader).unwrap();
    for index in 0..MAX_REDIRECTS { route.follow(&format!("/step-{index}")).unwrap(); }
    assert!(route.follow("/one-too-many").unwrap_err().to_string().contains("LIMIT"));
}

#[test]
fn literal_host_checks_cover_alternative_ipv4_and_mapped_ipv6() {
    for url in ["http://127.1/", "http://2130706433/", "http://0x7f000001/", "http://[::ffff:192.168.1.1]/",
        "http://LOCALHOST./", "http://100.64.0.1/", "http://100.127.255.255/", "http://224.0.0.1/",
        "http://[ff02::1]/", "http://[2001:db8::1]/"] { assert!(host_is_private_target(url), "{url}"); }
    for url in ["https://example.com/", "http://100.63.255.255/", "http://100.128.0.1/", "http://100.192.0.1/"] {
        assert!(!host_is_private_target(url), "{url}");
    }
}

#[test]
fn url_limits_are_checked_before_and_after_encoding() {
    let raw = format!("https://example.com/{}", "a".repeat(MAX_URL_BYTES));
    assert!(Route::new(&raw, SsrfPolicy::AllowPrivateTargets, UrlReadMode::Raw).is_err());
    let unicode = format!("https://example.com/{}", "界".repeat(2000));
    assert!(unicode.len() < MAX_URL_BYTES);
    assert!(Route::new(&unicode, SsrfPolicy::AllowPrivateTargets, UrlReadMode::Raw).is_err());
}

// Real HTTP requests exercise the production transport; the server is a
// deterministic protocol peer, not an algorithm-only converter mock.
fn peer(replies: Vec<String>) -> (String, std::thread::JoinHandle<Vec<String>>) {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let thread = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for reply in replies {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "missing expected HTTP request");
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                assert!(request.len() < 32768);
                let mut byte = [0];
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            requests.push(String::from_utf8(request).unwrap());
            socket.write_all(reply.as_bytes()).unwrap();
        }
        requests
    });
    (url, thread)
}

fn run<F: std::future::Future>(future: F) -> F::Output {
    asupersync::runtime::RuntimeBuilder::current_thread().blocking_threads(1, 8)
        .build().unwrap().block_on(future)
}

fn response(status: &str, headers: &str, body: &str) -> String {
    format!("HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n{body}", body.len())
}

#[test]
fn http_query_redirect_retrieves_literal_final_document() {
    let (base, server) = peer(vec![
        response("302 Found", "Location: ?page=2\r\n", ""),
        response("200 OK", "Content-Type: text/plain\r\n", "actual document\n界\n"),
    ]);
    let out = run(fetch(&format!("{base}/document?page=1"), SsrfPolicy::AllowPrivateTargets, UrlReadMode::Reader)).unwrap();
    assert_eq!(out.content, "actual document\n界\n");
    assert_eq!(out.final_url, format!("{base}/document?page=2"));
    let requests = server.join().unwrap();
    assert!(requests[0].starts_with("GET /document?page=1 "));
    assert!(requests[1].starts_with("GET /document?page=2 "));
}

#[test]
fn http_network_path_redirect_is_followed() {
    let (destination, body_server) = peer(vec![response("200 OK", "Content-Type: text/plain\r\n", "destination")]);
    let target = destination.strip_prefix("http:").unwrap();
    let (base, redirect_server) = peer(vec![response("307 Temporary Redirect", &format!("Location: {target}/file\r\n"), "")]);
    let out = run(fetch(&format!("{base}/start"), SsrfPolicy::AllowPrivateTargets, UrlReadMode::Reader)).unwrap();
    assert_eq!(out.content, "destination");
    assert_eq!(out.final_url, format!("{destination}/file"));
    assert_eq!(redirect_server.join().unwrap().len(), 1);
    assert!(body_server.join().unwrap()[0].starts_with("GET /file "));
}

#[test]
fn http_missing_location_and_terminal_error_do_not_report_success() {
    for reply in [response("302 Found", "", ""), response("404 Not Found", "Content-Type: text/html\r\n", "login") ] {
        let (base, server) = peer(vec![reply]);
        assert!(run(fetch(&base, SsrfPolicy::AllowPrivateTargets, UrlReadMode::Reader)).is_err());
        assert_eq!(server.join().unwrap().len(), 1);
    }
}

#[test]
fn http_raw_mode_keeps_the_wire_page() {
    let html = "<nav>kept</nav><script>source()</script>";
    let (base, server) = peer(vec![response("200 OK", "Content-Type: text/html\r\n", html)]);
    let out = run(fetch(&base, SsrfPolicy::AllowPrivateTargets, UrlReadMode::Raw)).unwrap();
    assert_eq!(out.content, html);
    assert_eq!(out.extractor, "raw");
    assert_eq!(server.join().unwrap().len(), 1);
}
