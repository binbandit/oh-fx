use ofx_contract::{ToolImage, ToolImages};
use serde::Serialize;

use super::durable::durable_text;
use crate::image_data::{SavedImagesError, parse_saved_tool_images};
use crate::json_fields::{Fields, Json};

const IMAGE: &str = "image";

#[derive(Serialize)]
pub(crate) struct ToolImageWire<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(rename = "mimeType")]
    mime_type: &'a str,
    data: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_ref: Option<&'a str>,
}

impl<'a> From<&'a ToolImage> for ToolImageWire<'a> {
    fn from(image: &'a ToolImage) -> Self {
        Self {
            kind: IMAGE,
            mime_type: &image.mime_type,
            data: &image.data,
            source_ref: image.source_ref.as_deref(),
        }
    }
}

pub(super) fn read(fields: &mut Fields<'_>, output: &mut String) -> Option<ToolImages> {
    match (
        fields.required("tool_image_handle"),
        fields.required("tool_images"),
    ) {
        (None | Some(Json::Null), None) => Some(ToolImages::None),
        (Some(handle), None) => durable_text(handle).map(ToolImages::Stored),
        (None, Some(Json::Array(items))) => inline(&items, output),
        (_, Some(_)) => None,
    }
}

fn inline(items: &[Json<'_>], output: &mut String) -> Option<ToolImages> {
    let unavailable = match parse_saved_tool_images(items) {
        Err(SavedImagesError::InvalidSourceRef) => return None,
        Err(SavedImagesError::Image(error)) => error.name(),
        Ok(images) if images.len() == items.len() => {
            return Some(if images.is_empty() {
                ToolImages::None
            } else {
                ToolImages::Inline(images)
            });
        }
        Ok(_) => "unsupported content",
    };
    output.push_str("\n[Saved tool image unavailable: ");
    output.push_str(unavailable);
    output.push(']');
    Some(ToolImages::None)
}
