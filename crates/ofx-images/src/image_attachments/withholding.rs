use std::collections::HashMap;
use std::fmt::Write;
use std::fs::File;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::sync::{Mutex, PoisonError};

use ofx_contract::{ChatMessage, DEFAULT_MAX_TOOL_RESULT_BYTES, ImageAttachment, ToolImage};

use super::snapshots::open_snapshot_file_no_follow;
use crate::image_data::{
    Dimensions, MAX_ENCODED_IMAGE_BYTES, count_request_images, encoded_image_dimensions,
    fits_encoded_image_limit, image_dimensions, positional_image_dimensions, request_max_dimension,
    supported_media_type, write_host_image_recovery_notice,
};

#[derive(Debug, Default)]
pub struct AttachmentDimensionCache {
    dimensions: Mutex<HashMap<String, Option<Dimensions>>>,
}

impl AttachmentDimensionCache {
    pub fn withhold_oversized_images(&self, messages: &[ChatMessage]) -> Option<Vec<ChatMessage>> {
        let max_dimension = request_max_dimension(count_request_images(messages));
        let tools = withhold_tool_images(messages, max_dimension, DEFAULT_MAX_TOOL_RESULT_BYTES);
        self.withhold(tools.as_deref().unwrap_or(messages), max_dimension)
            .or(tools)
    }

    fn withhold(&self, messages: &[ChatMessage], max_dimension: u32) -> Option<Vec<ChatMessage>> {
        let mut projected: Option<Vec<ChatMessage>> = None;
        for (index, message) in messages.iter().enumerate() {
            let ChatMessage::User { images, .. } = message else {
                continue;
            };
            let mut kept: Option<Vec<ImageAttachment>> = None;
            let mut notice = String::new();
            for (position, image) in images.iter().enumerate() {
                let dimensions = self.dimensions(image);
                if dimensions.is_some_and(|size| !size.exceeds(max_dimension))
                    && fits_encoded_limit(image)
                {
                    if let Some(kept) = &mut kept {
                        kept.push(image.clone());
                    }
                    continue;
                }
                kept.get_or_insert_with(|| images[..position].to_vec());
                write_withheld_notice(&mut notice, image, dimensions, max_dimension);
            }
            let Some(kept) = kept else {
                continue;
            };
            let messages = projected.get_or_insert_with(|| messages.to_vec());
            if let ChatMessage::User {
                content, images, ..
            } = &mut messages[index]
            {
                *images = kept;
                content.insert_str(0, &notice);
            }
        }
        projected
    }

    fn dimensions(&self, image: &ImageAttachment) -> Option<Dimensions> {
        if let Some(bytes) = &image.inline_data {
            return image_dimensions(bytes);
        }
        let path = image.snapshot_path.as_deref()?;
        let Some(digest) = &image.snapshot_sha256 else {
            return probe_snapshot_dimensions(path);
        };
        let mut cache = self
            .dimensions
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *cache
            .entry(digest.clone())
            .or_insert_with(|| probe_snapshot_dimensions(path))
    }
}

fn withhold_tool_images(
    messages: &[ChatMessage],
    max_dimension: u32,
    text_limit: usize,
) -> Option<Vec<ChatMessage>> {
    let mut projected: Option<Vec<ChatMessage>> = None;
    for (index, message) in messages.iter().enumerate() {
        let ChatMessage::Tool { images, .. } = message else {
            continue;
        };
        if images
            .iter()
            .all(|image| tool_image_fits(image, max_dimension))
        {
            continue;
        }
        let mut kept = Vec::with_capacity(images.len());
        let mut notice = String::new();
        for image in images {
            if tool_image_fits(image, max_dimension) {
                kept.push(image.clone());
            } else {
                write_withheld_tool_image_notice(&mut notice, image, max_dimension);
            }
        }
        let messages = projected.get_or_insert_with(|| messages.to_vec());
        if let ChatMessage::Tool {
            content, images, ..
        } = &mut messages[index]
        {
            *images = kept;
            *content = prepend_image_notice(&notice, content, text_limit);
        }
    }
    projected
}

fn tool_image_fits(image: &ToolImage, max_dimension: u32) -> bool {
    encoded_image_dimensions(&image.data).is_some_and(|size| !size.exceeds(max_dimension))
        && image.data.len() <= MAX_ENCODED_IMAGE_BYTES
        && supported_media_type(&image.mime_type)
}

fn write_withheld_tool_image_notice(out: &mut String, image: &ToolImage, max_dimension: u32) {
    if let Some(size) = encoded_image_dimensions(&image.data) {
        let _ = write!(
            out,
            "[Image not sent: {} is {}x{} pixels; this request permits at most {max_dimension} per side and 5 MiB encoded per image. ",
            image.mime_type, size.width, size.height
        );
    } else if image.source_ref.is_some() && image.data.is_empty() {
        let _ = write!(
            out,
            "[Image not sent: only a host source reference was supplied; this request permits at most {max_dimension} per side and 5 MiB encoded per image. "
        );
    } else {
        out.push_str("[Image not sent: its dimensions could not be verified. ");
    }
    match &image.source_ref {
        Some(source_ref) => write_host_image_recovery_notice(out, source_ref, max_dimension),
        None => out.push_str("If this tool result names a local file, use an available image tool to save a smaller copy, then read_file the copy. If no tool or path is available, ask the user; ask before installing software.]\n"),
    }
}

fn prepend_image_notice(notice: &str, content: &str, limit: usize) -> String {
    let notice = &notice[..notice.floor_char_boundary(limit)];
    let content = &content[..content.floor_char_boundary(limit.saturating_sub(notice.len()))];
    format!("{notice}{content}")
}

fn probe_snapshot_dimensions(path: &str) -> Option<Dimensions> {
    let file = open_snapshot_file_no_follow(path).ok()?;
    positional_image_dimensions(&|offset, buffer| read_at(&file, offset, buffer))
}

fn read_at(file: &File, offset: u64, buffer: &mut [u8]) -> usize {
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read_at(&mut buffer[filled..], offset + filled as u64) {
            Ok(0) | Err(_) => break,
            Ok(count) => filled += count,
        }
    }
    filled
}

fn fits_encoded_limit(image: &ImageAttachment) -> bool {
    if let Some(bytes) = &image.inline_data {
        return fits_encoded_image_limit(bytes.len());
    }
    let Some(file) = image
        .snapshot_path
        .as_deref()
        .and_then(|path| open_snapshot_file_no_follow(path).ok())
    else {
        return false;
    };
    file.metadata().is_ok_and(|metadata| {
        metadata.is_file()
            && metadata.nlink() == 1
            && usize::try_from(metadata.len()).is_ok_and(fits_encoded_image_limit)
    })
}

fn write_withheld_notice(
    out: &mut String,
    image: &ImageAttachment,
    dimensions: Option<Dimensions>,
    max_dimension: u32,
) {
    let id = image.id;
    if let Some(size) = dimensions {
        let _ = write!(
            out,
            "[Image #{id} not sent: {} is {}x{} pixels. This request permits at most {max_dimension} per side and 5 MiB encoded per image. ",
            image.media_type, size.width, size.height
        );
    } else if image.source_ref.is_some()
        && image.inline_data.is_none()
        && image.snapshot_path.is_none()
    {
        let _ = write!(
            out,
            "[Image #{id} not sent: only a host source reference was supplied. This request permits at most {max_dimension} per side and 5 MiB encoded per image. "
        );
    } else {
        let _ = write!(
            out,
            "[Image #{id} not sent: its dimensions could not be verified. "
        );
    }
    if let Some(source_ref) = &image.source_ref {
        write_host_image_recovery_notice(out, source_ref, max_dimension);
        return;
    }
    if image.inline_data.is_none()
        && let Some(path) = &image.snapshot_path
    {
        let _ = write!(
            out,
            "The original is saved at {path}. Use an available image tool to save a smaller copy to a new file"
        );
        if let Some(extension) = media_type_extension(&image.media_type) {
            let _ = write!(out, " ending in {extension}");
        }
        out.push_str(", then read_file the copy. If no image tool is available, ask the user before installing one.]\n");
        return;
    }
    out.push_str("No local image file is available; ask the user for a smaller copy.]\n");
}

fn media_type_extension(media_type: &str) -> Option<&'static str> {
    match media_type {
        "image/png" => Some(".png"),
        "image/jpeg" => Some(".jpg"),
        "image/gif" => Some(".gif"),
        "image/webp" => Some(".webp"),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
