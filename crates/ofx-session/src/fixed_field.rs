use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

macro_rules! fixed_field {
    ($name:ident, $value:expr) => {
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
        pub(crate) struct $name;

        impl $name {
            fn value() -> Value {
                $value
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                Self::value().serialize(serializer)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                if Value::deserialize(deserializer)? == Self::value() {
                    Ok(Self)
                } else {
                    Err(D::Error::custom("unsupported value"))
                }
            }
        }
    };
}

fixed_field!(Null, Value::Null);
fixed_field!(NoItems, Value::Array(Vec::new()));
fixed_field!(False, Value::Bool(false));
fixed_field!(ValidIdentity, Value::from("valid"));
fixed_field!(LocalProvenance, Value::from("fx_local"));
fixed_field!(TurnOrigin, Value::from("turn"));

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
}
