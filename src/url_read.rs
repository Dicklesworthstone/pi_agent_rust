//! URL-aware reads (bd-cv653.2.2).
//!
//! `read` on http(s):// paths fetches and converts to reader-mode markdown,
//! with site-aware extractors, PDF text extraction, notebook rendering, and a
//! default-on SSRF guard.
//!
//! Design notes:
//! - GitHub file pages resolve to their actual raw document before downloading.
//! - HTML converts through a boilerplate-skipping htmd pipeline.
//! - `:raw` bypasses source rewriting and conversion; offset/limit page the
//!   returned text exactly like file reads.
//! - Redirects and synthesized source URLs share admission and one deadline.

use crate::error::Error;

mod navigation;
mod notebook;
pub use navigation::{fetch, github_blob_to_raw, host_is_private_target};

/// Maximum download size for a URL read (10 MiB before conversion).
const MAX_DOWNLOAD_BYTES: usize = 10 * 1024 * 1024;

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

/// Whether a URL read should produce reader-mode text or the wire body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlReadMode {
    Reader,
    Raw,
}

/// A fetched URL read rendered for tool output.
#[derive(Debug)]
pub struct UrlReadOutcome {
    pub content: String,
    pub kind: UrlContentKind,
    /// Which extractor produced the content (`generic` when no site rule hit).
    pub extractor: &'static str,
    /// The final URL after redirects and source resolution.
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

fn raw_bytes(
    url: &str,
    wire_content_type: &str,
    bytes: &[u8],
    download_truncated: bool,
) -> UrlReadOutcome {
    UrlReadOutcome {
        content: String::from_utf8_lossy(bytes).into_owned(),
        kind: classify(url, wire_content_type, bytes),
        extractor: "raw",
        final_url: url.to_string(),
        wire_content_type: wire_content_type.to_string(),
        download_truncated,
    }
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
        UrlContentKind::Notebook => {
            if download_truncated {
                return Err(Error::tool("read", "[NOTEBOOK_INCOMPLETE] cannot interpret a truncated notebook"));
            }
            (convert_notebook(bytes)?, "notebook")
        }
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
    if ext_is("ipynb") || ct.contains("application/x-ipynb+json") {
        return UrlContentKind::Notebook;
    }
    if ct.contains("text/html") || ct.contains("application/xhtml") {
        return UrlContentKind::Html;
    }
    if notebook::recognizes(bytes) {
        return UrlContentKind::Notebook;
    }
    if ct.starts_with("text/") || ct.contains("json") || ct.contains("xml") {
        return UrlContentKind::PlainText;
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
    notebook::render(bytes)
}

/// HTML conversion with site-aware extraction. Returns (markdown, extractor).
fn convert_html(url: &str, html: &str) -> (String, &'static str) {
    // Source resolution belongs to fetch, not this network-free converter.
    // Offline HTML conversion must return the supplied content, not a fake read.
    if let Some(markdown) = arxiv_abs_abstract(url, html) {
        return (markdown, "arxiv-abs");
    }
    (generic_html_to_markdown(html), "generic")
}

/// arXiv /abs/<id>: pull the abstract + metadata from the page and point at
/// the PDF for full text.
fn arxiv_abs_abstract(url: &str, html: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    if parsed.host_str() != Some("arxiv.org") || !parsed.path().starts_with("/abs/") {
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
            navigation::resolve_redirect("https://a.com/x/y", "/z").unwrap(),
            "https://a.com/z".to_string()
        );
        assert_eq!(
            navigation::resolve_redirect("https://a.com/x/y", "https://b.com/q").unwrap(),
            "https://b.com/q".to_string()
        );
        assert_eq!(
            navigation::resolve_redirect("https://a.com/x/y", "w").unwrap(),
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

    #[test]
    fn raw_mode_preserves_wire_html_without_reader_conversion() {
        let html = b"<!DOCTYPE html><html><body><nav>raw navigation</nav><script>raw_script()</script></body></html>";
        let outcome = raw_bytes("https://x.test/page", "text/html", html, false);
        assert_eq!(outcome.kind, UrlContentKind::Html);
        assert_eq!(outcome.extractor, "raw");
        assert_eq!(outcome.content.as_bytes(), html);
    }
}
