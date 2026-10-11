use std::cmp::Ordering;
use std::collections::HashSet;

use serde_json::{Map, Value};

use super::{
    ElicitationError, MAX_NAME_BYTES, MAX_STRING_BYTES, Outcome, bounded_string, secret, uri,
    validate_field_json,
};
use crate::fmt_int::parse_unsigned;
use crate::json_number::{self, Number, NumberError};
use crate::json_schema_pattern::{self, Limits, PatternError};
use crate::protocol_negotiation::ElicitationWire;

const MAX_SCHEMA_BYTES: usize = 128 * 1024;
const MAX_FIELDS: usize = 64;
const MAX_OPTIONS: usize = 64;
const MAX_LABEL_BYTES: usize = 1024;
const MAX_PATTERN_BYTES: usize = 512;
pub(super) const PATTERN_LIMITS: Limits = Limits {
    depth: 64,
    states: 2048,
    repeat: 1024,
    steps: 100_000,
};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FormSchema {
    pub(crate) fields: Vec<Field>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FieldKind {
    String,
    Number,
    Integer,
    Boolean,
    SingleSelect,
    MultiSelect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    None,
    Email,
    Uri,
    Date,
    DateTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Choice {
    pub(crate) value: String,
    pub(crate) title: String,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Field {
    pub(crate) name: String,
    pub(crate) title: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) kind: FieldKind,
    pub(crate) required: bool,
    pub(crate) default_json: Option<String>,
    min_length: Option<usize>,
    max_length: Option<usize>,
    minimum: Option<Number>,
    maximum: Option<Number>,
    multiple_of: Option<Number>,
    pub(crate) min_items: Option<usize>,
    pub(crate) max_items: Option<usize>,
    format: Format,
    pattern: Option<String>,
    pub(crate) choices: Vec<Choice>,
}

impl Field {
    pub(crate) fn display_name(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.name)
    }
}

pub(super) fn parse_form_schema(wire: ElicitationWire, schema: &Value) -> Outcome<FormSchema> {
    let text = serde_json::to_string(schema).map_err(|_| ElicitationError::InvalidSchema)?;
    if text.len() > MAX_SCHEMA_BYTES {
        return Err(ElicitationError::LimitExceeded);
    }
    let object = schema.as_object().ok_or(ElicitationError::InvalidSchema)?;
    if object.contains_key("title") || object.contains_key("description") {
        return Err(ElicitationError::UnsupportedSchema);
    }
    if let Some(additional) = object.get("additionalProperties")
        && additional != &Value::Bool(false)
    {
        return Err(ElicitationError::UnsupportedSchema);
    }
    if object.get("type").and_then(Value::as_str) != Some("object") {
        return Err(ElicitationError::InvalidSchema);
    }
    let properties = object
        .get("properties")
        .ok_or(ElicitationError::InvalidSchema)?
        .as_object()
        .filter(|properties| properties.len() <= MAX_FIELDS)
        .ok_or(ElicitationError::LimitExceeded)?;
    let required = required_names(object.get("required"), properties)?;
    let fields = properties
        .iter()
        .map(|(name, schema)| {
            if name.is_empty() || name.len() > MAX_NAME_BYTES {
                return Err(ElicitationError::InvalidSchema);
            }
            parse_field(wire, name, schema, required.contains(name.as_str()))
        })
        .collect::<Outcome<_>>()?;
    Ok(FormSchema { fields })
}

fn required_names<'a>(
    required: Option<&'a Value>,
    properties: &Map<String, Value>,
) -> Outcome<HashSet<&'a str>> {
    let mut names = HashSet::new();
    let Some(required) = required else {
        return Ok(names);
    };
    let entries = required
        .as_array()
        .filter(|entries| entries.len() <= MAX_FIELDS)
        .ok_or(ElicitationError::InvalidSchema)?;
    for entry in entries {
        let name = entry
            .as_str()
            .filter(|name| {
                !name.is_empty() && name.len() <= MAX_NAME_BYTES && properties.contains_key(*name)
            })
            .ok_or(ElicitationError::InvalidSchema)?;
        if !names.insert(name) {
            return Err(ElicitationError::InvalidSchema);
        }
    }
    Ok(names)
}

fn parse_field(
    wire: ElicitationWire,
    name: &str,
    schema: &Value,
    required: bool,
) -> Outcome<Field> {
    let object = schema.as_object().ok_or(ElicitationError::InvalidSchema)?;
    let kind = object
        .get("type")
        .ok_or(ElicitationError::InvalidSchema)?
        .as_str()
        .ok_or(ElicitationError::UnsupportedSchema)?;
    let label = |key: &str| {
        object
            .get(key)
            .map(|value| {
                bounded_string(
                    Some(value),
                    MAX_LABEL_BYTES,
                    ElicitationError::InvalidSchema,
                )
            })
            .transpose()
    };
    let title = label("title")?;
    let description = label("description")?;
    if secret::is_secret_field(name, title) {
        return Err(ElicitationError::SecretField);
    }
    let mut field = Field {
        name: name.to_owned(),
        title: title.map(str::to_owned),
        description: description.map(str::to_owned),
        kind: FieldKind::String,
        required,
        default_json: None,
        min_length: None,
        max_length: None,
        minimum: None,
        maximum: None,
        multiple_of: None,
        min_items: None,
        max_items: None,
        format: Format::None,
        pattern: None,
        choices: Vec::new(),
    };
    let legacy_2025_06 = wire == ElicitationWire::LegacyMcp2025_06;
    match kind {
        "string" => {
            field.parse_string_constraints(object, legacy_2025_06)?;
            if let Some(choices) = object.get("oneOf") {
                if legacy_2025_06 {
                    return Err(ElicitationError::UnsupportedSchema);
                }
                field.choices = titled_choices(choices)?;
                field.kind = FieldKind::SingleSelect;
            } else if let Some(choices) = object.get("enum") {
                field.choices = enum_choices(choices, object.get("enumNames"))?;
                field.kind = FieldKind::SingleSelect;
            }
        }
        "number" | "integer" => {
            field.kind = if kind == "number" {
                FieldKind::Number
            } else {
                FieldKind::Integer
            };
            field.parse_number_constraints(object)?;
        }
        "boolean" => field.kind = FieldKind::Boolean,
        "array" => {
            if legacy_2025_06 {
                return Err(ElicitationError::UnsupportedSchema);
            }
            field.kind = FieldKind::MultiSelect;
            field.parse_multi_select(object)?;
        }
        _ => return Err(ElicitationError::UnsupportedSchema),
    }
    if let Some(default) = object.get("default") {
        let text = serde_json::to_string(default).map_err(|_| ElicitationError::InvalidSchema)?;
        if text.len() > MAX_STRING_BYTES {
            return Err(ElicitationError::LimitExceeded);
        }
        field.default_json = Some(text);
        field.validate_value(default)?;
    }
    Ok(field)
}

impl Field {
    fn parse_string_constraints(
        &mut self,
        object: &Map<String, Value>,
        legacy_2025_06: bool,
    ) -> Outcome<()> {
        self.min_length = bounded_count(object.get("minLength"), MAX_STRING_BYTES)?;
        self.max_length = bounded_count(object.get("maxLength"), MAX_STRING_BYTES)?;
        if self
            .min_length
            .zip(self.max_length)
            .is_some_and(|(min, max)| min > max)
        {
            return Err(ElicitationError::InvalidSchema);
        }
        if let Some(format) = object.get("format") {
            let format = bounded_string(
                Some(format),
                MAX_NAME_BYTES,
                ElicitationError::InvalidSchema,
            )?;
            self.format = match format {
                "email" => Format::Email,
                "uri" => Format::Uri,
                "date" => Format::Date,
                "date-time" => Format::DateTime,
                _ => return Err(ElicitationError::UnsupportedSchema),
            };
        }
        if let Some(pattern) = object.get("pattern") {
            if legacy_2025_06 {
                return Err(ElicitationError::UnsupportedSchema);
            }
            let pattern = pattern
                .as_str()
                .filter(|pattern| pattern.len() <= MAX_PATTERN_BYTES)
                .ok_or(ElicitationError::LimitExceeded)?;
            json_schema_pattern::validate(pattern, PATTERN_LIMITS).map_err(schema_pattern_error)?;
            self.pattern = Some(pattern.to_owned());
        }
        Ok(())
    }

    fn parse_number_constraints(&mut self, object: &Map<String, Value>) -> Outcome<()> {
        self.minimum = exact_number(object.get("minimum"))?;
        self.maximum = exact_number(object.get("maximum"))?;
        self.multiple_of = exact_number(object.get("multipleOf"))?;
        if let (Some(minimum), Some(maximum)) = (&self.minimum, &self.maximum)
            && minimum.order(maximum) == Ordering::Greater
        {
            return Err(ElicitationError::InvalidSchema);
        }
        if self
            .multiple_of
            .as_ref()
            .is_some_and(|multiple| multiple.is_negative() || multiple.is_zero())
        {
            return Err(ElicitationError::InvalidSchema);
        }
        Ok(())
    }

    fn parse_multi_select(&mut self, object: &Map<String, Value>) -> Outcome<()> {
        self.min_items = bounded_count(object.get("minItems"), MAX_OPTIONS)?;
        self.max_items = bounded_count(object.get("maxItems"), MAX_OPTIONS)?;
        if self
            .min_items
            .zip(self.max_items)
            .is_some_and(|(min, max)| min > max)
        {
            return Err(ElicitationError::InvalidSchema);
        }
        let items = object
            .get("items")
            .and_then(Value::as_object)
            .ok_or(ElicitationError::InvalidSchema)?;
        let item_type = items.get("type");
        let string_items = |item_type: &Value| {
            if item_type.as_str() == Some("string") {
                Ok(())
            } else {
                Err(ElicitationError::UnsupportedSchema)
            }
        };
        if let Some(choices) = items.get("anyOf") {
            item_type.map(string_items).transpose()?;
            self.choices = titled_choices(choices)?;
        } else if let Some(choices) = items.get("enum") {
            string_items(item_type.ok_or(ElicitationError::InvalidSchema)?)?;
            self.choices = enum_choices(choices, items.get("enumNames"))?;
        } else {
            return Err(ElicitationError::InvalidSchema);
        }
        if self.choices.is_empty() {
            return Err(ElicitationError::InvalidSchema);
        }
        Ok(())
    }

    pub(crate) fn validate_value(&self, value: &Value) -> Outcome<()> {
        match self.kind {
            FieldKind::String => self.validate_string(value),
            FieldKind::Number | FieldKind::Integer => {
                let number = json_number::from_value(value)
                    .ok()
                    .flatten()
                    .ok_or(ElicitationError::InvalidResponse)?;
                self.validate_number(&number)
            }
            FieldKind::Boolean if value.is_boolean() => Ok(()),
            FieldKind::SingleSelect
                if value.as_str().is_some_and(|value| self.has_choice(value)) =>
            {
                Ok(())
            }
            FieldKind::MultiSelect => self.validate_selection(value),
            FieldKind::Boolean | FieldKind::SingleSelect => Err(ElicitationError::InvalidResponse),
        }
    }

    pub(super) fn validate_number_lexeme(&self, lexeme: &str) -> Outcome<()> {
        let number = Number::parse(lexeme).map_err(|_| ElicitationError::InvalidResponse)?;
        self.validate_number(&number)
    }

    fn validate_string(&self, value: &Value) -> Outcome<()> {
        let text = value
            .as_str()
            .filter(|text| text.len() <= MAX_STRING_BYTES)
            .ok_or(ElicitationError::InvalidResponse)?;
        let count = text.chars().count();
        if self.min_length.is_some_and(|minimum| count < minimum)
            || self.max_length.is_some_and(|maximum| count > maximum)
        {
            return Err(ElicitationError::InvalidResponse);
        }
        if let Some(pattern) = &self.pattern
            && !json_schema_pattern::matches(pattern, text, PATTERN_LIMITS)
                .map_err(schema_pattern_error)?
        {
            return Err(ElicitationError::InvalidResponse);
        }
        if !self.format.accepts(text) {
            return Err(ElicitationError::InvalidResponse);
        }
        Ok(())
    }

    fn validate_number(&self, number: &Number) -> Outcome<()> {
        if self.kind == FieldKind::Integer && !number.is_integer() {
            return Err(ElicitationError::InvalidResponse);
        }
        if self
            .minimum
            .as_ref()
            .is_some_and(|minimum| number.order(minimum) == Ordering::Less)
            || self
                .maximum
                .as_ref()
                .is_some_and(|maximum| number.order(maximum) == Ordering::Greater)
        {
            return Err(ElicitationError::InvalidResponse);
        }
        if let Some(multiple) = &self.multiple_of
            && number.is_multiple_of(multiple) != Ok(true)
        {
            return Err(ElicitationError::InvalidResponse);
        }
        Ok(())
    }

    fn validate_selection(&self, value: &Value) -> Outcome<()> {
        let items = value
            .as_array()
            .filter(|items| items.len() <= self.choices.len())
            .ok_or(ElicitationError::InvalidResponse)?;
        if self.min_items.is_some_and(|minimum| items.len() < minimum)
            || self.max_items.is_some_and(|maximum| items.len() > maximum)
        {
            return Err(ElicitationError::InvalidResponse);
        }
        for (index, item) in items.iter().enumerate() {
            let text = item
                .as_str()
                .filter(|text| self.has_choice(text))
                .ok_or(ElicitationError::InvalidResponse)?;
            if items[..index]
                .iter()
                .any(|prior| prior.as_str() == Some(text))
            {
                return Err(ElicitationError::InvalidResponse);
            }
        }
        Ok(())
    }

    fn has_choice(&self, value: &str) -> bool {
        self.choices.iter().any(|choice| choice.value == value)
    }
}

impl FormSchema {
    pub(super) fn validate_answers(&self, content: &[(String, String)]) -> Outcome<()> {
        if content.len() > self.fields.len()
            || content
                .iter()
                .any(|(name, _)| !self.fields.iter().any(|field| &field.name == name))
        {
            return Err(ElicitationError::InvalidResponse);
        }
        for field in &self.fields {
            match content.iter().find(|(name, _)| name == &field.name) {
                Some((_, json)) => validate_field_json(field, json)?,
                None if field.required => return Err(ElicitationError::InvalidResponse),
                None => {}
            }
        }
        Ok(())
    }
}

fn titled_choices(value: &Value) -> Outcome<Vec<Choice>> {
    let items = value
        .as_array()
        .filter(|items| (1..=MAX_OPTIONS).contains(&items.len()))
        .ok_or(ElicitationError::InvalidSchema)?;
    let mut choices: Vec<Choice> = Vec::with_capacity(items.len());
    for item in items {
        let item = item.as_object().ok_or(ElicitationError::InvalidSchema)?;
        let constant = item.get("const").ok_or(ElicitationError::InvalidSchema)?;
        let title = item.get("title").ok_or(ElicitationError::InvalidSchema)?;
        let value = constant
            .as_str()
            .ok_or(ElicitationError::UnsupportedSchema)?;
        let title = bounded_string(
            Some(title),
            MAX_LABEL_BYTES,
            ElicitationError::InvalidSchema,
        )?;
        if choices.iter().any(|prior| prior.value == value) {
            return Err(ElicitationError::InvalidSchema);
        }
        if item.contains_key("description") {
            return Err(ElicitationError::UnsupportedSchema);
        }
        choices.push(Choice {
            value: value.to_owned(),
            title: title.to_owned(),
        });
    }
    Ok(choices)
}

fn enum_choices(value: &Value, names: Option<&Value>) -> Outcome<Vec<Choice>> {
    let items = value
        .as_array()
        .filter(|items| (1..=MAX_OPTIONS).contains(&items.len()))
        .ok_or(ElicitationError::InvalidSchema)?;
    let names = names
        .map(|names| {
            names
                .as_array()
                .filter(|names| names.len() == items.len())
                .ok_or(ElicitationError::InvalidSchema)
        })
        .transpose()?;
    let mut choices: Vec<Choice> = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let value = item.as_str().ok_or(ElicitationError::UnsupportedSchema)?;
        let title = names.map_or(item, |names| &names[index]);
        let title = bounded_string(
            Some(title),
            MAX_LABEL_BYTES,
            ElicitationError::InvalidSchema,
        )?;
        if choices.iter().any(|prior| prior.value == value) {
            return Err(ElicitationError::InvalidSchema);
        }
        choices.push(Choice {
            value: value.to_owned(),
            title: title.to_owned(),
        });
    }
    Ok(choices)
}

fn bounded_count(value: Option<&Value>, maximum: usize) -> Outcome<Option<usize>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let count = value.as_i64().ok_or(ElicitationError::InvalidSchema)?;
    if count < 0 {
        return Err(ElicitationError::InvalidSchema);
    }
    usize::try_from(count)
        .ok()
        .filter(|count| *count <= maximum)
        .map(Some)
        .ok_or(ElicitationError::LimitExceeded)
}

fn exact_number(value: Option<&Value>) -> Outcome<Option<Number>> {
    let Some(value) = value else {
        return Ok(None);
    };
    match json_number::from_value(value) {
        Ok(Some(number)) => Ok(Some(number)),
        Ok(None) | Err(NumberError::InvalidNumber) => Err(ElicitationError::InvalidSchema),
        Err(NumberError::NumberLimitExceeded) => Err(ElicitationError::LimitExceeded),
    }
}

fn schema_pattern_error(error: PatternError) -> ElicitationError {
    match error {
        PatternError::InvalidPattern => ElicitationError::InvalidSchema,
        PatternError::PatternLimitExceeded => ElicitationError::LimitExceeded,
    }
}

impl Format {
    fn accepts(self, value: &str) -> bool {
        match self {
            Self::None => true,
            Self::Email => simple_email(value),
            Self::Uri => uri::parse(value).is_some(),
            Self::Date => valid_date(value.as_bytes()),
            Self::DateTime => valid_date_time(value.as_bytes()),
        }
    }
}

fn simple_email(value: &str) -> bool {
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    !local.is_empty() && !domain.is_empty() && !domain.contains('@') && domain.contains('.')
}

fn digits(bytes: &[u8], maximum: u16) -> Option<u64> {
    parse_unsigned(bytes, 10).filter(|value| *value <= u64::from(maximum))
}

fn valid_date(value: &[u8]) -> bool {
    if value.len() != 10 || value[4] != b'-' || value[7] != b'-' {
        return false;
    }
    let (Some(year), Some(month), Some(day)) = (
        digits(&value[..4], u16::MAX),
        digits(&value[5..7], u8::MAX.into()),
        digits(&value[8..10], u8::MAX.into()),
    ) else {
        return false;
    };
    if year == 0 || month == 0 || month > 12 || day == 0 {
        return false;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    usize::try_from(month - 1)
        .ok()
        .and_then(|index| days.get(index))
        .is_some_and(|last| day <= *last)
}

fn valid_date_time(value: &[u8]) -> bool {
    if value.len() < 20
        || !valid_date(&value[..10])
        || !matches!(value[10], b'T' | b't')
        || value[13] != b':'
        || value[16] != b':'
    {
        return false;
    }
    let (Some(hour), Some(minute), Some(second)) = (
        digits(&value[11..13], u8::MAX.into()),
        digits(&value[14..16], u8::MAX.into()),
        digits(&value[17..19], u8::MAX.into()),
    ) else {
        return false;
    };
    if hour > 23 || minute > 59 || second > 60 {
        return false;
    }
    let mut index = 19;
    if value.get(index) == Some(&b'.') {
        index += 1;
        let start = index;
        while value.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if index == start {
            return false;
        }
    }
    if index + 1 == value.len() && matches!(value[index], b'Z' | b'z') {
        return true;
    }
    if index + 6 != value.len() || !matches!(value[index], b'+' | b'-') || value[index + 3] != b':'
    {
        return false;
    }
    digits(&value[index + 1..index + 3], u8::MAX.into()).is_some_and(|hour| hour <= 23)
        && digits(&value[index + 4..index + 6], u8::MAX.into()).is_some_and(|minute| minute <= 59)
}
