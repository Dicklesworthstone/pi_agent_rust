//! Bounded local references for provider-native image editing.
//! Never ask a provider to fetch an arbitrary source URL or overwrite an input.

use super::super::transport;
use crate::error::{Error, Result};
use base64::Engine as _;
use serde_json::{Value, json};
use std::path::Path;

const NAME: &str = "generate_image";
const MAX_REFERENCES: usize = 5;
const MAX_INPUT_BYTES: u64 = 20 * 1024 * 1024;
const MAX_OPENAI_DATA_URL_BYTES: usize = 20 * 1024 * 1024;

// Keep shared input budgets and provider-specific edit payload assembly in one flow.
#[allow(clippy::too_many_lines)]
pub(super) fn attach(
    cwd: &Path,
    workspace: &crate::workspace::WorkspaceHandle,
    provider: &str,
    model: &str,
    args: &Value,
    endpoint: &mut String,
    payload: &mut Value,
) -> Result<usize> {
    let single = transport::optional(args, NAME, "image_path")?;
    let multiple = args.get("image_paths");
    if single.is_some() && multiple.is_some() {
        return Err(Error::tool(NAME, "use image_path or image_paths, not both"));
    }
    let paths = match (single, multiple) {
        (Some(path), None) => vec![path],
        (None, Some(value)) => {
            let array = value
                .as_array()
                .filter(|paths| !paths.is_empty() && paths.len() <= MAX_REFERENCES)
                .ok_or_else(|| Error::tool(NAME, "image_paths must contain 1..=5 local paths"))?;
            array
                .iter()
                .map(|value| {
                    value.as_str().ok_or_else(|| {
                        Error::tool(NAME, "every image_paths entry must be a string")
                    })
                })
                .collect::<Result<Vec<_>>>()?
        }
        _ => Vec::new(),
    };
    let mask = transport::optional(args, NAME, "mask_path")?;
    let fidelity = transport::optional(args, NAME, "input_fidelity")?;
    if paths.is_empty() {
        if mask.is_some() || fidelity.is_some() {
            return Err(Error::tool(
                NAME,
                "mask_path and input_fidelity require a source image",
            ));
        }
        return Ok(0);
    }
    if provider != "openai" && (mask.is_some() || fidelity.is_some()) {
        return Err(Error::tool(
            NAME,
            "mask_path and input_fidelity are OpenAI editing options",
        ));
    }
    if provider == "openai" && !model.starts_with("gpt-image-") {
        return Err(Error::tool(
            NAME,
            "native OpenAI JSON editing requires a gpt-image-* model",
        ));
    }
    if fidelity.is_some_and(|value| !matches!(value, "low" | "high")) {
        return Err(Error::tool(NAME, "input_fidelity must be low or high"));
    }
    let mut budget = MAX_INPUT_BYTES;
    let mut references = Vec::with_capacity(paths.len());
    for path in &paths {
        let (mime, encoded) = load(cwd, workspace, path, &mut budget)?;
        references.push(match provider {
            "gemini" => json!({"inlineData": {"mimeType": mime, "data": encoded}}),
            "openai" => json!({"image_url": data_url(mime, &encoded, true)?}),
            "xai" => json!({"type": "image_url", "url": data_url(mime, &encoded, false)?}),
            _ => return Err(Error::tool(NAME, "unsupported image editing provider")),
        });
    }
    match provider {
        "gemini" => {
            let parts = payload
                .pointer_mut("/contents/0/parts")
                .and_then(Value::as_array_mut)
                .ok_or_else(|| Error::tool(NAME, "invalid Gemini image request"))?;
            references.append(parts);
            *parts = references;
            // Editing should preserve the input's shape unless explicitly asked.
            if args.get("aspect_ratio").is_none()
                && let Some(config) = payload
                    .pointer_mut("/generationConfig/responseFormat/image")
                    .and_then(Value::as_object_mut)
            {
                config.remove("aspectRatio");
            }
        }
        "openai" => {
            *endpoint = "images/edits".to_string();
            payload["images"] = json!(references);
            if args.get("size").is_none() {
                payload["size"] = json!("auto");
            }
            if let Some(fidelity) = fidelity {
                payload["input_fidelity"] = json!(fidelity);
            }
            if let Some(mask) = mask {
                let (mime, encoded) = load(cwd, workspace, mask, &mut budget)?;
                if mime != "image/png" {
                    return Err(Error::tool(NAME, "mask_path must contain a PNG alpha mask"));
                }
                payload["mask"] = json!({"image_url": data_url(mime, &encoded, true)?});
            }
        }
        "xai" => {
            *endpoint = "images/edits".to_string();
            if references.len() == 1 {
                payload["image"] = references.pop().expect("one reference");
            } else {
                payload["images"] = json!(references);
            }
            if args.get("aspect_ratio").is_none()
                && let Some(body) = payload.as_object_mut()
            {
                body.remove("aspect_ratio");
            }
        }
        _ => return Err(Error::tool(NAME, "unsupported image editing provider")),
    }
    Ok(paths.len())
}

fn load(
    cwd: &Path,
    workspace: &crate::workspace::WorkspaceHandle,
    path: &str,
    remaining: &mut u64,
) -> Result<(&'static str, String)> {
    if path.trim().is_empty() || path.len() > 4096 {
        return Err(Error::tool(
            NAME,
            "image paths must be nonempty and at most 4096 bytes",
        ));
    }
    let bytes = transport::read_capped(Path::new(path), cwd, workspace, NAME, *remaining)?;
    *remaining = remaining.saturating_sub(bytes.len() as u64);
    let mime = transport::image_mime(&bytes).ok_or_else(|| {
        Error::tool(
            NAME,
            "image inputs must contain PNG, JPEG, WebP or GIF bytes",
        )
    })?;
    Ok((
        mime,
        base64::engine::general_purpose::STANDARD.encode(bytes),
    ))
}

fn data_url(mime: &str, encoded: &str, openai: bool) -> Result<String> {
    let url = format!("data:{mime};base64,{encoded}");
    if openai && url.len() > MAX_OPENAI_DATA_URL_BYTES {
        return Err(Error::tool(
            NAME,
            "OpenAI image references must fit in a 20 MiB encoded data URL; resize the input",
        ));
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::super::super::transport::tests::peer;
    use super::*;
    use crate::media_tools::GenerateImageTool;
    use crate::tools::Tool;

    const OUTPUT: &str = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAFklEQVR4nGP8z8DAwMDAxMDAwMDAAAANHQEDasKb6QAAAABJRU5ErkJggg==";

    #[test]
    fn native_openai_and_xai_edits_upload_references_in_order_and_preserve_inputs() {
        for provider in ["openai", "xai"] {
            let response = json!({"data":[{"b64_json":OUTPUT}]});
            let (endpoint, worker) = peer(
                200,
                "application/json",
                serde_json::to_vec(&response).unwrap(),
            );
            let dir = tempfile::tempdir().unwrap();
            let first = super::super::super::MIN_VALID_PNG;
            let second = base64::engine::general_purpose::STANDARD
                .decode(OUTPUT)
                .unwrap();
            std::fs::write(dir.path().join("first.png"), first).unwrap();
            std::fs::write(dir.path().join("second.png"), &second).unwrap();
            let mut args = json!({"prompt":"Combine these images","image_paths":["first.png","second.png"],"output_path":"edited.png"});
            if provider == "openai" {
                args["mask_path"] = json!("first.png");
                args["input_fidelity"] = json!("high");
            }
            let tool = GenerateImageTool::with_provider(dir.path(), Some(provider.into()))
                .with_mock(false)
                .with_api_key(Some("edit-test-key".into()))
                .with_base_url(endpoint);
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .unwrap();
            let output = runtime.block_on(tool.execute("edit", args, None)).unwrap();
            assert_eq!(output.details.as_ref().unwrap()["reference_count"], 2);
            assert_eq!(output.details.as_ref().unwrap()["edited"], true);
            assert_eq!(
                std::fs::read(dir.path().join("edited.png")).unwrap(),
                second
            );
            assert_eq!(std::fs::read(dir.path().join("first.png")).unwrap(), first);
            assert_eq!(
                std::fs::read(dir.path().join("second.png")).unwrap(),
                second
            );
            let request = worker.join().unwrap();
            assert!(request.headers.starts_with("POST /v1/images/edits "));
            let images = request.body["images"].as_array().unwrap();
            assert_eq!(images.len(), 2);
            let field = if provider == "openai" {
                "image_url"
            } else {
                "url"
            };
            let encoded = images[0][field]
                .as_str()
                .unwrap()
                .strip_prefix("data:image/png;base64,")
                .unwrap();
            assert_eq!(
                base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .unwrap(),
                first
            );
            assert_eq!(images[1][field], format!("data:image/png;base64,{OUTPUT}"));
            if provider == "openai" {
                assert_eq!(request.body["input_fidelity"], "high");
                assert_eq!(request.body["mask"]["image_url"], images[0]["image_url"]);
                assert_eq!(request.body["size"], "auto");
            } else {
                assert!(request.body.get("aspect_ratio").is_none());
                assert_eq!(images[0]["type"], "image_url");
            }
        }
    }

    #[test]
    fn ambiguous_inputs_and_unsupported_masks_fail_before_file_io() {
        let mut endpoint = String::new();
        let mut body = json!({});
        for args in [
            json!({"image_path":"first.png","image_paths":["second.png"]}),
            json!({"image_paths":[]}),
            json!({"image_paths":[1]}),
            json!({"image_paths":["1","2","3","4","5","6"]}),
            json!({"mask_path":"mask.png"}),
            json!({"input_fidelity":"high"}),
        ] {
            assert!(
                attach(
                    Path::new("missing-directory"),
                    &crate::workspace::WorkspaceHandle::default(),
                    "openai",
                    "gpt-image-1.5",
                    &args,
                    &mut endpoint,
                    &mut body
                )
                .is_err()
            );
        }
        assert!(
            attach(
                Path::new("missing-directory"),
                &crate::workspace::WorkspaceHandle::default(),
                "xai",
                "grok-imagine-image-2.0",
                &json!({"image_path":"first.png","mask_path":"mask.png"}),
                &mut endpoint,
                &mut body
            )
            .is_err()
        );
        assert!(
            attach(
                Path::new("missing-directory"),
                &crate::workspace::WorkspaceHandle::default(),
                "openai",
                "dall-e-3",
                &json!({"image_path":"first.png"}),
                &mut endpoint,
                &mut body
            )
            .is_err()
        );
    }

    #[test]
    fn references_and_masks_follow_live_workspace_roots() {
        let root = tempfile::tempdir().unwrap();
        let extra = tempfile::tempdir().unwrap();
        let source = extra.path().join("source.png");
        std::fs::write(&source, super::super::super::MIN_VALID_PNG).unwrap();
        std::fs::write(
            root.path().join("local.png"),
            super::super::super::MIN_VALID_PNG,
        )
        .unwrap();
        let mut workspace = crate::workspace::WorkspaceHandle::single(root.path());
        let bound = workspace.clone();
        let requests = [
            json!({"image_path":source}),
            json!({"image_paths":["local.png",source]}),
            json!({"image_path":"local.png","mask_path":source}),
        ];
        for admitted in [false, true, false] {
            if admitted {
                workspace.add_root(extra.path());
            } else {
                workspace.remove_root(extra.path());
            }
            for args in &requests {
                let mut endpoint = "images/generations".to_string();
                let mut body = json!({});
                let result = attach(
                    root.path(),
                    &bound,
                    "openai",
                    "gpt-image-1.5",
                    args,
                    &mut endpoint,
                    &mut body,
                );
                if admitted {
                    assert!(result.is_ok(), "{result:?}");
                    assert_eq!(endpoint, "images/edits");
                    let expected = format!(
                        "data:image/png;base64,{}",
                        base64::engine::general_purpose::STANDARD
                            .encode(super::super::super::MIN_VALID_PNG)
                    );
                    assert_eq!(body["images"][0]["image_url"], expected);
                    if args.get("mask_path").is_some() {
                        assert_eq!(body["mask"]["image_url"], expected);
                    }
                } else {
                    let error = result.expect_err("unattached source or mask must be refused");
                    assert!(error.to_string().contains("Cannot read outside"), "{error}");
                }
            }
        }
    }
}
