use std::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::map::Entry;
use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrictJsonError {
    Syntax,
    DuplicateField,
}

pub fn parse(bytes: &[u8]) -> Result<Value, StrictJsonError> {
    serde_json::from_slice::<UniqueKeys>(bytes)
        .map(|unique| unique.0)
        .map_err(|error| {
            if error.is_data() {
                StrictJsonError::DuplicateField
            } else {
                StrictJsonError::Syntax
            }
        })
}

struct UniqueKeys(Value);

impl<'de> Deserialize<'de> for UniqueKeys {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer
            .deserialize_any(UniqueKeysVisitor)
            .map(UniqueKeys)
    }
}

struct UniqueKeysVisitor;

impl<'de> Visitor<'de> for UniqueKeysVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Value, E> {
        Ok(Value::from(value))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Value, E> {
        Ok(Value::from(value))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Value, E> {
        Ok(Value::from(value))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Value, E> {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<Value, E> {
        Ok(Value::String(value))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Value, A::Error> {
        let mut items = Vec::new();
        while let Some(UniqueKeys(item)) = sequence.next_element()? {
            items.push(item);
        }
        Ok(Value::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut entries: A) -> Result<Value, A::Error> {
        let mut map = Map::new();
        while let Some(key) = entries.next_key::<String>()? {
            let UniqueKeys(value) = entries.next_value()?;
            match map.entry(key) {
                Entry::Vacant(entry) => {
                    entry.insert(value);
                }
                Entry::Occupied(_) => return Err(de::Error::custom("duplicate field")),
            }
        }
        Ok(Value::Object(map))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_duplicate_keys_at_any_depth_after_unescaping() {
        for json in [
            r#"{"a":1,"a":2}"#,
            r#"{"outer":{"a":1,"a":2}}"#,
            r#"[{"a":1,"a":2}]"#,
        ] {
            assert_eq!(parse(json.as_bytes()), Err(StrictJsonError::DuplicateField));
        }
    }

    #[test]
    fn keeps_object_order_and_reports_syntax_errors() {
        let value = parse(br#"{"b":1,"a":[true,null,"x",1.5,-2]}"#).unwrap();
        assert_eq!(value.to_string(), r#"{"b":1,"a":[true,null,"x",1.5,-2]}"#);
        assert_eq!(parse(b"{"), Err(StrictJsonError::Syntax));
        assert_eq!(parse(b"{} trailing"), Err(StrictJsonError::Syntax));
    }
}
