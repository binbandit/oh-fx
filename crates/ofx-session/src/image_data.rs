use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ofx_contract::{Json, ToolImage};

pub(crate) const MAX_RESULT_FRAME_BYTES: usize = 8 * 1024 * 1024;
const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;
const MAX_ENCODED_IMAGE_BYTES: usize = 5 * 1024 * 1024;
const MAX_TOOL_IMAGES: usize = 8;
const MAX_SOURCE_REF_BYTES: usize = 512;
const SUPPORTED_MEDIA_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImageError {
    InvalidImage,
    ImageLimitExceeded,
    UnsupportedImageType,
}

impl ImageError {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::InvalidImage => "InvalidImage",
            Self::ImageLimitExceeded => "ImageLimitExceeded",
            Self::UnsupportedImageType => "UnsupportedImageType",
        }
    }
}

pub(crate) fn valid_source_ref(source_ref: &str) -> bool {
    (1..=MAX_SOURCE_REF_BYTES).contains(&source_ref.len())
        && !source_ref.bytes().any(|byte| byte < 0x20 || byte == 0x7f)
}

pub(crate) fn is_valid_inline_image(bytes: &[u8], media_type: &str) -> bool {
    (1..=MAX_IMAGE_BYTES).contains(&bytes.len()) && detect_media_type(bytes) == Some(media_type)
}

fn detect_media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn supported_media_type(mime_type: &str) -> bool {
    SUPPORTED_MEDIA_TYPES.contains(&mime_type)
}

fn validate_image(encoded: &str, mime_type: &str) -> Result<(), ImageError> {
    if encoded.is_empty() || encoded.len() > MAX_ENCODED_IMAGE_BYTES {
        return Err(ImageError::ImageLimitExceeded);
    }
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| ImageError::InvalidImage)?;
    let detected = detect_media_type(&bytes).ok_or(ImageError::UnsupportedImageType)?;
    if detected == mime_type {
        Ok(())
    } else {
        Err(ImageError::InvalidImage)
    }
}

#[derive(Default)]
struct ImageList {
    items: Vec<ToolImage>,
    encoded_bytes: usize,
}

impl ImageList {
    fn append(
        &mut self,
        data: &str,
        mime_type: &str,
        source_ref: Option<&str>,
    ) -> Result<(), ImageError> {
        if self.items.len() >= MAX_TOOL_IMAGES
            || data.len() > MAX_RESULT_FRAME_BYTES.saturating_sub(self.encoded_bytes)
        {
            return Err(ImageError::ImageLimitExceeded);
        }
        if source_ref.is_some_and(|source_ref| !valid_source_ref(source_ref)) {
            return Err(ImageError::InvalidImage);
        }
        if data.is_empty() {
            if source_ref.is_none() {
                return Err(ImageError::ImageLimitExceeded);
            }
            if !supported_media_type(mime_type) {
                return Err(ImageError::UnsupportedImageType);
            }
        } else {
            validate_image(data, mime_type)?;
        }
        self.items.push(ToolImage {
            data: data.to_owned(),
            mime_type: mime_type.to_owned(),
            source_ref: source_ref.map(str::to_owned),
        });
        self.encoded_bytes += data.len();
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SavedImagesError {
    InvalidSourceRef,
    Image(ImageError),
}

pub(crate) fn parse_saved_tool_images(
    content: &[Json<'_>],
) -> Result<Vec<ToolImage>, SavedImagesError> {
    if content.len() > MAX_TOOL_IMAGES {
        return Err(SavedImagesError::Image(ImageError::ImageLimitExceeded));
    }
    for item in content {
        let object = item
            .as_object()
            .ok_or(SavedImagesError::Image(ImageError::InvalidImage))?;
        let valid_reference = object
            .get("source_ref")
            .is_none_or(|value| value.as_str().is_some_and(valid_source_ref));
        if object.contains_key("sourceRef") || !valid_reference {
            return Err(SavedImagesError::InvalidSourceRef);
        }
    }
    parse_tool_images(content).map_err(SavedImagesError::Image)
}

fn parse_tool_images(content: &[Json<'_>]) -> Result<Vec<ToolImage>, ImageError> {
    let mut images = ImageList::default();
    for item in content {
        let Some(kind) = item.get("type").and_then(Json::as_str) else {
            continue;
        };
        let embedded = kind == "resource";
        if !embedded && kind != "image" {
            continue;
        }
        let Some(block) = (if embedded {
            item.get("resource")
        } else {
            Some(item)
        })
        .filter(|block| block.as_object().is_some()) else {
            continue;
        };
        let source_ref = if embedded {
            None
        } else {
            block.get("source_ref").and_then(Json::as_str)
        };
        let Some(data) = block.get(if embedded { "blob" } else { "data" }) else {
            if embedded {
                continue;
            }
            let source_ref = source_ref.ok_or(ImageError::InvalidImage)?;
            let mime_type = block
                .get("mimeType")
                .and_then(Json::as_str)
                .filter(|mime_type| supported_media_type(mime_type))
                .ok_or(ImageError::InvalidImage)?;
            images.append("", mime_type, Some(source_ref))?;
            continue;
        };
        let Some(mime_type) = block.get("mimeType") else {
            if embedded {
                continue;
            }
            return Err(ImageError::InvalidImage);
        };
        let (Some(data), Some(mime_type)) = (data.as_str(), mime_type.as_str()) else {
            return Err(ImageError::InvalidImage);
        };
        if supported_media_type(mime_type) {
            images.append(data, mime_type, source_ref)?;
        }
    }
    Ok(images.items)
}

#[cfg(test)]
mod tests;
