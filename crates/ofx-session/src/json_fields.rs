use std::borrow::Cow;
use std::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};

use crate::fixed_field::FixedField;

pub(crate) enum Json<'a> {
    Null,
    Bool(bool),
    Unsigned(u64),
    Signed(i64),
    Float,
    Text(Cow<'a, str>),
    List(Vec<Json<'a>>),
    Object(Vec<(Cow<'a, str>, Json<'a>)>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JsonError {
    Syntax,
    DuplicateField,
}

pub(crate) fn parse_json(bytes: &[u8]) -> Result<Json<'_>, JsonError> {
    serde_json::from_slice(bytes).map_err(|error: serde_json::Error| {
        if error.is_data() {
            JsonError::DuplicateField
        } else {
            JsonError::Syntax
        }
    })
}

impl<'a> Json<'a> {
    pub(crate) fn as_u64(&self) -> Option<u64> {
        match *self {
            Self::Unsigned(value) => Some(value),
            Self::Signed(value) => u64::try_from(value).ok(),
            _ => None,
        }
    }

    pub(crate) fn as_i64(&self) -> Option<i64> {
        match *self {
            Self::Unsigned(value) => i64::try_from(value).ok(),
            Self::Signed(value) => Some(value),
            _ => None,
        }
    }

    pub(crate) fn as_bool(&self) -> Option<bool> {
        match *self {
            Self::Bool(value) => Some(value),
            _ => None,
        }
    }

    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            _ => None,
        }
    }

    pub(crate) fn get(&self, key: &str) -> Option<&Json<'a>> {
        match self {
            Self::Object(fields) => fields
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }
}

pub(crate) fn string(value: Json<'_>) -> Option<String> {
    match value {
        Json::Text(text) => Some(text.into_owned()),
        _ => None,
    }
}

pub(crate) struct Fields<'a>(Vec<(Cow<'a, str>, Json<'a>)>);

impl<'a> Fields<'a> {
    pub(crate) fn new(value: Json<'a>) -> Option<Self> {
        match value {
            Json::Object(fields) => Some(Self(fields)),
            _ => None,
        }
    }

    pub(crate) fn required(&mut self, key: &str) -> Option<Json<'a>> {
        let index = self.0.iter().position(|(name, _)| name == key)?;
        Some(self.0.swap_remove(index).1)
    }

    pub(crate) fn string(&mut self, key: &str) -> Option<String> {
        string(self.required(key)?)
    }

    pub(crate) fn unsigned(&mut self, key: &str) -> Option<u64> {
        self.required(key)?.as_u64()
    }

    pub(crate) fn signed(&mut self, key: &str) -> Option<i64> {
        self.required(key)?.as_i64()
    }

    pub(crate) fn flag(&mut self, key: &str) -> Option<bool> {
        self.required(key)?.as_bool()
    }

    pub(crate) fn or<T>(
        &mut self,
        key: &str,
        missing: T,
        read: impl FnOnce(Json<'a>) -> Option<T>,
    ) -> Option<T> {
        match self.required(key) {
            None => Some(missing),
            Some(value) => read(value),
        }
    }

    pub(crate) fn nullable<T: Default>(
        &mut self,
        key: &str,
        read: impl FnOnce(Json<'a>) -> Option<T>,
    ) -> Option<T> {
        match self.required(key) {
            None | Some(Json::Null) => Some(T::default()),
            Some(value) => read(value),
        }
    }

    pub(crate) fn fixed<T: FixedField>(&mut self, key: &str) -> Option<T> {
        self.or(key, T::default(), |value| {
            T::accepts(&value).then(T::default)
        })
    }

    pub(crate) fn finish<T>(self, decoded: T) -> Option<T> {
        self.0.is_empty().then_some(decoded)
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
        Ok(Json::Signed(value))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
        Ok(Json::Unsigned(value))
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
        Ok(Json::Float)
    }

    fn visit_borrowed_str<E: de::Error>(self, value: &'de str) -> Result<Self::Value, E> {
        Ok(Json::Text(Cow::Borrowed(value)))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        Ok(Json::Text(Cow::Owned(value.to_owned())))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
        Ok(Json::Text(Cow::Owned(value)))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(Json::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut items: A) -> Result<Self::Value, A::Error> {
        let mut list = Vec::new();
        while let Some(item) = items.next_element()? {
            list.push(item);
        }
        Ok(Json::List(list))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut entries: A) -> Result<Self::Value, A::Error> {
        let mut fields: Vec<(Cow<'de, str>, Json<'de>)> = Vec::new();
        while let Some(key) = entries.next_key::<Json<'de>>()? {
            let Json::Text(name) = key else {
                return Err(de::Error::custom("object keys are strings"));
            };
            if fields.iter().any(|(seen, _)| *seen == name) {
                return Err(de::Error::custom("duplicate field"));
            }
            let value = entries.next_value()?;
            fields.push((name, value));
        }
        Ok(Json::Object(fields))
    }
}
