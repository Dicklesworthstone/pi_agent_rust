//! Image evidence stays outside text serialization and privacy projection.
//!
//! Admission checks canonical base64, exact supported MIME metadata and the
//! existing bounded image envelope. Pixels are not decoded, inspected for
//! secrets or transformed; successful admission preserves the supplied bytes.

use crate::error::Result;
use crate::model::ImageContent;

use super::failure;

pub(crate) const MAX_JUDGMENT_IMAGES: usize = 4;
pub(crate) const MAX_JUDGMENT_IMAGE_BYTES: usize = 4_718_592;
pub(crate) const MAX_JUDGMENT_IMAGE_ENCODED_BYTES: usize = 8 * 1024 * 1024;
// The decoded allowance is divisible by three, so this exact encoded ceiling
// also bounds every canonical padded base64 payload to the decoded allowance.
const MAX_ENCODED_IMAGE_BYTES: usize = MAX_JUDGMENT_IMAGE_BYTES / 3 * 4;

fn invalid() -> crate::error::Error {
    failure(
        "PI_JUDGE_IMAGE_INPUT: expected canonical base64 PNG, JPEG, GIF or WebP with a bounded image envelope",
    )
}

fn limit() -> crate::error::Error {
    failure(
        "PI_JUDGE_IMAGE_LIMIT: at most 4 images, 4718592 decoded bytes per image and 8 MiB encoded bytes per request",
    )
}

/// Validate one image before it is retained or sent to a provider. The returned
/// size is its encoded payload length, for the bridge's aggregate batch budget.
/// Native image metadata must match exactly; no MIME sanitization or format
/// coercion is applied at this boundary. The shared decoder rejects whitespace,
/// nonstandard alphabets, missing canonical padding and nonzero trailing bits.
pub(crate) fn validate_image(image: &ImageContent) -> Result<usize> {
    if !matches!(
        image.mime_type.as_str(),
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    ) || image.data.is_empty()
    {
        return Err(invalid());
    }
    if image.data.len() > MAX_ENCODED_IMAGE_BYTES {
        return Err(limit());
    }
    crate::eval::image_dimensions(&image.data, &image.mime_type).map_err(|_| invalid())?;
    Ok(image.data.len())
}

pub(super) fn validate_images(images: &[ImageContent]) -> Result<()> {
    if images.len() > MAX_JUDGMENT_IMAGES {
        return Err(limit());
    }
    // Bound the whole collection before decoding any member. Callers cannot
    // create an oversized request from individually admissible attachments.
    let total = images
        .iter()
        .try_fold(0_usize, |total, image| total.checked_add(image.data.len()));
    if total.is_none_or(|total| total > MAX_JUDGMENT_IMAGE_ENCODED_BYTES) {
        return Err(limit());
    }
    for image in images {
        validate_image(image)?;
    }
    Ok(())
}
