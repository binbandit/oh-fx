use ofx_contract::ImageAttachment;
use serde::Serialize;

use super::durable::{DurableBytes, durable_bytes, durable_text};
use crate::image_data::{is_valid_inline_image, valid_source_ref};
use crate::json_fields::{Fields, Json};
use crate::session_event::snapshot_locator;

#[derive(Serialize)]
pub(super) struct ImageWire<'a> {
    id: u64,
    path: &'a str,
    media_type: &'a str,
    snapshot_path: Option<String>,
    snapshot_sha256: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    inline_data: Option<DurableBytes<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_ref: Option<&'a str>,
}

impl<'a> From<&'a ImageAttachment> for ImageWire<'a> {
    fn from(image: &'a ImageAttachment) -> Self {
        Self {
            id: image.id,
            path: &image.path,
            media_type: &image.media_type,
            snapshot_path: image.snapshot_path.as_deref().map(snapshot_locator),
            snapshot_sha256: image.snapshot_sha256.as_deref(),
            inline_data: image.inline_data.as_deref().map(DurableBytes),
            source_ref: image.source_ref.as_deref(),
        }
    }
}

pub(super) fn read(value: Json<'_>) -> Option<Vec<ImageAttachment>> {
    let Json::Array(items) = value else {
        return None;
    };
    items.into_iter().map(image).collect()
}

fn image(value: Json<'_>) -> Option<ImageAttachment> {
    let mut fields = Fields::new(value)?;
    let image = ImageAttachment {
        id: fields.unsigned("id")?,
        path: durable_text(fields.required("path")?)?,
        media_type: durable_text(fields.required("media_type")?)?,
        snapshot_path: fields.nullable("snapshot_path", |value| durable_text(value).map(Some))?,
        snapshot_sha256: fields
            .nullable("snapshot_sha256", |value| durable_text(value).map(Some))?,
        inline_data: fields.nullable("inline_data", |value| durable_bytes(value).map(Some))?,
        source_ref: fields.or("source_ref", None, |value| {
            value
                .as_str()
                .filter(|source_ref| valid_source_ref(source_ref))
                .map(|source_ref| Some(source_ref.to_owned()))
        })?,
    };
    let sources_agree = match &image.inline_data {
        Some(bytes) => {
            image.snapshot_path.is_none()
                && image.snapshot_sha256.is_some()
                && is_valid_inline_image(bytes, &image.media_type)
        }
        None => image.snapshot_path.is_some() == image.snapshot_sha256.is_some(),
    };
    sources_agree.then_some(())?;
    fields.finish(image)
}
