//! Invocation-owned image evidence for judgments and one-shot completions.
//!
//! Paths always enter the current Agent's enabled read tool and its policy.
//! Only validated native image blocks cross into the model request; filenames,
//! read notes and encoded image bytes never enter the text privacy projection.

use super::{AgentCx, Error, Invocation, JudgmentRequest, MAX_TOOL_CALLS, Result};
use crate::judgment::{
    MAX_JUDGMENT_IMAGE_BYTES, MAX_JUDGMENT_IMAGE_ENCODED_BYTES, MAX_JUDGMENT_IMAGES, validate_image,
};
use crate::model::{ContentBlock, ImageContent};
use serde_json::{Value, json};

pub(super) const MAX_BATCH_IMAGE_BYTES: usize = 16 * 1024 * 1024;
const MAX_PATH_BYTES: usize = 4096;

fn invalid() -> Error {
    Error::tool(
        "judge",
        "PI_JUDGE_IMAGE_INPUT: images must be an array of local paths or {type:'image',data,mimeType}; only omitted or auto detail is supported",
    )
}

pub(super) fn limit() -> Error {
    Error::tool(
        "judge",
        "PI_JUDGE_IMAGE_LIMIT: image evidence exceeds its count or byte budget",
    )
}

fn checkpoint() -> Result<()> {
    AgentCx::for_current_or_request()
        .checkpoint()
        .map_err(|_| Error::tool("judge", "PI_JUDGE_CANCELLED: image preparation stopped"))
}

enum Source {
    Path(String),
    Inline(ImageContent),
}

#[derive(Default)]
pub(super) struct Sources {
    items: Vec<Source>,
    inline_bytes: usize,
}

impl Sources {
    /// Validate every descriptor before cloning bounded payloads or entering
    /// any host. Empty lists are ordinary text-only judgments.
    pub(super) fn parse(value: Option<&Value>) -> Result<Self> {
        let Some(value) = value else {
            return Ok(Self::default());
        };
        let values = value.as_array().ok_or_else(invalid)?;
        if values.len() > MAX_JUDGMENT_IMAGES {
            return Err(limit());
        }
        let mut sources = Self::default();
        for value in values {
            let source = if let Some(path) = value.as_str() {
                if path.trim().is_empty()
                    || path.len() > MAX_PATH_BYTES
                    || path.chars().any(char::is_control)
                    || path.contains("://")
                    || path.starts_with("data:")
                {
                    return Err(invalid());
                }
                Source::Path(path.to_owned())
            } else {
                let fields = value.as_object().ok_or_else(invalid)?;
                if fields
                    .keys()
                    .any(|key| !matches!(key.as_str(), "type" | "data" | "mimeType" | "detail"))
                    || fields.get("type").and_then(Value::as_str) != Some("image")
                    || fields
                        .get("detail")
                        .is_some_and(|detail| detail.as_str() != Some("auto"))
                {
                    return Err(invalid());
                }
                let data = fields
                    .get("data")
                    .and_then(Value::as_str)
                    .ok_or_else(invalid)?;
                let mime = fields
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .ok_or_else(invalid)?;
                // Refuse oversized input before copying; validate the exact
                // MIME before any serde normalizer can alter supplied data.
                if data.len() > MAX_JUDGMENT_IMAGE_BYTES / 3 * 4
                    || data.len()
                        > MAX_JUDGMENT_IMAGE_ENCODED_BYTES.saturating_sub(sources.inline_bytes)
                {
                    return Err(limit());
                }
                if !matches!(
                    mime,
                    "image/png" | "image/jpeg" | "image/gif" | "image/webp"
                ) {
                    return Err(invalid());
                }
                let image = ImageContent {
                    data: data.to_owned(),
                    mime_type: mime.to_owned(),
                };
                sources.inline_bytes += validate_image(&image)?;
                Source::Inline(image)
            };
            sources.items.push(source);
        }
        Ok(sources)
    }

    pub(super) const fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub(super) fn path_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| matches!(item, Source::Path(_)))
            .count()
    }

    pub(super) const fn inline_bytes(&self) -> usize {
        self.inline_bytes
    }
}

pub(super) fn parse_request(input: Value) -> Result<(JudgmentRequest, Sources)> {
    let Value::Object(mut fields) = input else {
        return Err(invalid());
    };
    let image_input = fields.remove("images");
    let request = JudgmentRequest::parse(Value::Object(fields))?;
    let images = Sources::parse(image_input.as_ref())?;
    Ok((request, images))
}

impl Invocation<'_> {
    pub(super) fn check_image_reads(&self, count: usize) -> Result<()> {
        checkpoint()?;
        if count == 0 {
            return Ok(());
        }
        if self.host.is_none() || self.host_session.is_none() {
            return Err(Error::tool(
                "judge",
                "PI_JUDGE_IMAGE_HOST: local image paths require this cell's Agent tool host; supply an embedded image for a standalone Judge",
            ));
        }
        if count > MAX_TOOL_CALLS.saturating_sub(self.tool_receipts.len()) {
            return Err(Error::tool(
                "judge",
                "PI_JUDGE_IMAGE_CALL_LIMIT: image reads exceed the remaining per-cell host-call allowance",
            ));
        }
        Ok(())
    }

    pub(super) async fn attach_images(
        &mut self,
        request: JudgmentRequest,
        sources: Sources,
    ) -> Result<JudgmentRequest> {
        request.with_images(self.load_images(sources).await?)
    }

    /// Shared acquisition path. Callers preflight the model capability and
    /// complete read count before entering it; every read keeps host policy,
    /// the original owner/deadline and native tool receipts.
    pub(super) async fn load_images(&mut self, sources: Sources) -> Result<Vec<ImageContent>> {
        let mut images = Vec::with_capacity(sources.items.len());
        let mut bytes = 0_usize;
        for source in sources.items {
            checkpoint()?;
            let image = match source {
                Source::Inline(image) => image,
                Source::Path(path) => self.read_judgment_image(path).await?,
            };
            bytes = bytes
                .checked_add(image.data.len())
                .filter(|bytes| *bytes <= MAX_JUDGMENT_IMAGE_ENCODED_BYTES)
                .ok_or_else(limit)?;
            images.push(image);
        }
        checkpoint()?;
        Ok(images)
    }

    async fn read_judgment_image(&mut self, path: String) -> Result<ImageContent> {
        let host = self.host.ok_or_else(|| {
            Error::tool(
                "judge",
                "PI_JUDGE_IMAGE_HOST: the current Agent tool host is unavailable",
            )
        })?;
        let read_error = || {
            Error::tool(
                "judge",
                "PI_JUDGE_IMAGE_READ: the enabled read tool must permit the path and return exactly one native image",
            )
        };
        let output = self
            .call_host_output(host, "read".to_owned(), json!({"path":path}))
            .await
            .map_err(|_| read_error())?;
        checkpoint()?;
        if output.is_error {
            return Err(read_error());
        }
        let mut image = None;
        for block in output.content {
            match block {
                ContentBlock::Image(candidate) => {
                    if image.replace(candidate).is_some() {
                        return Err(read_error());
                    }
                }
                ContentBlock::Text(_) => {}
                _ => return Err(read_error()),
            }
        }
        let image = image.ok_or_else(read_error)?;
        validate_image(&image)?;
        Ok(image)
    }
}

#[cfg(test)]
mod tests;
