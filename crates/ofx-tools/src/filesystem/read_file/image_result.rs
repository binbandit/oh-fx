use std::io::Write;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ofx_contract::ToolImage;
use ofx_images::{
    MAX_ENCODED_IMAGE_BYTES, MAX_SINGLE_IMAGE_DIMENSION, detect_media_type, image_dimensions,
};
use ofx_text::sanitize_model_text_owned;

#[derive(Debug)]
pub(super) struct ImageRead {
    pub(super) text: String,
    pub(super) covered: bool,
    pub(super) images: Vec<ToolImage>,
}

pub(super) fn image_tool_result(
    display_path: &[u8],
    bytes: &[u8],
    file_size: u64,
    incomplete_read: bool,
) -> Option<ImageRead> {
    let mime_type = detect_media_type(bytes)?;
    let dimensions = image_dimensions(bytes);
    let encoded_len = bytes.len().div_ceil(3).saturating_mul(4);
    let mut text = b"<path>".to_vec();
    text.extend_from_slice(display_path);
    let reason = if incomplete_read {
        Some("the file exceeds read_file's 10 MiB read limit")
    } else if let Some(dimensions) = dimensions {
        if encoded_len > MAX_ENCODED_IMAGE_BYTES {
            Some("the image exceeds the 5 MiB encoded attach limit")
        } else if dimensions.exceeds(MAX_SINGLE_IMAGE_DIMENSION) {
            Some("the image exceeds 8000 pixels per side")
        } else {
            None
        }
    } else {
        Some("the image dimensions could not be verified")
    };
    if let Some(reason) = reason {
        let _ = write!(
            text,
            "</path>\n<content>image not attached: {mime_type} ({file_size} bytes); {reason}. Use an available image tool to save a smaller copy to a new file, then read_file the copy. If no image tool is available, ask the user before installing one.</content>"
        );
        return Some(ImageRead {
            text: sanitize_model_text_owned(text),
            covered: false,
            images: Vec::new(),
        });
    }
    let _ = write!(
        text,
        "</path>\n<content>image attached ({mime_type}, {file_size} bytes)</content>"
    );
    Some(ImageRead {
        text: sanitize_model_text_owned(text),
        covered: true,
        images: vec![ToolImage {
            data: STANDARD.encode(bytes),
            mime_type: mime_type.to_owned(),
            source_ref: None,
        }],
    })
}

#[cfg(test)]
mod tests;
