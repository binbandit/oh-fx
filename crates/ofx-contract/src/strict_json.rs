use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt;

use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::ser::{Serialize, SerializeMap, Serializer};
use serde_json::{Map, Number, Value};

const PAIRWISE_KEY_CHECK_LIMIT: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrictJsonError {
    Syntax,
    DuplicateField,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DuplicateKeys {
    BeforeValue,
    AfterValue,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Json<'a> {
    Null,
    Bool(bool),
    Number(Number),
    String(Cow<'a, str>),
    Array(Vec<Json<'a>>),
    Object(Object<'a>),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Object<'a> {
    entries: Vec<(Cow<'a, str>, Json<'a>)>,
}

pub fn parse_strict_json(
    bytes: &[u8],
    duplicates: DuplicateKeys,
) -> Result<Json<'_>, StrictJsonError> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let parsed = Seed(duplicates)
        .deserialize(&mut deserializer)
        .and_then(|json| deserializer.end().map(|()| json));
    parsed.map_err(|error| {
        if error.is_data() {
            StrictJsonError::DuplicateField
        } else {
            StrictJsonError::Syntax
        }
    })
}

pub fn parse_strict_json_value(bytes: &[u8]) -> Result<Value, StrictJsonError> {
    parse_strict_json(bytes, DuplicateKeys::AfterValue).map(Json::into_value)
}

impl<'a> Json<'a> {
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            _ => None,
        }
    }

    pub fn is_string(&self) -> bool {
        matches!(self, Self::String(_))
    }

    pub fn is_i64(&self) -> bool {
        matches!(self, Self::Number(number) if number.is_i64())
    }

    pub fn is_u64(&self) -> bool {
        matches!(self, Self::Number(number) if number.is_u64())
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Self::Number(number) => number.as_u64(),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Number(number) => number.as_i64(),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(text) => Some(text),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Json<'a>]> {
        match self {
            Self::Array(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&Object<'a>> {
        match self {
            Self::Object(object) => Some(object),
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&Json<'a>> {
        self.as_object()?.get(key)
    }

    pub fn into_value(self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Bool(value) => Value::Bool(value),
            Self::Number(number) => Value::Number(number),
            Self::String(text) => Value::String(text.into_owned()),
            Self::Array(items) => Value::Array(items.into_iter().map(Json::into_value).collect()),
            Self::Object(object) => Value::Object(
                object
                    .entries
                    .into_iter()
                    .map(|(key, value)| (key.into_owned(), value.into_value()))
                    .collect::<Map<String, Value>>(),
            ),
        }
    }
}

impl<'a> Object<'a> {
    pub fn get(&self, key: &str) -> Option<&Json<'a>> {
        self.entries
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value)
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Json<'a>)> {
        self.entries
            .iter()
            .map(|(name, value)| (name.as_ref(), value))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> &[(Cow<'a, str>, Json<'a>)] {
        &self.entries
    }

    pub fn into_entries(self) -> Vec<(Cow<'a, str>, Json<'a>)> {
        self.entries
    }
}

impl<'de> de::Deserialize<'de> for Json<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Seed(DuplicateKeys::BeforeValue).deserialize(deserializer)
    }
}

struct Seed(DuplicateKeys);

impl<'de> DeserializeSeed<'de> for Seed {
    type Value = Json<'de>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Json<'de>, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Seed {
    type Value = Json<'de>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Json<'de>, E> {
        Ok(Json::Bool(value))
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Json<'de>, E> {
        Ok(Json::Number(value.into()))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Json<'de>, E> {
        Ok(Json::Number(value.into()))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Json<'de>, E> {
        Ok(Number::from_f64(value).map_or(Json::Null, Json::Number))
    }

    fn visit_borrowed_str<E: de::Error>(self, value: &'de str) -> Result<Json<'de>, E> {
        Ok(Json::String(Cow::Borrowed(value)))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Json<'de>, E> {
        Ok(Json::String(Cow::Owned(value.to_owned())))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<Json<'de>, E> {
        Ok(Json::String(Cow::Owned(value)))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Json<'de>, E> {
        Ok(Json::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Json<'de>, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = sequence.next_element_seed(Seed(self.0))? {
            items.push(item);
        }
        Ok(Json::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut fields: A) -> Result<Json<'de>, A::Error> {
        let mut keys = Keys::default();
        let mut entries = Vec::new();
        while let Some(Key(key)) = fields.next_key()? {
            let seen = self.0 == DuplicateKeys::BeforeValue && keys.repeats(&key, &entries);
            if seen {
                return Err(de::Error::custom("duplicate field"));
            }
            let value = fields.next_value_seed(Seed(self.0))?;
            if self.0 == DuplicateKeys::AfterValue && keys.repeats(&key, &entries) {
                return Err(de::Error::custom("duplicate field"));
            }
            entries.push((key, value));
            keys.remember(&entries);
        }
        Ok(Json::Object(Object { entries }))
    }
}

#[derive(Default)]
struct Keys<'a> {
    hashed: Option<HashSet<Cow<'a, str>>>,
}

impl<'a> Keys<'a> {
    fn repeats(&self, key: &str, entries: &[(Cow<'a, str>, Json<'a>)]) -> bool {
        match &self.hashed {
            Some(hashed) => hashed.contains(key),
            None => entries.iter().any(|(prior, _)| prior == key),
        }
    }

    fn remember(&mut self, entries: &[(Cow<'a, str>, Json<'a>)]) {
        if let Some(hashed) = &mut self.hashed {
            if let Some((key, _)) = entries.last() {
                hashed.insert(key.clone());
            }
        } else if entries.len() > PAIRWISE_KEY_CHECK_LIMIT {
            self.hashed = Some(entries.iter().map(|(key, _)| key.clone()).collect());
        }
    }
}

struct Key<'a>(Cow<'a, str>);

impl<'de> de::Deserialize<'de> for Key<'de> {
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
