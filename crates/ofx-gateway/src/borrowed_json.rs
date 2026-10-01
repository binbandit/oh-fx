use std::borrow::Cow;
use std::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::ser::{Serialize, SerializeMap, Serializer};
use serde_json::Number;

const PAIRWISE_KEY_CHECK_LIMIT: usize = 16;

pub(crate) enum Json<'a> {
    Null,
    Bool(bool),
    Number(Number),
    String(Cow<'a, str>),
    Array(Vec<Json<'a>>),
    Object(Object<'a>),
}

#[derive(Default)]
pub(crate) struct Object<'a> {
    entries: Vec<(Cow<'a, str>, Json<'a>)>,
}

pub(crate) fn parse(bytes: &[u8]) -> Option<Json<'_>> {
    serde_json::from_slice(bytes).ok()
}

pub(crate) fn compact(value: &impl Serialize) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

impl<'a> Json<'a> {
    pub(crate) fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(text) => Some(text),
            _ => None,
        }
    }

    pub(crate) fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Number(number) => number.as_i64(),
            _ => None,
        }
    }

    pub(crate) fn as_array(&self) -> Option<&[Json<'a>]> {
        match self {
            Self::Array(items) => Some(items),
            _ => None,
        }
    }

    pub(crate) fn as_object(&self) -> Option<&Object<'a>> {
        match self {
            Self::Object(object) => Some(object),
            _ => None,
        }
    }
}

impl<'a> Object<'a> {
    pub(crate) fn get(&self, key: &str) -> Option<&Json<'a>> {
        self.entries
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&str, &Json<'a>)> {
        self.entries
            .iter()
            .map(|(name, value)| (name.as_ref(), value))
    }

    fn has_duplicate_key(&self) -> bool {
        let entries = &self.entries;
        if entries.len() <= PAIRWISE_KEY_CHECK_LIMIT {
            return entries
                .iter()
                .enumerate()
                .any(|(index, (key, _))| entries[..index].iter().any(|(prior, _)| prior == key));
        }
        let mut keys: Vec<&str> = entries.iter().map(|(key, _)| key.as_ref()).collect();
        keys.sort_unstable();
        keys.windows(2).any(|pair| pair[0] == pair[1])
    }
}

impl<'de> Deserialize<'de> for Json<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(JsonVisitor)
    }
}

struct JsonVisitor;

impl<'de> Visitor<'de> for JsonVisitor {
    type Value = Json<'de>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Self::Value, E> {
        Ok(Json::Bool(value))
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
        Ok(Json::Number(value.into()))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
        Ok(Json::Number(value.into()))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Self::Value, E> {
        Ok(Number::from_f64(value).map_or(Json::Null, Json::Number))
    }

    fn visit_borrowed_str<E: de::Error>(self, value: &'de str) -> Result<Self::Value, E> {
        Ok(Json::String(Cow::Borrowed(value)))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        Ok(Json::String(Cow::Owned(value.to_owned())))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
        Ok(Json::String(Cow::Owned(value)))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(Json::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Self::Value, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = sequence.next_element()? {
            items.push(item);
        }
        Ok(Json::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut fields: A) -> Result<Self::Value, A::Error> {
        let mut object = Object::default();
        while let Some(Key(key)) = fields.next_key()? {
            let value = fields.next_value()?;
            object.entries.push((key, value));
        }
        if object.has_duplicate_key() {
            return Err(de::Error::custom("duplicate field"));
        }
        Ok(Json::Object(object))
    }
}

struct Key<'a>(Cow<'a, str>);

impl<'de> Deserialize<'de> for Key<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_str(KeyVisitor)
    }
}

struct KeyVisitor;

impl<'de> Visitor<'de> for KeyVisitor {
    type Value = Key<'de>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an object key")
    }

    fn visit_borrowed_str<E: de::Error>(self, value: &'de str) -> Result<Self::Value, E> {
        Ok(Key(Cow::Borrowed(value)))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        Ok(Key(Cow::Owned(value.to_owned())))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
        Ok(Key(Cow::Owned(value)))
    }
}

impl Serialize for Json<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Null => serializer.serialize_unit(),
            Self::Bool(value) => serializer.serialize_bool(*value),
            Self::Number(number) => number.serialize(serializer),
            Self::String(text) => serializer.serialize_str(text),
            Self::Array(items) => items.serialize(serializer),
            Self::Object(object) => object.serialize(serializer),
        }
    }
}

impl Serialize for Object<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.entries.len()))?;
        for (key, value) in &self.entries {
            map.serialize_entry(key.as_ref(), value)?;
        }
        map.end()
    }
}

#[cfg(test)]
mod tests;
