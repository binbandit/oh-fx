use serde::Serialize;

use super::{MAX_IDENTITY_BYTES, is_valid_path};

const MAX_IMAGES: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(serde::Deserialize), serde(deny_unknown_fields))]
pub(crate) struct ImageAttachment {
    #[cfg_attr(test, serde(default))]
    pub(crate) id: u64,
    pub(crate) path: String,
    pub(crate) media_type: String,
    #[cfg_attr(test, serde(default))]
    pub(crate) snapshot_path: Option<String>,
    #[cfg_attr(test, serde(default))]
    pub(crate) snapshot_sha256: Option<String>,
    #[cfg_attr(test, serde(default))]
    pub(crate) inline_data: Option<String>,
    #[cfg_attr(test, serde(default))]
    pub(crate) source_ref: Option<String>,
}

pub(crate) fn are_valid_images(images: &[ImageAttachment]) -> bool {
    images.len() <= MAX_IMAGES
        && images.iter().all(|image| {
            is_valid_path(&image.path) && (1..=MAX_IDENTITY_BYTES).contains(&image.media_type.len())
        })
}
