use serde::{Serialize, Serializer};
use serde_json::Value;

use crate::json_fields::Json;

pub(crate) trait FixedField: Default {
    fn accepts(value: &Json<'_>) -> bool;
}

macro_rules! fixed_field {
    ($name:ident, $value:expr, $accepts:pat $(if $guard:expr)?) => {
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
        pub(crate) struct $name;

        impl $name {
            fn value() -> Value {
                $value
            }
        }

        impl FixedField for $name {
            fn accepts(value: &Json<'_>) -> bool {
                matches!(value, $accepts $(if $guard)?)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                Self::value().serialize(serializer)
            }
        }

        #[cfg(test)]
        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                if Self::accepts(&<Json<'de> as serde::Deserialize>::deserialize(
                    deserializer,
                )?) {
                    Ok(Self)
                } else {
                    Err(serde::de::Error::custom("unsupported value"))
                }
            }
        }
    };
}

fixed_field!(Null, Value::Null, Json::Null);
fixed_field!(NoItems, Value::Array(Vec::new()), Json::List(items) if items.is_empty());
fixed_field!(False, Value::Bool(false), Json::Bool(false));
fixed_field!(ValidIdentity, Value::from("valid"), Json::Text(text) if text == "valid");
fixed_field!(LocalProvenance, Value::from("fx_local"), Json::Text(text) if text == "fx_local");
fixed_field!(TurnOrigin, Value::from("turn"), Json::Text(text) if text == "turn");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_fields_write_upstream_defaults_and_accept_nothing_else() {
        assert_eq!(serde_json::to_string(&Null).unwrap(), "null");
        assert_eq!(serde_json::to_string(&NoItems).unwrap(), "[]");
        assert_eq!(serde_json::to_string(&False).unwrap(), "false");
        assert_eq!(serde_json::to_string(&ValidIdentity).unwrap(), "\"valid\"");
        assert_eq!(
            serde_json::to_string(&LocalProvenance).unwrap(),
            "\"fx_local\""
        );
        assert_eq!(serde_json::to_string(&TurnOrigin).unwrap(), "\"turn\"");
        assert!(serde_json::from_str::<Null>("null").is_ok());
        assert!(serde_json::from_str::<Null>("\"x\"").is_err());
        assert!(serde_json::from_str::<NoItems>("[]").is_ok());
        assert!(serde_json::from_str::<NoItems>("[1]").is_err());
        assert!(serde_json::from_str::<False>("true").is_err());
        assert!(serde_json::from_str::<ValidIdentity>("\"absent\"").is_err());
        assert!(serde_json::from_str::<LocalProvenance>("\"provider_executed\"").is_err());
        assert!(serde_json::from_str::<TurnOrigin>("\"compaction\"").is_err());
        assert!(serde_json::from_str::<TurnOrigin>("0").is_err());
    }

    fn accepts_what_it_writes<T: FixedField + Serialize>(value: &T) -> bool {
        let written = serde_json::to_string(value).unwrap();
        T::accepts(&serde_json::from_str(&written).unwrap())
    }

    #[test]
    fn fixed_fields_accept_exactly_what_they_write() {
        assert!(accepts_what_it_writes(&Null));
        assert!(accepts_what_it_writes(&NoItems));
        assert!(accepts_what_it_writes(&False));
        assert!(accepts_what_it_writes(&ValidIdentity));
        assert!(accepts_what_it_writes(&LocalProvenance));
        assert!(accepts_what_it_writes(&TurnOrigin));
    }
}
