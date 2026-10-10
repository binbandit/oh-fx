use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ofx_contract::ToolImage;
use serde_json::{Map, Value};

use crate::image_data::{MAX_ENCODED_IMAGE_BYTES, detect_media_type, supported_media_type};

pub const MAX_RESULT_FRAME_BYTES: usize = 8 * 1024 * 1024;
const MAX_TOOL_IMAGES: usize = 8;
const MAX_SOURCE_REF_BYTES: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ImageError {
    #[error("InvalidImage")]
    InvalidImage,
    #[error("ImageLimitExceeded")]
    ImageLimitExceeded,
    #[error("UnsupportedImageType")]
    UnsupportedImageType,
}

fn valid_source_ref(source_ref: &str) -> bool {
    (1..=MAX_SOURCE_REF_BYTES).contains(&source_ref.len())
        && !source_ref.bytes().any(|byte| byte < 0x20 || byte == 0x7f)
}

#[derive(Debug, Default)]
pub struct ImageList {
    items: Vec<ToolImage>,
    encoded_bytes: usize,
}

impl ImageList {
    pub fn append(&mut self, data: &str, mime_type: &str) -> Result<(), ImageError> {
        self.append_with_source_ref(data, mime_type, None)
    }

    fn append_with_source_ref(
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
        if source_ref.is_some_and(|value| !valid_source_ref(value)) {
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

    pub fn into_images(self) -> Vec<ToolImage> {
        self.items
    }
}

pub fn parse_tool_images(content: &[Value]) -> Result<Vec<ToolImage>, ImageError> {
    let mut images = ImageList::default();
    for item in content {
        let Some(item) = item.as_object() else {
            continue;
        };
        match item.get("type").and_then(Value::as_str) {
            Some("image") => append_image_block(&mut images, item)?,
            Some("resource") => {
                if let Some(resource) = item.get("resource").and_then(Value::as_object) {
                    append_embedded_resource(&mut images, resource)?;
                }
            }
            _ => {}
        }
    }
    Ok(images.into_images())
}

fn append_image_block(
    images: &mut ImageList,
    block: &Map<String, Value>,
) -> Result<(), ImageError> {
    let source_ref = match block.get("sourceRef") {
        None => None,
        Some(Value::String(value)) if valid_source_ref(value) => Some(value.as_str()),
        Some(_) => return Err(ImageError::InvalidImage),
    };
    let mime_type = block.get("mimeType");
    let Some(data) = block.get("data") else {
        let Some(source_ref) = source_ref else {
            return Err(ImageError::InvalidImage);
        };
        let mime_type = mime_type
            .and_then(Value::as_str)
            .filter(|mime_type| supported_media_type(mime_type))
            .ok_or(ImageError::InvalidImage)?;
        return images.append_with_source_ref("", mime_type, Some(source_ref));
    };
    let (Some(data), Some(mime_type)) = (
        data.as_str(),
        mime_type.ok_or(ImageError::InvalidImage)?.as_str(),
    ) else {
        return Err(ImageError::InvalidImage);
    };
    if supported_media_type(mime_type) {
        images.append_with_source_ref(data, mime_type, source_ref)
    } else {
        Ok(())
    }
}

fn append_embedded_resource(
    images: &mut ImageList,
    resource: &Map<String, Value>,
) -> Result<(), ImageError> {
    let (Some(data), Some(mime_type)) = (resource.get("blob"), resource.get("mimeType")) else {
        return Ok(());
    };
    let (Some(data), Some(mime_type)) = (data.as_str(), mime_type.as_str()) else {
        return Err(ImageError::InvalidImage);
    };
    if supported_media_type(mime_type) {
        images.append(data, mime_type)
    } else {
        Ok(())
    }
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

#[cfg(test)]
mod tests;
