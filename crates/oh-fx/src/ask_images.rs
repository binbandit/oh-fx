use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::{env, fmt, fs};

use ofx_contract::{ApplicableTarget, ImageAttachment, applicable_targets_for_images};
use ofx_images::{
    AttachmentError, IMAGE_TOO_LARGE_NOTICE, TempSnapshotDir, capture_image_snapshots,
    load_resolved_image_attachment, normalize_path_input,
};
use ofx_workspace::{PATH_ENTRY_WHITESPACE, PathError, resolve_workspace_or_external_literal_path};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImageLoadError {
    Path(PathError),
    Image(AttachmentError),
}

impl fmt::Display for ImageLoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Path(error) => error.fmt(formatter),
            Self::Image(error) => error.fmt(formatter),
        }
    }
}

impl ImageLoadError {
    pub(crate) fn reason(self) -> String {
        match self {
            Self::Path(PathError::FileNotFound) | Self::Image(AttachmentError::FileNotFound) => {
                "image file not found".to_owned()
            }
            Self::Image(AttachmentError::UnsupportedImageType) => {
                "unsupported image type".to_owned()
            }
            Self::Image(AttachmentError::ImageTooLarge) => IMAGE_TOO_LARGE_NOTICE.to_owned(),
            other => other.to_string(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ImageFailure<'a> {
    pub(crate) path: &'a OsStr,
    pub(crate) error: ImageLoadError,
}

pub(crate) fn load_image_paths(
    paths: &[OsString],
) -> Result<Vec<ImageAttachment>, ImageFailure<'_>> {
    let Some(first) = paths.first() else {
        return Ok(Vec::new());
    };
    let workspace_root =
        env::current_dir()
            .and_then(fs::canonicalize)
            .map_err(|_| ImageFailure {
                path: first,
                error: ImageLoadError::Path(PathError::WorkspaceUnavailable),
            })?;
    paths
        .iter()
        .map(|path| {
            load_user_image_attachment(&workspace_root, path)
                .map_err(|error| ImageFailure { path, error })
        })
        .collect()
}

fn load_user_image_attachment(
    workspace_root: &Path,
    path_input: &OsStr,
) -> Result<ImageAttachment, ImageLoadError> {
    let invalid = ImageLoadError::Path(PathError::InvalidPath);
    let normalized = normalize_path_input(path_input.to_str().ok_or(invalid)?);
    let literal = normalized.trim_matches(PATH_ENTRY_WHITESPACE);
    let resolved = resolve_workspace_or_external_literal_path(workspace_root, literal)
        .map_err(ImageLoadError::Path)?;
    let path = resolved
        .into_os_string()
        .into_string()
        .map_err(|_| invalid)?;
    load_resolved_image_attachment(path).map_err(ImageLoadError::Image)
}

#[derive(Debug, Default)]
pub(crate) struct CapturedImages {
    images: Vec<ImageAttachment>,
    _snapshots: Option<TempSnapshotDir>,
}

impl CapturedImages {
    pub(crate) fn capture(
        mut images: Vec<ImageAttachment>,
        cancel: &CancellationToken,
    ) -> Result<Self, AttachmentError> {
        if images.is_empty() {
            return Ok(Self::default());
        }
        for (image, id) in images.iter_mut().zip(1..) {
            image.id = id;
        }
        let snapshots = TempSnapshotDir::create()?;
        let budget = || {
            if cancel.is_cancelled() {
                Err(AttachmentError::Cancelled)
            } else {
                Ok(())
            }
        };
        capture_image_snapshots(&mut images, snapshots.path(), &budget)?;
        Ok(Self {
            images,
            _snapshots: Some(snapshots),
        })
    }

    pub(crate) fn images(&self) -> &[ImageAttachment] {
        &self.images
    }

    pub(crate) fn context_targets(&self) -> Vec<ApplicableTarget> {
        applicable_targets_for_images(&self.images)
    }
}

#[cfg(test)]
mod tests;
