use super::crc32c::crc32c;

pub(crate) fn framed(payload: &str) -> Vec<u8> {
    format!(
        "{payload},\"crc\":\"{:08x}\"}}\n",
        crc32c(payload.as_bytes())
    )
    .into_bytes()
}
