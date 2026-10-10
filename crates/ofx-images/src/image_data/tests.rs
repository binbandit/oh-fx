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
