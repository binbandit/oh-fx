use std::path::PathBuf;

use crate::types::ImageAttachment;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TargetKind {
    File,
    Directory,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ApplicableTarget {
    pub path: PathBuf,
    pub kind: TargetKind,
}

pub fn applicable_targets_for_images(images: &[ImageAttachment]) -> Vec<ApplicableTarget> {
    images
        .iter()
        .map(|image| ApplicableTarget {
            path: PathBuf::from(&image.path),
            kind: TargetKind::File,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_attachments_project_to_file_context_targets() {
        let image = |id: u64, path: &str, media_type: &str| ImageAttachment {
            id,
            path: path.to_owned(),
            media_type: media_type.to_owned(),
            ..ImageAttachment::default()
        };
        let images = [
            image(1, "/workspace/a.png", "image/png"),
            image(2, "/workspace/b.jpg", "image/jpeg"),
        ];

        assert_eq!(
            applicable_targets_for_images(&images),
            [
                ApplicableTarget {
                    path: PathBuf::from("/workspace/a.png"),
                    kind: TargetKind::File,
                },
                ApplicableTarget {
                    path: PathBuf::from("/workspace/b.jpg"),
                    kind: TargetKind::File,
                },
            ]
        );
        assert!(applicable_targets_for_images(&[]).is_empty());
    }
}
