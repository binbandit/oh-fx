use super::*;

#[test]
fn detect_media_type_from_supported_magic_bytes() {
    assert_eq!(
        detect_media_type(b"\x89PNG\r\n\x1a\nrest"),
        Some("image/png")
    );
    assert_eq!(detect_media_type(b"\xff\xd8\xffrest"), Some("image/jpeg"));
    assert_eq!(detect_media_type(b"GIF87arest"), Some("image/gif"));
    assert_eq!(detect_media_type(b"GIF89arest"), Some("image/gif"));
    assert_eq!(detect_media_type(b"RIFFxxxxWEBPrest"), Some("image/webp"));
}

#[test]
fn detect_media_type_rejects_truncated_and_unknown_signatures() {
    for bytes in [
        &b""[..],
        b"\x89PNG\r\n\x1a",
        b"\xff\xd8",
        b"GIF88a",
        b"RIFFxxxxWEB",
        b"RIFFxxxxWEBQ",
        b"not an image at all, just text",
    ] {
        assert_eq!(detect_media_type(bytes), None, "{bytes:?}");
    }
}

const TEST_JPEG_FRAME_OFFSET: usize = 27;

pub(crate) fn test_png_header(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes
}

pub(crate) fn test_jpeg(width: u16, height: u16) -> Vec<u8> {
    let mut bytes = [
        &b"\xff\xd8"[..],
        b"\xff\xe0\x00\x10JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00",
        b"\xff\xc4\x00\x04\x00\x00",
        b"\xff",
        b"\xff\xc2\x00\x11\x08\x00\x00\x00\x00\x03\x01\x22\x00\x02\x11\x01\x03\x11\x01",
        b"\xff\xd9",
    ]
    .concat();
    bytes[TEST_JPEG_FRAME_OFFSET + 5..TEST_JPEG_FRAME_OFFSET + 7]
        .copy_from_slice(&height.to_be_bytes());
    bytes[TEST_JPEG_FRAME_OFFSET + 7..TEST_JPEG_FRAME_OFFSET + 9]
        .copy_from_slice(&width.to_be_bytes());
    bytes
}

pub(crate) fn test_jpeg_behind_metadata(width: u16, height: u16) -> Vec<u8> {
    let mut bytes = b"\xff\xd8".to_vec();
    for _ in 0..6 {
        let mut segment = vec![0; 2 + 65535];
        segment[..4].copy_from_slice(b"\xff\xe1\xff\xff");
        bytes.extend_from_slice(&segment);
    }
    bytes.extend_from_slice(&test_jpeg(width, height)[2..]);
    bytes
}

fn test_gif(width: u16, height: u16) -> Vec<u8> {
    [&b"GIF89a"[..], &width.to_le_bytes(), &height.to_le_bytes()].concat()
}

fn test_webp_header(chunk: [u8; 4]) -> Vec<u8> {
    let mut bytes = vec![0; 30];
    bytes[..4].copy_from_slice(b"RIFF");
    bytes[8..12].copy_from_slice(b"WEBP");
    bytes[12..16].copy_from_slice(&chunk);
    bytes
}

fn test_webp_lossy(width: u16, height: u16) -> Vec<u8> {
    let mut bytes = test_webp_header(*b"VP8 ");
    bytes[23..26].copy_from_slice(b"\x9d\x01\x2a");
    bytes[26..28].copy_from_slice(&width.to_le_bytes());
    bytes[28..30].copy_from_slice(&height.to_le_bytes());
    bytes
}

fn test_webp_lossless(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = test_webp_header(*b"VP8L");
    bytes.truncate(25);
    bytes[20] = 0x2f;
    bytes[21..25].copy_from_slice(&((width - 1) | ((height - 1) << 14)).to_le_bytes());
    bytes
}

fn test_webp_extended(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = test_webp_header(*b"VP8X");
    bytes[24..27].copy_from_slice(&(width - 1).to_le_bytes()[..3]);
    bytes[27..30].copy_from_slice(&(height - 1).to_le_bytes()[..3]);
    bytes
}

fn size(width: u32, height: u32) -> Dimensions {
    Dimensions { width, height }
}

#[test]
fn image_dimensions_read_every_supported_header() {
    assert_eq!(
        image_dimensions(&test_png_header(3420, 2224)),
        Some(size(3420, 2224))
    );
    assert_eq!(
        image_dimensions(&test_jpeg(3420, 2224)),
        Some(size(3420, 2224))
    );
    assert_eq!(image_dimensions(&test_gif(640, 480)), Some(size(640, 480)));
    assert_eq!(
        image_dimensions(&test_webp_lossy(2001, 17)),
        Some(size(2001, 17))
    );
    assert_eq!(
        image_dimensions(&test_webp_lossless(16384, 3)),
        Some(size(16384, 3))
    );
    assert_eq!(
        image_dimensions(&test_webp_extended(5000, 2)),
        Some(size(5000, 2))
    );
}

#[test]
fn request_image_limit_follows_the_count_and_byte_boundaries() {
    assert_eq!(request_max_dimension(1), 8000);
    assert_eq!(request_max_dimension(20), 8000);
    assert_eq!(request_max_dimension(21), 2000);
    let wide = Dimensions {
        width: 3420,
        height: 2224,
    };
    assert!(!wide.exceeds(request_max_dimension(20)));
    assert!(wide.exceeds(request_max_dimension(21)));
    assert!(
        !Dimensions {
            width: 8000,
            height: 1
        }
        .exceeds(request_max_dimension(1))
    );
    assert!(
        Dimensions {
            width: 8001,
            height: 1
        }
        .exceeds(request_max_dimension(1))
    );
    let max_raw = MAX_ENCODED_IMAGE_BYTES / 4 * 3;
    assert!(fits_encoded_image_limit(max_raw));
    assert!(!fits_encoded_image_limit(max_raw + 1));
    assert!(!fits_encoded_image_limit(usize::MAX));
}

#[test]
fn request_image_count_includes_every_user_attachment() {
    let image = ofx_contract::ImageAttachment {
        path: "image.png".to_owned(),
        media_type: "image/png".to_owned(),
        ..ofx_contract::ImageAttachment::default()
    };
    let messages = [
        ChatMessage::User {
            content: String::new(),
            restored_steering: false,
            feedback_for: None,
            images: vec![image.clone(), image],
        },
        ChatMessage::user("text only"),
    ];
    assert_eq!(count_request_images(&messages), 2);
}

#[test]
fn image_dimensions_reject_malformed_and_truncated_headers() {
    let png = test_png_header(10, 10);
    assert_eq!(image_dimensions(&png[..23]), None);
    assert_eq!(image_dimensions(&test_png_header(0, 10)), None);
    let mut not_ihdr = png.clone();
    not_ihdr[12..16].copy_from_slice(b"IDAT");
    assert_eq!(image_dimensions(&not_ihdr), None);

    let jpeg = test_jpeg(10, 10);
    assert_eq!(image_dimensions(&jpeg[..TEST_JPEG_FRAME_OFFSET + 8]), None);
    let mut scan_first = jpeg.clone();
    scan_first[3] = 0xda;
    assert_eq!(image_dimensions(&scan_first), None);
    let mut short_length = jpeg;
    short_length[4..6].copy_from_slice(&1_u16.to_be_bytes());
    assert_eq!(image_dimensions(&short_length), None);

    assert_eq!(image_dimensions(&test_gif(0, 3)), None);
    assert_eq!(image_dimensions(&test_webp_header(*b"ALPH")), None);
    assert_eq!(image_dimensions(b"not an image at all, just text"), None);
    assert_eq!(image_dimensions(b""), None);
}

#[test]
fn image_dimension_parsing_stays_bounded_on_truncated_and_mutated_headers() {
    let seeds = [
        test_png_header(3420, 2224),
        test_jpeg(2001, 1),
        test_gif(1, 1),
        test_webp_lossy(2001, 17),
        test_webp_lossless(3, 5000),
        test_webp_extended(5000, 2),
        b"\xff\xd8\xff\xff\xff\xff".to_vec(),
        b"\xff\xd8\xff\xe0\xff\xff".to_vec(),
        b"\x89PNG\r\n\x1a\n\x00\x00\x00\x00IHDR\xff\xff\xff\xff\xff\xff\xff\xff".to_vec(),
        b"RIFF\x00\x00\x00\x00WEBP".to_vec(),
    ];
    let mut state: u64 = 0x1049;
    let mut next = || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        state >> 33
    };
    for seed in &seeds {
        for length in 0..=seed.len() {
            expect_encoded_agrees_with_raw(&seed[..length]);
        }
    }
    for round in 0..4000 {
        let mut bytes = seeds[round % seeds.len()].clone();
        for _ in 0..=next() % 4 {
            let index = usize::try_from(next()).unwrap() % bytes.len();
            bytes[index] = u8::try_from(next() & 0xff).unwrap();
        }
        let length = usize::try_from(next()).unwrap() % (bytes.len() + 1);
        expect_encoded_agrees_with_raw(&bytes[..length]);
    }
}

#[test]
fn positional_dimensions_find_a_jpeg_frame_header_behind_large_metadata() {
    let bytes = test_jpeg_behind_metadata(4032, 3024);
    let reader = |offset: u64, buffer: &mut [u8]| {
        let start = usize::try_from(offset).unwrap().min(bytes.len());
        let count = buffer.len().min(bytes.len() - start);
        buffer[..count].copy_from_slice(&bytes[start..start + count]);
        count
    };

    assert_eq!(positional_image_dimensions(&reader), Some(size(4032, 3024)));
    assert_eq!(image_dimensions(&bytes), Some(size(4032, 3024)));
    assert_eq!(image_dimensions(&bytes[..256 * 1024]), None);
}

#[test]
fn host_recovery_notices_quote_the_source_reference() {
    let mut out = String::new();
    write_host_image_recovery_notice(&mut out, "host:\"shot\"", 8000);
    assert_eq!(
        out,
        "Host source reference: \"host:\\\"shot\\\"\". Use an available host-provided tool that accepts this reference to make a new copy at most 8000 pixels per side and 5 MiB encoded, then return the copy as image data. If no suitable host tool or source is available, ask the user for a smaller image.]\n"
    );
}

fn expect_encoded_agrees_with_raw(bytes: &[u8]) {
    assert_eq!(
        encoded_image_dimensions(&STANDARD.encode(bytes)),
        image_dimensions(bytes),
        "{bytes:?}"
    );
}

#[test]
fn encoded_image_dimensions_agree_with_raw_headers_at_every_truncation() {
    for fixture in [
        test_png_header(3420, 2224),
        test_jpeg(3420, 2224),
        test_gif(640, 480),
        test_webp_lossy(2001, 17),
        test_webp_lossless(16384, 3),
        test_webp_extended(5000, 2),
    ] {
        for length in 0..=fixture.len() {
            expect_encoded_agrees_with_raw(&fixture[..length]);
        }
    }
    assert_eq!(encoded_image_dimensions("not base64 at all"), None);
}
