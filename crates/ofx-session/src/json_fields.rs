use std::borrow::Cow;

pub(crate) use ofx_contract::Json;
use ofx_contract::{DuplicateKeys, StrictJsonError, parse_strict_json};

use crate::fixed_field::FixedField;

pub(crate) fn parse_json(bytes: &[u8]) -> Result<Json<'_>, StrictJsonError> {
    parse_strict_json(bytes, DuplicateKeys::BeforeValue)
}

pub(crate) fn string(value: Json<'_>) -> Option<String> {
    match value {
        Json::String(text) => Some(text.into_owned()),
        _ => None,
    }
}

pub(crate) struct Fields<'a>(Vec<(Cow<'a, str>, Json<'a>)>);

impl<'a> Fields<'a> {
    pub(crate) fn new(value: Json<'a>) -> Option<Self> {
        match value {
            Json::Object(fields) => Some(Self(fields.into_entries())),
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

    pub(crate) fn text(&mut self, key: &str) -> Option<Cow<'a, str>> {
        match self.required(key)? {
            Json::String(text) => Some(text),
            _ => None,
        }
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

    pub(crate) fn present_or_null<T: Default>(
        &mut self,
        key: &str,
        read: impl FnOnce(Json<'a>) -> Option<T>,
    ) -> Option<T> {
        match self.required(key)? {
            Json::Null => Some(T::default()),
            value => read(value),
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
