use std::fmt::Write;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ofx_contract::ChatMessage;

const SUPPORTED_MEDIA_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImageFormat {
    Png,
    Jpeg,
    Gif,
    Webp,
}

pub fn detect_media_type(bytes: &[u8]) -> Option<&'static str> {
    Some(match detect_format(bytes)? {
        ImageFormat::Png => "image/png",
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::Gif => "image/gif",
        ImageFormat::Webp => "image/webp",
    })
}

fn detect_format(bytes: &[u8]) -> Option<ImageFormat> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(ImageFormat::Png)
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(ImageFormat::Jpeg)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(ImageFormat::Gif)
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some(ImageFormat::Webp)
    } else {
        None
    }
}

pub const MAX_ENCODED_IMAGE_BYTES: usize = 5 * 1024 * 1024;
const MAX_IMAGE_DIMENSION: u32 = 2000;
pub const MAX_SINGLE_IMAGE_DIMENSION: u32 = 8000;
const STRICT_IMAGE_COUNT: usize = 20;
const HEADER_PROBE_BYTES: usize = 30;
const MAX_JPEG_SEGMENTS: usize = 4096;

pub(crate) type ReadAt<'a> = &'a dyn Fn(u64, &mut [u8]) -> usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dimensions {
    pub width: u32,
    pub height: u32,
}

impl Dimensions {
    pub fn exceeds(self, limit: u32) -> bool {
        self.width > limit || self.height > limit
    }
}

pub(crate) fn request_max_dimension(image_count: usize) -> u32 {
    if image_count > STRICT_IMAGE_COUNT {
        MAX_IMAGE_DIMENSION
    } else {
        MAX_SINGLE_IMAGE_DIMENSION
    }
}

pub(crate) fn supported_media_type(mime_type: &str) -> bool {
    SUPPORTED_MEDIA_TYPES.contains(&mime_type)
}

pub(crate) fn count_request_images(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .map(|message| match message {
            ChatMessage::User { images, .. } => images.len(),
            ChatMessage::Tool { images, .. } => images.len(),
            _ => 0,
        })
        .fold(0, usize::saturating_add)
}

pub(crate) fn fits_encoded_image_limit(raw_bytes: usize) -> bool {
    raw_bytes
        .checked_add(2)
        .and_then(|padded| (padded / 3).checked_mul(4))
        .is_some_and(|encoded| encoded <= MAX_ENCODED_IMAGE_BYTES)
}

pub(crate) fn write_host_image_recovery_notice(
    out: &mut String,
    source_ref: &str,
    max_dimension: u32,
) {
    let quoted = serde_json::to_string(source_ref).unwrap_or_default();
    let _ = writeln!(
        out,
        "Host source reference: {quoted}. Use an available host-provided tool that accepts this reference to make a new copy at most {max_dimension} pixels per side and 5 MiB encoded, then return the copy as image data. If no suitable host tool or source is available, ask the user for a smaller image.]"
    );
}

pub(crate) fn encoded_image_dimensions(encoded: &str) -> Option<Dimensions> {
    dimensions_from(&|offset, buffer| read_base64(encoded.as_bytes(), offset, buffer))
}

fn read_base64(encoded: &[u8], offset: u64, buffer: &mut [u8]) -> usize {
    let Ok(offset) = usize::try_from(offset) else {
        return 0;
    };
    let mut written = 0;
    let mut group = offset / 3;
    let mut skip = offset % 3;
    while written < buffer.len() {
        let Some(quad) = group
            .checked_mul(4)
            .and_then(|start| encoded.get(start..start.checked_add(4)?))
        else {
            break;
        };
        let mut decoded = [0; 3];
        let Ok(length) = STANDARD.decode_slice(quad, &mut decoded) else {
            break;
        };
        if skip < length {
            let count = (length - skip).min(buffer.len() - written);
            buffer[written..written + count].copy_from_slice(&decoded[skip..skip + count]);
            written += count;
        }
        skip = 0;
        if length < 3 {
            break;
        }
        group += 1;
    }
    written
}

pub fn image_dimensions(bytes: &[u8]) -> Option<Dimensions> {
    dimensions_from(&|offset, buffer| {
        let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(bytes.len());
        let count = buffer.len().min(bytes.len() - start);
        buffer[..count].copy_from_slice(&bytes[start..start + count]);
        count
    })
}

pub(crate) fn positional_image_dimensions(read_at: ReadAt<'_>) -> Option<Dimensions> {
    dimensions_from(read_at)
}

fn dimensions_from(read_at: ReadAt<'_>) -> Option<Dimensions> {
    let mut buffer = [0; HEADER_PROBE_BYTES];
    let length = read_at(0, &mut buffer);
    let header = &buffer[..length];
    match detect_format(header)? {
        ImageFormat::Png => png_dimensions(header),
        ImageFormat::Jpeg => jpeg_dimensions(read_at),
        ImageFormat::Gif => gif_dimensions(header),
        ImageFormat::Webp => webp_dimensions(header),
    }
}

fn non_zero(width: u32, height: u32) -> Option<Dimensions> {
    (width != 0 && height != 0).then_some(Dimensions { width, height })
}

fn u16_le(bytes: &[u8]) -> u32 {
    u32::from(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn u16_be(bytes: &[u8]) -> u32 {
    u32::from(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn u24_le(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0])
}

fn u32_le(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn u32_be(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn png_dimensions(header: &[u8]) -> Option<Dimensions> {
    if header.len() < 24 || &header[12..16] != b"IHDR" {
        return None;
    }
    non_zero(u32_be(&header[16..20]), u32_be(&header[20..24]))
}

fn gif_dimensions(header: &[u8]) -> Option<Dimensions> {
    if header.len() < 10 {
        return None;
    }
    non_zero(u16_le(&header[6..8]), u16_le(&header[8..10]))
}

fn webp_dimensions(header: &[u8]) -> Option<Dimensions> {
    if header.len() < 16 {
        return None;
    }
    match &header[12..16] {
        b"VP8 " => {
            if header.len() < 30 || &header[23..26] != b"\x9d\x01\x2a" {
                return None;
            }
            non_zero(
                u16_le(&header[26..28]) & 0x3fff,
                u16_le(&header[28..30]) & 0x3fff,
            )
        }
        b"VP8L" => {
            if header.len() < 25 || header[20] != 0x2f {
                return None;
            }
            let bits = u32_le(&header[21..25]);
            non_zero((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1)
        }
        b"VP8X" => {
            if header.len() < 30 {
                return None;
            }
            non_zero(u24_le(&header[24..27]) + 1, u24_le(&header[27..30]) + 1)
        }
        _ => None,
    }
}

fn is_jpeg_start_of_frame(marker: u8) -> bool {
    matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf)
}

fn jpeg_dimensions(read_at: ReadAt<'_>) -> Option<Dimensions> {
    let mut offset: u64 = 2;
    for _ in 0..MAX_JPEG_SEGMENTS {
        let mut buffer = [0; 9];
        let length = read_at(offset, &mut buffer);
        let segment = &buffer[..length];
        if segment.len() < 2 || segment[0] != 0xff {
            return None;
        }
        match segment[1] {
            0xff => offset += 1,
            0x01 | 0xd0..=0xd8 => offset += 2,
            0xd9 | 0xda => return None,
            marker => {
                if segment.len() < 4 {
                    return None;
                }
                let length = u16_be(&segment[2..4]);
                if length < 2 {
                    return None;
                }
                if is_jpeg_start_of_frame(marker) {
                    if segment.len() < 9 {
                        return None;
                    }
                    return non_zero(u16_be(&segment[7..9]), u16_be(&segment[5..7]));
                }
                offset = offset.checked_add(2 + u64::from(length))?;
            }
        }
    }
    None
}

#[cfg(test)]
pub(crate) mod tests;
