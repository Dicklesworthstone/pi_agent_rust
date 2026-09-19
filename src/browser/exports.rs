//! Bounded page exports with atomic, no-clobber artifact publication.

use super::cdp::Cdp;
use super::{output, required};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::model::{ContentBlock, ImageContent};
use crate::tools::ToolOutput;
use base64::Engine as _;
use serde_json::{Value, json};
use std::path::Path;

const MAX_EXPORT_BYTES: usize = 20 * 1024 * 1024;
const MAX_CAPTURE_PIXELS: f64 = 128.0 * 1024.0 * 1024.0;

fn error(message: impl Into<String>) -> Error {
    Error::tool("browser", message)
}

fn flag(args: &Value, name: &str, default: bool) -> Result<bool> {
    args.get(name).map_or(Ok(default), |value| {
        value
            .as_bool()
            .ok_or_else(|| error(format!("{name} must be a boolean")))
    })
}

pub(super) fn validate(args: &Value) -> Result<()> {
    let action = required(args, "action")?;
    let allowed: &[&str] = if action == "print_pdf" {
        &["landscape", "print_background", "page_ranges"]
    } else {
        &["full_page"]
    };
    let object = args
        .as_object()
        .ok_or_else(|| error("export arguments must be an object"))?;
    for field in object.keys() {
        if !matches!(
            field.as_str(),
            "action" | "tab" | "output_path" | "timeout_ms"
        ) && !allowed.contains(&field.as_str())
        {
            return Err(error(format!("unsupported {action} parameter: {field}")));
        }
    }
    for name in ["full_page", "landscape", "print_background"] {
        flag(args, name, false)?;
    }
    if let Some(value) = args.get("page_ranges") {
        let ranges = value
            .as_str()
            .ok_or_else(|| error("page_ranges must be a string"))?;
        if ranges.len() > 1024
            || !ranges
                .bytes()
                .all(|byte| byte.is_ascii_digit() || b",- ".contains(&byte))
        {
            return Err(error(
                "page_ranges must be at most 1024 bytes of page numbers/ranges, e.g. 1-3,5",
            ));
        }
    }
    if let Some(value) = args.get("output_path") {
        let path = value
            .as_str()
            .filter(|path| !path.is_empty() && path.len() <= 4096 && !path.contains('\0'))
            .ok_or_else(|| {
                error("output_path must be a nonempty NUL-free string of at most 4096 bytes")
            })?;
        let extension = if action == "print_pdf" { "pdf" } else { "png" };
        if !Path::new(path)
            .extension()
            .is_some_and(|value| value.eq_ignore_ascii_case(extension))
        {
            return Err(error(format!("{action} output_path must use .{extension}")));
        }
    }
    Ok(())
}

pub(super) async fn execute(
    owner: &AgentCx,
    cdp: &mut Cdp,
    cwd: &Path,
    tab: &str,
    args: &Value,
) -> Result<ToolOutput> {
    validate(args)?;
    let pdf = required(args, "action")? == "print_pdf";
    let extension = if pdf { "pdf" } else { "png" };
    let folder = if pdf { "exports" } else { "screenshots" };
    let requested = args.get("output_path").and_then(Value::as_str).map_or_else(
        || {
            format!(
                "{folder}/browser_{}.{extension}",
                uuid::Uuid::new_v4().simple()
            )
        },
        ToString::to_string,
    );
    // Resolve before asking Chromium to render. The shared publisher repeats
    // destination checks under a pinned workspace directory at commit time.
    let target = crate::artifact_output::resolve_new(cwd, &requested, "browser")?;
    let response = if pdf {
        let mut parameters = json!({
            "transferMode": "ReturnAsBase64",
            "preferCSSPageSize": true,
            "printBackground": flag(args, "print_background", true)?,
            "landscape": flag(args, "landscape", false)?
        });
        if let Some(ranges) = args.get("page_ranges") {
            parameters["pageRanges"] = ranges.clone();
        }
        cdp.command(owner, "Page.printToPDF", parameters).await?
    } else {
        let mut parameters = json!({"format": "png", "fromSurface": true});
        if flag(args, "full_page", false)? {
            let metrics = cdp
                .command(owner, "Page.getLayoutMetrics", json!({}))
                .await?;
            parameters["clip"] = clip(&metrics)?;
            parameters["captureBeyondViewport"] = json!(true);
        }
        cdp.command(owner, "Page.captureScreenshot", parameters)
            .await?
    };
    let bytes = decode(&response, pdf)?;
    owner
        .checkpoint()
        .map_err(|_| error("page export cancelled before publication"))?;
    owner
        .checkpoint()
        .map_err(|_| error("page export cancelled before publication"))?;
    crate::artifact_output::publish(&target, &bytes, "browser")?;
    let preview = !pdf && bytes.len() <= crate::tools::IMAGE_MAX_BYTES;
    let mime = if pdf { "application/pdf" } else { "image/png" };
    let mut result = output(
        format!(
            "Exported tab {tab} to {} ({mime}, {} bytes){}",
            target.path().display(),
            bytes.len(),
            if !pdf && !preview {
                "; capture exceeds the inline image budget; inspect the saved file"
            } else {
                ""
            }
        ),
        json!({
            "tab": tab, "saved_path": target.path().display().to_string(), "size_bytes": bytes.len(),
            "mime_type": mime, "preview_included": preview, "backend": "cdp",
            "full_page": !pdf && flag(args, "full_page", false)?
        }),
    );
    if preview {
        result.content.push(ContentBlock::Image(ImageContent {
            data: base64::engine::general_purpose::STANDARD.encode(&bytes),
            mime_type: "image/png".into(),
        }));
    }
    Ok(result)
}

fn clip(metrics: &Value) -> Result<Value> {
    let size = metrics
        .get("cssContentSize")
        .ok_or_else(|| error("full-page capture requires CSS content metrics"))?;
    let number = |name: &str| {
        size[name]
            .as_f64()
            .filter(|value| value.is_finite())
            .ok_or_else(|| error(format!("invalid page content {name}")))
    };
    let (x, y, width, height) = (
        number("x")?,
        number("y")?,
        number("width")?,
        number("height")?,
    );
    if x.abs() > 1_000_000.0
        || y.abs() > 1_000_000.0
        || width <= 0.0
        || height <= 0.0
        || width > 32768.0
        || height > 32768.0
        || width.ceil() * height.ceil() > MAX_CAPTURE_PIXELS
    {
        return Err(error(
            "full-page capture exceeds 32768 pixels per side or 128 megapixels",
        ));
    }
    Ok(json!({"x": x, "y": y, "width": width, "height": height, "scale": 1}))
}

fn decode(response: &Value, pdf: bool) -> Result<Vec<u8>> {
    let encoded = required(response, "data")?;
    if encoded.is_empty() || encoded.len() > MAX_EXPORT_BYTES.div_ceil(3) * 4 {
        return Err(error("page export is empty or exceeds 20 MiB"));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| error("page export contains invalid base64"))?;
    if bytes.len() > MAX_EXPORT_BYTES {
        return Err(error("page export exceeds 20 MiB"));
    }
    if pdf {
        let end = bytes
            .iter()
            .rposition(|byte| !byte.is_ascii_whitespace())
            .map_or(0, |index| index + 1);
        if !bytes.starts_with(b"%PDF-") || !bytes[..end].ends_with(b"%%EOF") {
            return Err(error("Chromium did not return a complete PDF container"));
        }
    } else {
        if bytes.len() < 45
            || !bytes.starts_with(b"\x89PNG\r\n\x1a\n")
            || bytes.get(12..16) != Some(b"IHDR".as_slice())
            || !bytes.ends_with(b"\0\0\0\0IEND\xaeB`\x82")
        {
            return Err(error("Chromium did not return a complete PNG container"));
        }
        let width = u32::from_be_bytes(bytes[16..20].try_into().expect("checked PNG length"));
        let height = u32::from_be_bytes(bytes[20..24].try_into().expect("checked PNG length"));
        if width == 0 || height == 0 || u64::from(width) * u64::from(height) > 128 * 1024 * 1024 {
            return Err(error("PNG dimensions are empty or exceed 128 megapixels"));
        }
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_parameters_do_not_ignore_wrong_types_or_cross_format_options() {
        for args in [
            json!({"action":"print_pdf","full_page":true}),
            json!({"action":"screenshot","landscape":true}),
            json!({"action":"screenshot","full_page":"yes"}),
            json!({"action":"print_pdf","page_ranges":"javascript:run()"}),
            json!({"action":"print_pdf","output_path":"image.png"}),
        ] {
            assert!(validate(&args).is_err(), "{args}");
        }
        assert!(
            validate(
                &json!({"action":"print_pdf","page_ranges":"1-3,5","output_path":"report.PDF"})
            )
            .is_ok()
        );
    }

    #[test]
    #[allow(clippy::case_sensitive_file_extension_comparisons)]
    fn export_paths_cannot_escape_or_cross_symlinked_ancestors() {
        let dir = tempfile::tempdir().unwrap();
        for path in [
            "../escape.png",
            "/tmp/escape.png",
            "a/../../escape.pdf",
            "a\\escape.png",
        ] {
            let action = if path.ends_with(".pdf") {
                "print_pdf"
            } else {
                "screenshot"
            };
            let args = json!({"action":action,"output_path":path});
            assert!(
                validate(&args).is_ok(),
                "schema validation stays format-focused"
            );
            assert!(crate::artifact_output::resolve_new(dir.path(), path, "browser").is_err());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let outside = tempfile::tempdir().unwrap();
            symlink(outside.path(), dir.path().join("linked")).unwrap();
            assert!(
                crate::artifact_output::resolve_new(dir.path(), "linked/capture.png", "browser")
                    .is_err()
            );
        }
    }

    #[test]
    fn full_page_geometry_uses_css_metrics_and_is_bounded() {
        let metrics = json!({"cssContentSize":{"x":0,"y":0,"width":800,"height":3000}});
        assert_eq!(clip(&metrics).unwrap()["height"], 3000.0);
        assert!(clip(&json!({"contentSize":{"width":800,"height":3000}})).is_err());
        assert!(
            clip(&json!({"cssContentSize":{"x":0,"y":0,"width":20000,"height":20000}})).is_err()
        );
        assert!(clip(&json!({"cssContentSize":{"x":0,"y":0,"width":0,"height":1}})).is_err());
    }

    #[test]
    fn truncated_containers_and_existing_destinations_are_rejected() {
        let encode =
            |bytes: &[u8]| json!({"data":base64::engine::general_purpose::STANDARD.encode(bytes)});
        assert!(decode(&encode(b"%PDF-1.7\npartial"), true).is_err());
        assert!(decode(&encode(b"\x89PNG\r\n\x1a\npartial"), false).is_err());
        let pdf = b"%PDF-1.7\nfixture container only\n%%EOF\n";
        assert_eq!(decode(&encode(pdf), true).unwrap(), pdf);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("existing.pdf");
        std::fs::write(&path, b"do not replace").unwrap();
        assert!(
            crate::artifact_output::resolve_new(dir.path(), "existing.pdf", "browser").is_err()
        );
        assert_eq!(std::fs::read(path).unwrap(), b"do not replace");
    }
}
