use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};

use crate::json_fields::Json;

pub(super) fn durable_bytes(value: Json<'_>) -> Option<Vec<u8>> {
    match value {
        Json::String(text) => Some(text.into_owned().into_bytes()),
        Json::Object(entries) => match entries.entries() {
            [
                (encoding, Json::String(scheme)),
                (data, Json::String(encoded)),
            ] if encoding == "encoding" && scheme == "base64" && data == "data" => {
                STANDARD.decode(encoded.as_bytes()).ok()
            }
            _ => None,
        },
        _ => None,
    }
}

pub(super) fn durable_text(value: Json<'_>) -> Option<String> {
    String::from_utf8(durable_bytes(value)?).ok()
}

pub(super) struct DurableBytes<'a>(pub(super) &'a [u8]);

impl Serialize for DurableBytes<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if let Ok(text) = std::str::from_utf8(self.0) {
            return serializer.serialize_str(text);
        }
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("encoding", "base64")?;
        map.serialize_entry("data", &STANDARD.encode(self.0))?;
        map.end()
    }
}
