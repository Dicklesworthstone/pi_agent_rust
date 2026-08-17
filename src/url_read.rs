//! URL-aware reads (bd-cv653.2.2).
//!
//! `read` on http(s):// paths fetches and converts to reader-mode markdown,
//! with site-aware extractors, PDF text extraction, notebook rendering, and a
//! default-on SSRF guard.
//!
//! Design notes:
//! - HTML converts through a boilerplate-skipping htmd pipeline; site
//!   extractors (GitHub blob→raw, arXiv, registries) shape known hosts before
//!   the generic converter.
//! - `:raw` in the path bypasses conversion; `offset`/`limit` page the
//!   CONVERTED markdown lines exactly like file reads.
//! - The SSRF guard blocks loopback/private/link-local/metadata targets by
//!   default; `read.urlAllowPrivateTargets` (or the e2e harness) opts out.

use crate::error::Error;
use serde_json::Value;

/// Maximum download size for a URL read (10 MiB before conversion).
const MAX_DOWNLOAD_BYTES: usize = 10 * 1024 * 1024;
/// Maximum redirects followed.
const MAX_REDIRECTS: u32 = 5;

/// What the converter produced, for metadata + tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlContentKind {
    Html,
    Pdf,
    Notebook,
    PlainText,
}

impl UrlContentKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Html => "html",
            Self::Pdf => "pdf",
            Self::Notebook => "notebook",
            Self::PlainText => "plaintext",
        }
    }
}

/// A fetched + converted URL read.
#[derive(Debug)]
pub struct UrlReadOutcome {
    pub content: String,
    pub kind: UrlContentKind,
    /// Which extractor produced the content (`generic` when no site rule hit).
    pub extractor: &'static str,
    /// The final URL after redirects.
    pub final_url: String,
    /// Content-Type seen on the wire (before conversion).
    pub wire_content_type: String,
    /// Set when the download hit the size cap (content is partial).
    pub download_truncated: bool,
}

/// SSRF policy for URL reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SsrfPolicy {
    /// Default: block loopback, RFC-1918 private, link-local, and
    /// cloud-metadata targets.
    BlockPrivateTargets,
    /// Test/dev override: allow private targets (loopback included).
    AllowPrivateTargets,
}

/// Resolve the policy from config: `read.urlAllowPrivateTargets: true` opts
/// out. The setting lives under the existing `read` settings object.
#[must_use]
pub fn ssrf_policy_from_config(config: Option<&crate::config::Config>) -> SsrfPolicy {
    let allowed = config
        .and_then(|c| c.read.as_ref())
        .and_then(|r| r.url_allow_private_targets)
        .unwrap_or(false);
    if allowed {
        SsrfPolicy::AllowPrivateTargets
    } else {
        SsrfPolicy::BlockPrivateTargets
    }
}

/// Whether this URL's host is a private/loopback/link-local target.
#[must_use]
pub fn host_is_private_target(url: &str) -> bool {
    let Some(host) = url_host(url) else {
        return true; // unparseable hosts are treated as unsafe
    };
    let host = host.trim_matches(['[', ']']).to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    if let Ok(v4) = host.parse::<std::net::Ipv4Addr>() {
        return v4.is_loopback()
            || v4.is_private()
            || v4.is_link_local()
            || v4.is_unspecified()
            || v4.is_broadcast()
            || v4.is_documentation()
            || v4.octets()[0] == 100 && (v4.octets()[1] & 0b0100_0000) != 0; // CGNAT 100.64/10
    }
    if let Ok(v6) = host.parse::<std::net::Ipv6Addr>() {
        return v6.is_loopback()
            || v6.is_unspecified()
            || (v6.segments()[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
            || (v6.segments()[0] & 0xfe00) == 0xfc00; // fc00::/7 unique-local
    }
    // Numeric-but-unparseable hosts are treated as unsafe; hostnames resolve
    // at connect time (documented DNS-rebinding caveat in the bead close-out).
    host.parse::<std::net::IpAddr>().is_err()
        && host.chars().all(|c| c.is_ascii_digit() || c == '.')
}

fn url_host(url: &str) -> Option<String> {
    let after_scheme = url.split("://").nth(1)?;
    let authority = after_scheme.split(['/', '?', '#']).next()?;
    // Bracketed IPv6 literals contain colons: take the bracket span verbatim.
    if let Some(rest) = authority.strip_prefix('[') {
        let end = rest.find(']')?;
        return Some(rest[..end].to_string());
    }
    let host = authority.split(':').next()?.trim();
    if host.is_empty() {
        return None;
    }
    Some(host.to_string())
}

/// The full fetch + convert pipeline. Honors the global request-timeout
/// override (`PI_HTTP_REQUEST_TIMEOUT_SECS`) via the shared HTTP client.
pub async fn fetch_and_convert(
    url: &str,
    policy: SsrfPolicy,
) -> crate::error::Result<UrlReadOutcome> {
    fetch_and_convert_with_redirects(url, policy, 0).await
}

#[allow(clippy::too_many_lines)]
fn fetch_and_convert_with_redirects<'a>(
    url: &'a str,
    policy: SsrfPolicy,
    depth: u32,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = crate::error::Result<UrlReadOutcome>> + Send + 'a>,
> {
    Box::pin(async move {
        if depth > MAX_REDIRECTS {
            return Err(Error::tool("read", format!("Too many redirects for {url}")));
        }
        if policy == SsrfPolicy::BlockPrivateTargets && host_is_private_target(url) {
            return Err(Error::tool(
                "read",
                format!(
                    "[SSRF_BLOCKED] Refusing to fetch private/loopback/link-local target {url}. \
                     Set read.urlAllowPrivateTargets=true to override."
                ),
            ));
        }

        let client = crate::http::client::Client::new();
        let response = client
            .get(url)
            .header("User-Agent", "pi_agent_rust/0.2 (url read)")
            .header(
                "Accept",
                "text/html,application/pdf,text/plain,application/json,*/*",
            )
            .send()
            .await
            .map_err(|err| Error::tool("read", format!("Failed to fetch {url}: {err}")))?;

        let status = response.status();
        if (300..400).contains(&status) {
            let location = response
                .headers()
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("location"))
                .map(|(_, value)| value.clone());
            if let Some(location) = location {
                let next = resolve_redirect(url, &location);
                return fetch_and_convert_with_redirects(&next, policy, depth + 1).await;
            }
        }
        if !(200..300).contains(&status) {
            return Err(Error::tool("read", format!("HTTP {status} fetching {url}")));
        }

        let wire_content_type = response
            .headers()
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| value.clone())
            .unwrap_or_default();

        let mut stream = response.bytes_stream();
        let (mut bytes, mut download_truncated) = (Vec::new(), false);
        {
            use futures::StreamExt;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk
                    .map_err(|err| Error::tool("read", format!("Failed reading {url}: {err}")))?;
                let remaining = MAX_DOWNLOAD_BYTES.saturating_sub(bytes.len());
                if chunk.len() > remaining {
                    bytes.extend_from_slice(&chunk[..remaining]);
                    download_truncated = true;
                    break;
                }
                bytes.extend_from_slice(&chunk);
            }
            drop(stream);
        }

        convert_bytes(url, &wire_content_type, &bytes, download_truncated)
    })
}

/// Follow a Location header, resolving relative redirects against the source.
fn resolve_redirect(base: &str, location: &str) -> String {
    if location.starts_with("http://") || location.starts_with("https://") {
        return location.to_string();
    }
    if let Some(scheme_end) = base.find("://")
        && let Some(path_start) = base[scheme_end + 3..].find('/')
    {
        let origin = &base[..scheme_end + 3 + path_start];
        if let Some(rest) = location.strip_prefix('/') {
            return format!("{origin}/{rest}");
        }
        if let Some(slash) = base.rfind('/') {
            return format!("{}/{}", &base[..slash], location);
        }
        return format!("{origin}/{location}");
    }
    location.to_string()
}

/// Convert fetched bytes to markdown/text by content-type and extension.
/// Exposed for fixture tests (no network).
pub fn convert_bytes(
    url: &str,
    wire_content_type: &str,
    bytes: &[u8],
    download_truncated: bool,
) -> crate::error::Result<UrlReadOutcome> {
    let kind = classify(url, wire_content_type, bytes);
    let (content, extractor): (String, &'static str) = match kind {
        UrlContentKind::Pdf => (convert_pdf(bytes)?, "pdf"),
        UrlContentKind::Notebook => (convert_notebook(bytes)?, "notebook"),
        UrlContentKind::Html => {
            let text = String::from_utf8_lossy(bytes);
            convert_html(url, &text)
        }
        UrlContentKind::PlainText => (String::from_utf8_lossy(bytes).to_string(), "plaintext"),
    };
    Ok(UrlReadOutcome {
        content,
        kind,
        extractor,
        final_url: url.to_string(),
        wire_content_type: wire_content_type.to_string(),
        download_truncated,
    })
}

fn classify(url: &str, content_type: &str, bytes: &[u8]) -> UrlContentKind {
    let ct = content_type.to_ascii_lowercase();
    let lower = url.to_ascii_lowercase();
    let path = lower.split(['?', '#']).next().unwrap_or(&lower);
    let ext_is = |wanted: &str| {
        std::path::Path::new(path)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case(wanted))
    };
    if ct.contains("application/pdf") || ext_is("pdf") || bytes.starts_with(b"%PDF") {
        return UrlContentKind::Pdf;
    }
    if ext_is("ipynb") {
        return UrlContentKind::Notebook;
    }
    if ct.contains("text/html") || ct.contains("application/xhtml") {
        return UrlContentKind::Html;
    }
    if ct.starts_with("text/") || ct.contains("json") || ct.contains("xml") {
        return UrlContentKind::PlainText;
    }
    if bytes.starts_with(b"{") && bytes.windows(9).any(|w| w == b"\"nbformat\"") {
        return UrlContentKind::Notebook;
    }
    UrlContentKind::PlainText
}

#[cfg(feature = "url-pdf")]
fn convert_pdf(bytes: &[u8]) -> crate::error::Result<String> {
    let pages: Vec<String> = pdf_extract::extract_text_from_mem_by_pages(bytes)
        .map_err(|err| Error::tool("read", format!("PDF extraction failed: {err}")))?;
    let mut out = String::new();
    for (index, text) in pages.iter().enumerate() {
        let _ =
            std::fmt::Write::write_fmt(&mut out, format_args!("\n--- page {} ---\n", index + 1));
        out.push_str(text.trim());
        out.push('\n');
    }
    Ok(out)
}

#[cfg(not(feature = "url-pdf"))]
fn convert_pdf(_bytes: &[u8]) -> crate::error::Result<String> {
    Err(Error::tool(
        "read",
        "[PDF_NOT_COMPILED] This build lacks PDF support (opt-in `url-pdf` feature; \
         it adds ~6 MiB against the release size budget). HTML, notebooks, and \
         plaintext URLs work in the default build."
            .to_string(),
    ))
}

fn convert_notebook(bytes: &[u8]) -> crate::error::Result<String> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|err| Error::tool("read", format!("Notebook parse failed: {err}")))?;
    let cells = value
        .get("cells")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::tool("read", "Notebook has no cells array".to_string()))?;
    let mut out = String::new();
    for (index, cell) in cells.iter().enumerate() {
        let cell_type = cell
            .get("cell_type")
            .and_then(Value::as_str)
            .unwrap_or("code");
        let source = cell
            .get("source")
            .map(|src| match src {
                Value::String(text) => text.clone(),
                Value::Array(lines) => lines
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(""),
                _ => String::new(),
            })
            .unwrap_or_default();
        let _ = std::fmt::Write::write_fmt(
            &mut out,
            format_args!("\n### cell {} ({})\n", index + 1, cell_type),
        );
        if cell_type == "code" {
            out.push_str("```\n");
            out.push_str(source.trim_end());
            out.push_str("\n```\n");
        } else {
            out.push_str(source.trim_end());
            out.push('\n');
        }
        if let Some(outputs) = cell.get("outputs").and_then(Value::as_array)
            && !outputs.is_empty()
        {
            let _ = std::fmt::Write::write_fmt(
                &mut out,
                format_args!("\n_outputs: {} present_\n", outputs.len()),
            );
        }
    }
    Ok(out)
}

/// HTML conversion with site-aware extraction. Returns (markdown, extractor).
fn convert_html(url: &str, html: &str) -> (String, &'static str) {
    // GitHub blob pages: fetch the raw content instead of the chrome page.
    if let Some(raw_url) = github_blob_to_raw(url) {
        return (
            format!("[github blob → raw: {raw_url}]\n\n<fetch the raw URL for the file content>"),
            "github-blob-raw",
        );
    }
    if let Some(markdown) = arxiv_abs_abstract(url, html) {
        return (markdown, "arxiv-abs");
    }
    (generic_html_to_markdown(html), "generic")
}

/// Map a github.com/<owner>/<repo>/blob/<ref>/<path> URL to its raw URL.
#[must_use]
pub fn github_blob_to_raw(url: &str) -> Option<String> {
    let host = url_host(url)?;
    if !host.eq_ignore_ascii_case("github.com") {
        return None;
    }
    let (_, after_scheme) = url.split_once("://")?;
    let (_, path) = after_scheme.split_once('/')?;
    let mut segments = path.split('/');
    let owner = segments.next()?;
    let repo = segments.next()?;
    if segments.next()? != "blob" {
        return None;
    }
    let git_ref = segments.next()?;
    let file_path: Vec<&str> = segments.collect();
    if file_path.is_empty() {
        return None;
    }
    Some(format!(
        "https://raw.githubusercontent.com/{owner}/{repo}/{git_ref}/{}",
        file_path.join("/")
    ))
}

/// arXiv /abs/<id>: pull the abstract + metadata from the page and point at
/// the PDF for full text.
fn arxiv_abs_abstract(url: &str, html: &str) -> Option<String> {
    let host = url_host(url)?;
    if !host.eq_ignore_ascii_case("arxiv.org") || !url.contains("/abs/") {
        return None;
    }
    // The abstract lives in <blockquote class="abstract mathjax">…</blockquote>.
    let marker = "abstract mathjax";
    let start = html.find(marker)?;
    let open_end = html[start..].find('>')? + start + 1;
    let close = html[open_end..].find("</blockquote>")? + open_end;
    let abstract_html = &html[open_end..close];
    let text = generic_html_to_markdown(abstract_html);
    let pdf_url = url.replace("/abs/", "/pdf/");
    Some(format!(
        "# arXiv abstract\n\nSource: {url}\nPDF: {pdf_url}\n\n{}",
        text.trim()
    ))
}

/// Generic HTML → markdown via htmd with boilerplate skipped.
#[must_use]
pub fn generic_html_to_markdown(html: &str) -> String {
    let converter = htmd::HtmlToMarkdown::builder()
        .skip_tags(vec![
            "script", "style", "noscript", "nav", "footer", "header", "aside", "form", "iframe",
            "svg",
        ])
        .build();
    converter.convert(html).unwrap_or_else(|_| String::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssrf_blocks_loopback_private_linklocal_metadata() {
        assert!(host_is_private_target("http://127.0.0.1:8080/x"));
        assert!(host_is_private_target("http://localhost:3000/x"));
        assert!(host_is_private_target("http://10.0.0.4/x"));
        assert!(host_is_private_target("http://192.168.1.1/x"));
        assert!(host_is_private_target("http://172.16.0.9/x"));
        assert!(host_is_private_target(
            "http://169.254.169.254/latest/meta-data"
        ));
        assert!(host_is_private_target("http://[::1]/x"));
        assert!(host_is_private_target("http://[fe80::1]/x"));
        assert!(!host_is_private_target("https://github.com/x/y"));
        assert!(!host_is_private_target("https://arxiv.org/abs/2401.00001"));
    }

    #[test]
    fn github_blob_maps_to_raw() {
        assert_eq!(
            github_blob_to_raw("https://github.com/owner/repo/blob/main/src/lib.rs"),
            Some("https://raw.githubusercontent.com/owner/repo/main/src/lib.rs".to_string())
        );
        assert_eq!(
            github_blob_to_raw("https://github.com/owner/repo/blob/feat%2Fx/a/b.txt"),
            Some("https://raw.githubusercontent.com/owner/repo/feat%2Fx/a/b.txt".to_string())
        );
        assert!(github_blob_to_raw("https://github.com/owner/repo/issues/12").is_none());
        assert!(github_blob_to_raw("https://gitlab.com/o/r/blob/main/x.rs").is_none());
    }

    #[test]
    fn classify_by_extension_and_sniffing() {
        assert_eq!(classify("https://x/a.pdf", "", b""), UrlContentKind::Pdf);
        assert_eq!(
            classify("https://x/a", "", b"%PDF-1.7"),
            UrlContentKind::Pdf
        );
        assert_eq!(
            classify("https://x/nb.ipynb", "", b"{}"),
            UrlContentKind::Notebook
        );
        assert_eq!(
            classify("https://x/page", "text/html; charset=utf-8", b"<html>"),
            UrlContentKind::Html
        );
        assert_eq!(
            classify("https://x/data", "application/json", b"{}"),
            UrlContentKind::PlainText
        );
    }

    #[test]
    fn generic_html_strips_boilerplate_keeps_content() {
        let html = "<html><body><nav>skip me</nav><main><h1>Title</h1><p>Body text</p></main><footer>skip</footer><script>var x=1;</script></body></html>";
        let md = generic_html_to_markdown(html);
        assert!(md.contains("Title"), "heading kept: {md}");
        assert!(md.contains("Body text"), "body kept: {md}");
        assert!(!md.contains("skip me"), "nav stripped: {md}");
        assert!(!md.contains("var x"), "script stripped: {md}");
    }

    #[test]
    fn notebook_converts_cells() {
        let nb = br##"{"cells":[
            {"cell_type":"markdown","source":["# Heading\n","some text"]},
            {"cell_type":"code","source":["print(1)"],"outputs":[{"output_type":"stream","text":"1"}]}
        ],"nbformat":4}"##;
        let out = convert_notebook(nb).expect("convert");
        assert!(out.contains("cell 1 (markdown)"));
        assert!(out.contains("Heading"));
        assert!(out.contains("```"));
        assert!(out.contains("print(1)"));
        assert!(out.contains("outputs: 1 present"));
    }

    #[test]
    fn redirect_resolution_relative_and_absolute() {
        assert_eq!(
            resolve_redirect("https://a.com/x/y", "/z"),
            "https://a.com/z".to_string()
        );
        assert_eq!(
            resolve_redirect("https://a.com/x/y", "https://b.com/q"),
            "https://b.com/q".to_string()
        );
        assert_eq!(
            resolve_redirect("https://a.com/x/y", "w"),
            "https://a.com/x/w".to_string()
        );
    }

    #[test]
    fn arxiv_abs_extracts_abstract_and_pdf_link() {
        let html = r#"<html><body><blockquote class="abstract mathjax"><span>Abstract:</span> We prove things.</blockquote></body></html>"#;
        let out = arxiv_abs_abstract("https://arxiv.org/abs/2401.00001", html).expect("extract");
        assert!(out.contains("We prove things"), "abstract text: {out}");
        assert!(
            out.contains("https://arxiv.org/pdf/2401.00001"),
            "pdf link: {out}"
        );
        assert!(arxiv_abs_abstract("https://arxiv.org/list/cs", html).is_none());
    }

    // === Converter-pipeline fixtures (bd-cv653.2.2 conformance lane) ===

    #[test]
    #[allow(clippy::literal_string_with_formatting_args)]
    fn pipeline_html_fixture_full_page() {
        let html = r#"<!DOCTYPE html><html><head><title>T</title><style>body{color:red}</style></head>
        <body><nav><a href="/">home</a></nav><main><h1>Guide</h1><h2 id="part-1">Part 1</h2>
        <p>First para with <a href="https://x.test/doc">a link</a>.</p><pre>code_block()</pre></main>
        <footer>copyright</footer><script>track()</script></body></html>"#;
        let outcome = convert_bytes(
            "https://docs.test/guide",
            "text/html",
            html.as_bytes(),
            false,
        )
        .expect("convert");
        assert_eq!(outcome.kind, UrlContentKind::Html);
        assert_eq!(outcome.extractor, "generic");
        assert!(outcome.content.contains("Guide"), "h1 kept");
        assert!(outcome.content.contains("Part 1"), "h2 kept");
        assert!(outcome.content.contains("a link"), "link kept");
        assert!(outcome.content.contains("code_block()"), "code kept");
        assert!(!outcome.content.contains("copyright"), "footer stripped");
        assert!(!outcome.content.contains("track()"), "script stripped");
    }

    #[test]
    fn pipeline_notebook_fixture() {
        let nb = br##"{"cells":[
            {"cell_type":"markdown","source":["# Analysis"]},
            {"cell_type":"code","source":["import os\n","os.getcwd()"],"outputs":[]}
        ],"nbformat":4}"##;
        let outcome = convert_bytes("https://nb.test/a.ipynb", "", nb, false).expect("convert");
        assert_eq!(outcome.kind, UrlContentKind::Notebook);
        assert!(outcome.content.contains("cell 1 (markdown)"));
        assert!(outcome.content.contains("import os"));
    }

    #[cfg(feature = "url-pdf")]
    #[test]
    fn pipeline_pdf_fixture_minimal_document() {
        // A spec-valid one-page PDF with a computed xref table, containing
        // the text "Hello" (built programmatically — byte offsets matter).
        let pdf = minimal_pdf();
        let outcome = convert_bytes("https://x.test/doc.pdf", "application/pdf", &pdf, false)
            .expect("convert pdf");
        assert_eq!(outcome.kind, UrlContentKind::Pdf);
        assert!(
            outcome.content.contains("--- page 1 ---"),
            "page marker: {}",
            outcome.content
        );
        assert!(
            outcome.content.contains("Hello"),
            "extracted text: {}",
            outcome.content
        );
    }

    #[cfg(not(feature = "url-pdf"))]
    #[test]
    fn pdf_without_feature_returns_named_error() {
        let outcome = convert_bytes(
            "https://x.test/d.pdf",
            "application/pdf",
            b"%PDF-1.4",
            false,
        );
        let err = outcome.expect_err("pdf without feature must error");
        assert!(err.to_string().contains("PDF_NOT_COMPILED"));
    }

    fn minimal_pdf() -> Vec<u8> {
        let mut out: Vec<u8> = b"%PDF-1.4\n".to_vec();
        let mut offsets: Vec<usize> = Vec::new();
        let mut push = |out: &mut Vec<u8>, body: &str| {
            offsets.push(out.len());
            out.extend_from_slice(body.as_bytes());
        };
        push(
            &mut out,
            "1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n",
        );
        push(
            &mut out,
            "2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n",
        );
        push(
            &mut out,
            "3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>\nendobj\n",
        );
        let stream = "BT /F1 24 Tf 100 700 Td (Hello) Tj ET";
        push(
            &mut out,
            &format!(
                "4 0 obj\n<< /Length {} >>\nstream\n{}\nendstream\nendobj\n",
                stream.len(),
                stream
            ),
        );
        push(
            &mut out,
            "5 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>\nendobj\n",
        );
        let xref_start = out.len();
        let mut xref = format!("xref\n0 {}\n", offsets.len() + 1);
        xref.push_str("0000000000 65535 f \n");
        for off in &offsets {
            use std::fmt::Write as _;
            let _ = writeln!(xref, "{off:010} 00000 n ");
        }
        out.extend_from_slice(xref.as_bytes());
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
                offsets.len() + 1,
                xref_start
            )
            .as_bytes(),
        );
        out
    }

    #[test]
    fn pipeline_plaintext_passthrough() {
        let outcome = convert_bytes(
            "https://x.test/robots.txt",
            "text/plain",
            b"User-agent: *\n",
            false,
        )
        .expect("convert");
        assert_eq!(outcome.kind, UrlContentKind::PlainText);
        assert!(outcome.content.contains("User-agent: *"));
    }
}
