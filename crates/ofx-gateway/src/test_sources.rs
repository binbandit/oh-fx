use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use ofx_contract::{ChatMessage, ImageAttachment};

use crate::chat_completions::ChunkSource;

pub(crate) struct Paced(VecDeque<(Duration, Vec<u8>)>);

impl Paced {
    pub(crate) fn new<const N: usize>(chunks: [(Duration, &str); N]) -> Self {
        Self(
            chunks
                .into_iter()
                .map(|(delay, chunk)| (delay, chunk.as_bytes().to_vec()))
                .collect(),
        )
    }
}

impl ChunkSource for Paced {
    async fn next_chunk(&mut self) -> Result<Option<impl AsRef<[u8]> + Send>, String> {
        let Some((delay, chunk)) = self.0.pop_front() else {
            return std::future::pending().await;
        };
        tokio::time::sleep(delay).await;
        Ok(Some(chunk))
    }
}

pub(crate) struct CapturedImages {
    _directory: tempfile::TempDir,
    root: PathBuf,
}

impl CapturedImages {
    pub(crate) fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        Self {
            _directory: directory,
            root,
        }
    }

    pub(crate) fn capture(&self, id: u64, name: &str, bytes: &[u8]) -> ImageAttachment {
        let source = self.root.join(name);
        fs::write(&source, bytes).unwrap();
        let mut images = [ImageAttachment {
            id,
            path: source.into_os_string().into_string().unwrap(),
            media_type: "image/png".to_owned(),
            ..ImageAttachment::default()
        }];
        let snapshots = self.root.join("snapshots");
        ofx_images::capture_image_snapshots(&mut images, snapshots.to_str().unwrap(), &|| Ok(()))
            .unwrap();
        let [image] = images;
        image
    }
}

pub(crate) fn user_with_images(content: &str, images: Vec<ImageAttachment>) -> ChatMessage {
    ChatMessage::User {
        content: content.to_owned(),
        restored_steering: false,
        feedback_for: None,
        images,
    }
}
