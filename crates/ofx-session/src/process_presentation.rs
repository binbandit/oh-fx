use ofx_contract::CommandProcessPresentation;

use crate::json_fields::{Fields, Json};

const EXIT_CODE: &str = "exit_code";
const SIGNAL: &str = "signal";
const TIMED_OUT: &str = "timed_out";
const OUTPUT_CAPTURE_FAILED: &str = "output_capture_failed";

fn kind(presentation: CommandProcessPresentation) -> &'static str {
    match presentation {
        CommandProcessPresentation::ExitCode(_) => EXIT_CODE,
        CommandProcessPresentation::Signal(_) => SIGNAL,
        CommandProcessPresentation::TimedOut => TIMED_OUT,
        CommandProcessPresentation::OutputCaptureFailed => OUTPUT_CAPTURE_FAILED,
    }
}

fn from_kind(kind: &str, value: &Json<'_>, empty: bool) -> Option<CommandProcessPresentation> {
    match kind {
        EXIT_CODE => value.as_i64().map(CommandProcessPresentation::ExitCode),
        SIGNAL => u32::try_from(value.as_u64()?)
            .ok()
            .map(CommandProcessPresentation::Signal),
        TIMED_OUT => empty.then_some(CommandProcessPresentation::TimedOut),
        OUTPUT_CAPTURE_FAILED => empty.then_some(CommandProcessPresentation::OutputCaptureFailed),
        _ => None,
    }
}

pub(crate) mod frame {
    use ofx_contract::CommandProcessPresentation;
    use serde::Serializer;
    use serde::ser::SerializeMap;

    use super::{Json, from_kind, kind};

    #[derive(serde::Serialize)]
    struct Empty {}

    pub(crate) fn serialize<S: Serializer, P: Copy + Into<Option<CommandProcessPresentation>>>(
        presentation: &P,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let Some(presentation) = (*presentation).into() else {
            return serializer.serialize_none();
        };
        let mut map = serializer.serialize_map(Some(1))?;
        match presentation {
            CommandProcessPresentation::ExitCode(code) => {
                map.serialize_entry(kind(presentation), &code)?;
            }
            CommandProcessPresentation::Signal(signal) => {
                map.serialize_entry(kind(presentation), &signal)?;
            }
            CommandProcessPresentation::TimedOut
            | CommandProcessPresentation::OutputCaptureFailed => {
                map.serialize_entry(kind(presentation), &Empty {})?;
            }
        }
        map.end()
    }

    #[cfg(test)]
    pub(crate) fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<CommandProcessPresentation>, D::Error> {
        let value = <serde_json::Value as serde::Deserialize>::deserialize(deserializer)?;
        if value.is_null() {
            return Ok(None);
        }
        let text = value.to_string();
        crate::json_fields::parse_json(text.as_bytes())
            .ok()
            .and_then(read)
            .map(Some)
            .ok_or_else(|| serde::de::Error::custom("InvalidCommandProcessPresentation"))
    }

    pub(crate) fn read(value: Json<'_>) -> Option<CommandProcessPresentation> {
        let Json::Object(object) = value else {
            return None;
        };
        let [(name, value)] = object.into_entries().try_into().ok()?;
        let empty = matches!(&value, Json::Object(fields) if fields.is_empty());
        from_kind(&name, &value, empty)
    }
}

pub(crate) mod checkpoint {
    use ofx_contract::CommandProcessPresentation;
    use serde::Serializer;
    use serde::ser::SerializeMap;

    use super::{Fields, Json, from_kind, kind};

    pub(crate) fn serialize<S: Serializer, P: Copy + Into<Option<CommandProcessPresentation>>>(
        presentation: &P,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let Some(presentation) = (*presentation).into() else {
            return serializer.serialize_none();
        };
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("kind", kind(presentation))?;
        match presentation {
            CommandProcessPresentation::ExitCode(code) => map.serialize_entry("value", &code)?,
            CommandProcessPresentation::Signal(signal) => map.serialize_entry("value", &signal)?,
            CommandProcessPresentation::TimedOut
            | CommandProcessPresentation::OutputCaptureFailed => {
                map.serialize_entry("value", &())?;
            }
        }
        map.end()
    }

    pub(crate) fn read(value: Json<'_>) -> Option<CommandProcessPresentation> {
        let mut fields = Fields::new(value)?;
        let name = fields.string("kind")?;
        let value = fields.required("value")?;
        let empty = matches!(value, Json::Null);
        let presentation = from_kind(&name, &value, empty)?;
        fields.finish(presentation)
    }
}

#[cfg(test)]
mod tests;
