use serde::Serialize;

use super::{MAX_IDENTITY_BYTES, is_valid_path};

const MAX_IMAGES: usize = 128;
const SNAPSHOT_DIR: &str = "images";

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

impl TryFrom<&ofx_contract::ImageAttachment> for ImageAttachment {
    type Error = std::string::FromUtf8Error;

    fn try_from(image: &ofx_contract::ImageAttachment) -> Result<Self, Self::Error> {
        Ok(Self {
            id: image.id,
            path: image.path.clone(),
            media_type: image.media_type.clone(),
            snapshot_path: image.snapshot_path.as_deref().map(snapshot_locator),
            snapshot_sha256: image.snapshot_sha256.clone(),
            inline_data: image
                .inline_data
                .clone()
                .map(String::from_utf8)
                .transpose()?,
            source_ref: image.source_ref.clone(),
        })
    }
}

pub(crate) fn snapshot_locator(path: &str) -> String {
    if !path.starts_with('/') {
        return path.to_owned();
    }
    let trimmed = path.trim_end_matches('/');
    let name = trimmed
        .rfind('/')
        .map_or(trimmed, |slash| &trimmed[slash + 1..]);
    format!("{SNAPSHOT_DIR}/{name}")
}
