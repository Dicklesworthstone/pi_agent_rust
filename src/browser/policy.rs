//! URL checks shared by live and explicitly simulated browser backends.

use crate::error::{Error, Result};
use url::Url;

pub(super) fn check_navigation(raw: &str, allowlist: Option<&[String]>) -> Result<()> {
    if raw == "about:blank" {
        return Ok(());
    }
    let url = Url::parse(raw).map_err(|e| Error::tool("browser", format!("invalid URL: {e}")))?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(Error::tool(
            "browser",
            "navigation requires HTTP(S) without URL credentials, or about:blank",
        ));
    }
    let host = url
        .host_str()
        .ok_or_else(|| Error::tool("browser", "navigation URL has no host"))?;
    if let Some(allowed) = allowlist
        && !allowed.iter().any(|rule| host_matches(host, rule))
    {
        return Err(Error::tool(
            "browser",
            format!("navigation to {raw} blocked by domain allowlist"),
        ));
    }
    Ok(())
}

/// A download event is not a navigation request. Call only after the capture
/// has admitted the initiating document and matched the event's source frame.
/// Never use a data payload or a revocable Blob handle as model-facing text.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct DownloadUrl {
    pub(super) display_url: String,
    pub(super) kind: &'static str,
    pub(super) redacted: bool,
}

fn download_url_error() -> Error {
    Error::tool(
        "browser",
        "BROWSER_DOWNLOAD_URL: expected bounded HTTP(S), data, or a Blob URL with an allowed HTTP(S) creator origin; opaque Blob origins and other schemes are unsupported",
    )
}

/// Authorize the browser-reported resource, not a model-provided URL. Chromium
/// performs the transfer; this function never fetches/decodes a URL or expands
/// navigation policy. The 64-KiB bound is also enforced by the event tracker.
pub(super) fn download_url(raw: &str, allowlist: Option<&[String]>) -> Result<DownloadUrl> {
    if raw.is_empty()
        || raw.len() > 64 * 1024
        || raw.trim() != raw
        || raw.chars().any(char::is_control)
    {
        return Err(download_url_error());
    }
    let mut parsed = Url::parse(raw).map_err(|_| download_url_error())?;
    let (kind, display_url) = match parsed.scheme() {
        "http" | "https" => {
            check_navigation(raw, allowlist).map_err(|_| download_url_error())?;
            let kind = if parsed.scheme() == "https" { "https" } else { "http" };
            // Signed export links commonly put credentials in these fields.
            // Keep the original URL only in the private immutable tracker.
            parsed.set_query(None);
            parsed.set_fragment(None);
            (kind, parsed.to_string())
        }
        "blob" => {
            // A Blob URL has no outer host. Inspect its embedded creator URL,
            // never a substring or a recursively unwrapped arbitrary scheme.
            let creator = Url::parse(parsed.path()).map_err(|_| download_url_error())?;
            if !matches!(creator.scheme(), "http" | "https")
                || creator.path().trim_matches('/').is_empty()
                || parsed.query().is_some()
            {
                return Err(download_url_error());
            }
            check_navigation(creator.as_str(), allowlist).map_err(|_| download_url_error())?;
            ("blob", format!("blob:{}/[redacted]", creator.origin().ascii_serialization()))
        }
        "data" => {
            // The browser has already interpreted this literal resource. Do
            // not decode another copy or expose its MIME parameters/payload.
            // Data has no remote creator host; the pinned source policy still
            // applies, including every selected frame ancestor.
            if !parsed.path().contains(',') {
                return Err(download_url_error());
            }
            ("data", "data:[redacted]".to_owned())
        }
        _ => return Err(download_url_error()),
    };
    Ok(DownloadUrl {
        redacted: display_url != raw,
        display_url,
        kind,
    })
}

fn host_matches(host: &str, rule: &str) -> bool {
    let rule = rule.trim().to_ascii_lowercase();
    if rule == "*" {
        return true;
    }
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let (subdomains, domain) = rule
        .strip_prefix("*.")
        .map_or((false, rule.as_str()), |s| (true, s));
    // Parse rules as hosts, never as substrings of a URL. This also normalizes IDNA.
    let Ok(parsed) = url::Host::parse(domain.trim_end_matches('.')) else {
        return false;
    };
    let domain = parsed.to_string();
    if subdomains {
        host.len() > domain.len() + 1 && host.ends_with(&format!(".{domain}"))
    } else {
        host == domain
    }
}

/// CDP is a powerful unauthenticated local control channel. Do not let endpoint
/// discovery turn an explicit loopback attachment into arbitrary remote access.
pub(super) fn endpoint(raw: &str, websocket: bool) -> Result<Url> {
    let url = Url::parse(raw)
        .map_err(|e| Error::tool("browser", format!("invalid CDP endpoint: {e}")))?;
    let local = match url.host() {
        Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        // The current native WebSocket handshake does not support IPv6 URL
        // authorities reliably. Require IPv4 loopback rather than misroute it.
        _ => false,
    };
    if !local
        || url.scheme() != if websocket { "ws" } else { "http" }
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || (!websocket && url.path() != "/")
        || (websocket && !url.path().starts_with("/devtools/browser/"))
    {
        return Err(Error::tool(
            "browser",
            "CDP endpoint must be an unauthenticated IPv4-loopback or localhost URL (HTTP discovery / WebSocket browser target)",
        ));
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_download_admission_does_not_relax_navigation_or_cdp_endpoints() {
        let rules = vec!["example.com".to_string()];
        for raw in ["data:text/plain,export", "blob:https://example.com/export-id"] {
            assert!(download_url(raw, Some(&rules)).is_ok());
            assert!(check_navigation(raw, Some(&rules)).is_err());
            assert!(endpoint(raw, false).is_err());
            assert!(endpoint(raw, true).is_err());
        }
        assert!(download_url("about:blank", None).is_err());
    }

    #[test]
    fn blob_downloads_check_creator_hosts_and_preserve_allowlist_rules() {
        let rules = vec!["example.com".to_string(), "*.allowed.test".to_string()];
        for raw in [
            "blob:https://EXAMPLE.com:443/opaque-id",
            "blob:http://example.com:8080/opaque-id",
            "blob:https://child.allowed.test/id#fragment",
        ] {
            assert_eq!(download_url(raw, Some(&rules)).unwrap().kind, "blob");
        }
        for raw in [
            "blob:https://blocked.test/example.com",
            "blob:https://example.com.blocked.test/id",
            "blob:https://sub.example.com/id",
            "blob:https://allowed.test/id",
        ] {
            assert!(download_url(raw, Some(&rules)).is_err());
        }
        assert!(download_url("blob:https://example.com/id", Some(&[])).is_err());
        let idna = vec!["bücher.example".to_string()];
        assert!(download_url("blob:https://xn--bcher-kva.example/id", Some(&idna)).is_ok());
    }

    #[test]
    fn blob_downloads_reject_unknown_origins_credentials_and_nested_schemes() {
        for raw in [
            "blob:null/opaque-id", "blob:file:///private/id", "blob:ftp://example.com/id",
            "blob:blob:https://example.com/id", "blob:data:text/plain,secret",
            "blob:javascript:secret", "blob:https://user:secret@example.com/id",
            "blob:https://example.com/", "blob:https://example.com/id?secret=value",
        ] {
            assert!(download_url(raw, None).is_err(), "unexpected admission");
        }
    }

    #[test]
    fn download_projection_omits_data_payloads_blob_handles_and_signed_url_queries() {
        for raw in [
            "data:text/plain;PRIVATE-CANARY=value,PRIVATE-CANARY",
            "data:application/octet-stream;base64,UFJJVkFURS1DQU5BUlk=",
            "data:,", "DATA:text/plain,PRIVATE-CANARY?also=private#fragment",
        ] {
            let resource = download_url(raw, Some(&[])).unwrap();
            assert_eq!(resource.display_url, "data:[redacted]");
            assert_eq!(resource.kind, "data");
            assert!(resource.redacted);
            assert!(!format!("{resource:?}").contains("PRIVATE-CANARY"));
        }
        let blob = download_url("blob:https://example.com/PRIVATE-CANARY#secret", None).unwrap();
        assert_eq!(blob.display_url, "blob:https://example.com/[redacted]");
        assert!(blob.redacted);
        let signed = download_url("https://example.com/export?token=PRIVATE-CANARY#secret", None).unwrap();
        assert_eq!(signed.display_url, "https://example.com/export");
        assert_eq!(signed.kind, "https");
        assert!(signed.redacted);
        assert!(!download_url("http://example.com/export", None).unwrap().redacted);
    }

    #[test]
    fn download_urls_are_byte_bounded_and_errors_never_echo_resource_data() {
        let prefix = "data:text/plain,";
        let exact = format!("{prefix}{}", "x".repeat(64 * 1024 - prefix.len()));
        assert!(download_url(&exact, None).is_ok());
        assert!(download_url(&format!("{exact}x"), None).is_err());
        for raw in [
            "", "data:text/plain", " data:,PRIVATE-CANARY", "data:,PRIVATE-CANARY\n",
            "data:,PRIVATE\0CANARY", "file:///PRIVATE-CANARY", "javascript:PRIVATE-CANARY",
            "https://blocked.test/PRIVATE-CANARY", "blob:https://blocked.test/PRIVATE-CANARY",
        ] {
            let error = download_url(raw, Some(&[])).unwrap_err().to_string();
            assert!(error.contains("BROWSER_DOWNLOAD_URL"));
            assert!(!error.contains("PRIVATE-CANARY"));
        }
        let unicode = format!("{prefix}{}", "🦀".repeat(16 * 1024));
        assert!(download_url(&unicode, None).is_err());
    }

    #[test]
    fn allowlist_matches_hosts_not_userinfo_paths_queries_or_suffixes() {
        let rules = vec!["example.com".into()];
        for url in ["https://example.com/", "https://EXAMPLE.com.:443/docs"] {
            assert!(check_navigation(url, Some(&rules)).is_ok(), "{url}");
        }
        for url in [
            "https://evil.test/?next=example.com",
            "https://evil.test/example.com",
            "https://example.com.evil.test",
            "https://notexample.com",
            "https://example.com@evil.test",
            "https://sub.example.com",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:text/html,hello",
        ] {
            assert!(check_navigation(url, Some(&rules)).is_err(), "{url}");
        }
    }

    #[test]
    fn wildcard_has_a_label_boundary_and_empty_policy_denies_network() {
        assert!(host_matches("a.b.example.com", "*.example.com"));
        assert!(!host_matches("example.com", "*.example.com"));
        assert!(!host_matches("notexample.com", "*.example.com"));
        assert!(!host_matches("example.com", "https://example.com"));
        assert!(check_navigation("about:blank", Some(&[])).is_ok());
        assert!(check_navigation("https://example.com", Some(&[])).is_err());
    }

    #[test]
    fn cdp_discovery_rejects_remote_credentialed_and_ambiguous_endpoints() {
        assert!(endpoint("http://127.0.0.1:9222", false).is_ok());
        assert!(endpoint("ws://localhost:9222/devtools/browser/uuid", true).is_ok());
        for raw in [
            "http://example.com:9222",
            "http://127.0.0.1.evil.test:9222",
            "http://user:pass@localhost:9222",
            "http://localhost:9222/?token=x",
            "http://localhost:9222/proxy",
            "file:///json/version",
        ] {
            assert!(endpoint(raw, false).is_err(), "{raw}");
        }
    }
}
