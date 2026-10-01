use ofx_contract::parse_tool_args_object;
use ofx_exec::Profile;
use ofx_text::parse_unsigned;
use serde_json::{Map, Number, Value, json};

use super::{
    DEFAULT_WAIT_CEILING_MS, DEFAULT_YIELD_TIME_MS, MAX_WAIT_CEILING_MS, MAX_YIELD_TIME_MS,
};

const MAX_COMMAND_BYTES: usize = 64 * 1024;
const MAX_WRITE_BYTES: usize = 64 * 1024;
const MAX_CORRECTION_SOURCE_BYTES: usize = 16 * 1024;
const MAX_CORRECTION_FIELDS: usize = 32;
const MAX_REPORTED_NAME_BYTES: usize = 64;
const FIELD_NAMES: [&str; 11] = [
    "action",
    "command",
    "cwd",
    "profile",
    "shell",
    "tty",
    "yield_time_ms",
    "timeout_ms",
    "session_id",
    "chars",
    "force",
];
const SHELL_MEMBERS: [&str; 3] = ["kind", "path", "clean_start"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Action {
    Run,
    Interact,
    Stop,
}

impl Action {
    fn parse(name: &str) -> Option<Self> {
        match name {
            "run" => Some(Self::Run),
            "interact" => Some(Self::Interact),
            "stop" => Some(Self::Stop),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Interact => "interact",
            Self::Stop => "stop",
        }
    }

    fn allowed(self) -> &'static [&'static str] {
        match self {
            Self::Run => &[
                "action",
                "command",
                "cwd",
                "profile",
                "shell",
                "tty",
                "yield_time_ms",
                "timeout_ms",
            ],
            Self::Interact => &["action", "session_id", "chars", "yield_time_ms"],
            Self::Stop => &["action", "session_id", "force"],
        }
    }

    fn required(self) -> &'static [&'static str] {
        match self {
            Self::Run => &["action", "command"],
            Self::Interact | Self::Stop => &["action", "session_id"],
        }
    }

    fn conflicts(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::Run => &[("profile", "shell")],
            Self::Interact | Self::Stop => &[],
        }
    }

    fn default_yield_time_ms(self) -> u32 {
        match self {
            Self::Run => DEFAULT_YIELD_TIME_MS,
            Self::Interact => DEFAULT_WAIT_CEILING_MS,
            Self::Stop => 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ShellRequest {
    pub(super) action: Action,
    pub(super) command: Option<String>,
    pub(super) cwd: Option<String>,
    pub(super) profile: Option<Profile>,
    pub(super) has_shell: bool,
    pub(super) tty: bool,
    pub(super) yield_time_ms: u32,
    pub(super) timeout_ms: Option<u64>,
    pub(super) session_id: Option<String>,
    pub(super) chars: Option<String>,
    pub(super) force: bool,
}

impl ShellRequest {
    pub(super) fn has_input(&self) -> bool {
        self.chars.as_deref().is_some_and(|chars| !chars.is_empty())
    }

    pub(super) fn argument_problem(&self) -> Option<&'static str> {
        match self.action {
            Action::Run => {
                let Some(command) = &self.command else {
                    return Some("request.command is required.");
                };
                if command.is_empty() || command.len() > MAX_COMMAND_BYTES {
                    return Some("request.command must contain 1-65536 bytes.");
                }
                if self.timeout_ms == Some(0) {
                    return Some(
                        "request.timeout_ms must be at least 1; choose the intended deadline.",
                    );
                }
                if self.profile.is_some() && self.has_shell {
                    return Some("Choose either request.profile or request.shell.");
                }
                if !self.tty && self.has_shell {
                    return Some(
                        "request.shell requires tty=true; choose the intended execution mode.",
                    );
                }
                if self.yield_time_ms > MAX_YIELD_TIME_MS {
                    return Some("request.yield_time_ms must be between 0 and 30000.");
                }
            }
            Action::Interact => {
                if self.yield_time_ms > MAX_WAIT_CEILING_MS {
                    return Some("request.yield_time_ms must be between 0 and 300000.");
                }
                if self
                    .chars
                    .as_ref()
                    .is_some_and(|chars| chars.len() > MAX_WRITE_BYTES)
                {
                    return Some("request.chars exceed 65536 bytes.");
                }
            }
            Action::Stop => {}
        }
        None
    }
}

pub(super) fn unwrap_request(arguments: &str) -> String {
    if parse_tool_args_object(arguments).is_err() {
        return arguments.to_owned();
    }
    match serde_json::from_str::<Value>(arguments) {
        Ok(Value::Object(fields)) if fields.len() == 1 => match fields.get("request") {
            Some(request @ Value::Object(_)) => request.to_string(),
            _ => arguments.to_owned(),
        },
        _ => arguments.to_owned(),
    }
}

pub(super) fn decode(arguments: &str) -> Result<ShellRequest, String> {
    decode_input(arguments).ok_or_else(|| request_correction(arguments))
}

fn decode_input(arguments: &str) -> Option<ShellRequest> {
    let Value::Object(mut fields) = parse_json(arguments)? else {
        return None;
    };
    let action = Action::parse(fields.get("action")?.as_str()?)?;
    elide_known_null_fields(&mut fields);
    if !FieldCorrection::of(action, &fields).is_empty() {
        return None;
    }
    if let Some(Value::String(text)) = fields.get("shell") {
        let composite = parse_json(text).filter(Value::is_object)?;
        fields.insert("shell".to_owned(), composite);
    }
    let mut request = parse_request(&fields).ok()?;
    if !fields.contains_key("yield_time_ms") {
        request.yield_time_ms = action.default_yield_time_ms();
    }
    request.argument_problem().is_none().then_some(request)
}

fn parse_json(text: &str) -> Option<Value> {
    match parse_tool_args_object(text) {
        Err(ofx_contract::ToolArgsError::InvalidJson) => None,
        _ => serde_json::from_str(text).ok(),
    }
}

fn is_null_placeholder(text: &str) -> bool {
    text.trim_matches(|character: char| character.is_ascii_whitespace())
        .eq_ignore_ascii_case("null")
}

fn elide_known_null_fields(fields: &mut Map<String, Value>) {
    for name in &FIELD_NAMES[1..] {
        let absent = match fields.get(*name) {
            Some(Value::Null) => true,
            Some(Value::String(text)) => is_null_placeholder(text),
            _ => false,
        };
        if absent {
            fields.shift_remove(*name);
        }
    }
}

#[derive(Debug, Default)]
struct FieldCorrection {
    invalid: Vec<String>,
    missing: Vec<&'static str>,
    conflicts: Vec<(&'static str, &'static str)>,
}

impl FieldCorrection {
    fn of(action: Action, fields: &Map<String, Value>) -> Self {
        let mut invalid: Vec<String> = fields
            .keys()
            .filter(|name| !action.allowed().contains(&name.as_str()))
            .cloned()
            .collect();
        invalid.sort();
        Self {
            invalid,
            missing: action
                .required()
                .iter()
                .filter(|name| !fields.contains_key(**name))
                .copied()
                .collect(),
            conflicts: action
                .conflicts()
                .iter()
                .filter(|(first, second)| {
                    fields.contains_key(*first) && fields.contains_key(*second)
                })
                .copied()
                .collect(),
        }
    }

    fn is_empty(&self) -> bool {
        self.invalid.is_empty() && self.missing.is_empty() && self.conflicts.is_empty()
    }
}

fn parse_request(fields: &Map<String, Value>) -> Result<ShellRequest, ()> {
    if fields
        .keys()
        .any(|name| !FIELD_NAMES.contains(&name.as_str()))
    {
        return Err(());
    }
    let action = fields.get("action").ok_or(())?;
    let action = match action {
        Value::String(name) => Action::parse(name),
        Value::Number(number) => number.as_u64().and_then(|index| match index {
            0 => Some(Action::Run),
            1 => Some(Action::Interact),
            2 => Some(Action::Stop),
            _ => None,
        }),
        _ => None,
    }
    .ok_or(())?;
    Ok(ShellRequest {
        action,
        command: optional(fields, "command", string)?,
        cwd: optional(fields, "cwd", string)?,
        profile: optional(fields, "profile", profile)?,
        has_shell: optional(fields, "shell", shell)?.is_some(),
        tty: optional(fields, "tty", boolean)?.unwrap_or(false),
        yield_time_ms: optional(fields, "yield_time_ms", |value| {
            unsigned(value, u64::from(u32::MAX)).and_then(|value| u32::try_from(value).ok())
        })?
        .unwrap_or(DEFAULT_YIELD_TIME_MS),
        timeout_ms: optional(fields, "timeout_ms", |value| unsigned(value, u64::MAX))?,
        session_id: optional(fields, "session_id", string)?,
        chars: optional(fields, "chars", string)?,
        force: optional(fields, "force", boolean)?.unwrap_or(false),
    })
}

fn optional<T>(
    fields: &Map<String, Value>,
    name: &str,
    parse: impl Fn(&Value) -> Option<T>,
) -> Result<Option<T>, ()> {
    match fields.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => parse(value).map(Some).ok_or(()),
    }
}

fn string(value: &Value) -> Option<String> {
    value.as_str().map(str::to_owned)
}

fn boolean(value: &Value) -> Option<bool> {
    value.as_bool()
}

fn profile(value: &Value) -> Option<Profile> {
    let index = match value {
        Value::String(name) if name == "clean" => return Some(Profile::Clean),
        Value::String(name) if name == "user" => return Some(Profile::User),
        Value::String(text) if is_number_formatted_like_an_integer(text) => {
            parse_signed_unsigned(text)?
        }
        Value::Number(number) => number.as_u64()?,
        _ => return None,
    };
    match index {
        0 => Some(Profile::Clean),
        1 => Some(Profile::User),
        _ => None,
    }
}

fn shell(value: &Value) -> Option<()> {
    let Value::Object(members) = value else {
        return None;
    };
    if members
        .keys()
        .any(|name| !SHELL_MEMBERS.contains(&name.as_str()))
    {
        return None;
    }
    let kind = match members.get("kind")? {
        Value::String(name) if name == "executable" => true,
        Value::String(text) if is_number_formatted_like_an_integer(text) => {
            parse_signed_unsigned(text)? == 0
        }
        Value::Number(number) => number.as_u64()? == 0,
        _ => false,
    };
    let path = members.get("path")?.is_string();
    let clean_start = match members.get("clean_start") {
        None | Some(Value::Null | Value::Bool(_)) => true,
        Some(_) => false,
    };
    (kind && path && clean_start).then_some(())
}

fn unsigned(value: &Value, maximum: u64) -> Option<u64> {
    let parsed = match value {
        Value::Number(number) => match (number.as_u64(), number.as_f64()) {
            (Some(integer), _) => Some(integer),
            (None, Some(float)) => integral_float(float),
            (None, None) => None,
        },
        Value::String(text) if is_number_formatted_like_an_integer(text) => {
            parse_signed_unsigned(text)
        }
        Value::String(text) => integral_float(text.parse::<f64>().ok()?),
        _ => None,
    }?;
    (parsed <= maximum).then_some(parsed)
}

fn integral_float(float: f64) -> Option<u64> {
    if float == 0.0 {
        return Some(0);
    }
    if float.fract() != 0.0 || float.is_sign_negative() {
        return None;
    }
    format!("{float:.0}").parse().ok()
}

fn is_number_formatted_like_an_integer(text: &str) -> bool {
    text != "-0" && !text.contains(['.', 'e', 'E'])
}

fn parse_signed_unsigned(text: &str) -> Option<u64> {
    if let Some(digits) = text.strip_prefix('-') {
        return parse_unsigned::<u64>(digits).filter(|value| *value == 0);
    }
    parse_unsigned(text.strip_prefix('+').unwrap_or(text))
}

fn request_correction(arguments: &str) -> String {
    let (mut object, mut problems, mut repairable) = match correction_source(arguments) {
        Ok(source) => source,
        Err(rejected) => return rejected,
    };
    elide_known_null_fields(&mut object);
    let Some(action) = correction_action(&mut object, &mut problems) else {
        return correction_json(&problems, None);
    };
    let correction = FieldCorrection::of(action, &object);
    for name in &correction.invalid {
        problems.push(format!(
            "request.{} is not accepted for {}.",
            utf8_prefix(name, MAX_REPORTED_NAME_BYTES),
            action.name()
        ));
        if object.get(name).is_some_and(|value| !value.is_null()) {
            repairable = false;
        }
        object.shift_remove(name);
    }
    for name in &correction.missing {
        problems.push(format!("request.{name} is required."));
        repairable = false;
    }
    for (first, second) in &correction.conflicts {
        problems.push(format!(
            "Choose either request.{first} or request.{second}."
        ));
        repairable = false;
    }
    let mut canonical = Map::new();
    for name in FIELD_NAMES {
        let Some(original) = object.get(name) else {
            continue;
        };
        let (value, field_repairable) = canonical_field(name, original, &mut problems);
        repairable &= field_repairable;
        canonical.insert(name.to_owned(), value);
    }
    let Ok(candidate) = parse_request(&canonical) else {
        return correction_json(&problems, None);
    };
    if let Some(problem) = candidate.argument_problem() {
        problems.push(problem.to_owned());
        repairable = false;
    }
    if ["tty", "shell", "chars"]
        .iter()
        .any(|name| canonical.contains_key(*name))
    {
        problems.push("Interactive Shell fields require a saved session.".to_owned());
        repairable = false;
    }
    if problems.is_empty() {
        problems.push("Submit one Shell action inside request.".to_owned());
    }
    correction_json(&problems, repairable.then_some(canonical))
}

type CorrectionSource = (Map<String, Value>, Vec<String>, bool);

fn correction_source(arguments: &str) -> Result<CorrectionSource, String> {
    if arguments.len() > MAX_CORRECTION_SOURCE_BYTES {
        return Err(correction_json(
            &["Request is too large to suggest a repair; submit the intended action with only its required fields.".to_owned()],
            None,
        ));
    }
    let Some(raw) = parse_json(arguments) else {
        return Err(correction_json(
            &["Shell arguments must be a JSON object.".to_owned()],
            None,
        ));
    };
    let Value::Object(raw) = raw else {
        return Err(correction_json(
            &["Shell arguments must be one bounded request object.".to_owned()],
            None,
        ));
    };
    if raw.len() > MAX_CORRECTION_FIELDS {
        return Err(correction_json(
            &["Shell arguments must be one bounded request object.".to_owned()],
            None,
        ));
    }
    let mut problems = Vec::new();
    let mut repairable = true;
    let Some(wrapper) = raw.get("request") else {
        return Ok((raw, problems, repairable));
    };
    let mut request = wrapper.clone();
    if let Value::String(text) = wrapper {
        problems.push("request must be an object, not a JSON string.".to_owned());
        let Some(parsed) = parse_json(text) else {
            return Err(correction_json(&problems, None));
        };
        request = parsed;
    }
    let Value::Object(mut object) = request else {
        return Err(correction_json(
            &["request must be one object containing the intended action.".to_owned()],
            None,
        ));
    };
    if object.len() > MAX_CORRECTION_FIELDS {
        return Err(correction_json(
            &["request must be one object containing the intended action.".to_owned()],
            None,
        ));
    }
    if raw.len() > 1 {
        problems.push(
            "Only request is allowed at the top level; put action fields inside request."
                .to_owned(),
        );
        for (name, value) in &raw {
            if name == "request" {
                continue;
            }
            if object.contains_key(name) {
                repairable = false;
            } else {
                object.insert(name.clone(), value.clone());
            }
        }
    }
    Ok((object, problems, repairable))
}

fn correction_action(
    object: &mut Map<String, Value>,
    problems: &mut Vec<String>,
) -> Option<Action> {
    if let Some(value) = object.get("action") {
        let action = value.as_str().and_then(Action::parse);
        if action.is_none() {
            problems.push("request.action must be run, interact, or stop.".to_owned());
        }
        return action;
    }
    problems.push("request.action is required.".to_owned());
    let runs = object.get("command").is_some_and(Value::is_string)
        && !["session_id", "chars", "force"]
            .iter()
            .any(|name| object.contains_key(*name));
    if !runs {
        return None;
    }
    object.insert("action".to_owned(), json!("run"));
    Some(Action::Run)
}

fn canonical_field(name: &str, original: &Value, problems: &mut Vec<String>) -> (Value, bool) {
    let integer_maximum = match name {
        "yield_time_ms" => Some(u64::from(u32::MAX)),
        "timeout_ms" => Some(u64::MAX),
        _ => None,
    };
    let mut value = original.clone();
    let mut repairable = true;
    let mut type_reported = false;
    if let (Some(maximum), Value::String(text)) = (integer_maximum, original) {
        problems.push(format!("request.{name} must be an integer."));
        type_reported = true;
        match parse_signed_unsigned(text).filter(|parsed| *parsed <= maximum) {
            Some(parsed) => value = Value::Number(Number::from(parsed)),
            None => repairable = false,
        }
    }
    let valid = match name {
        "action" => {
            value
                .as_str()
                .is_some_and(|text| Action::parse(text).is_some())
                || value.as_u64().is_some_and(|index| index < 3)
        }
        "command" | "cwd" | "session_id" | "chars" => value.is_string(),
        "profile" => profile(&value).is_some(),
        "shell" => shell(&value).is_some(),
        "tty" | "force" => value.is_boolean(),
        _ => integer_maximum.is_some_and(|maximum| unsigned(&value, maximum).is_some()),
    };
    if valid {
        if let Value::Object(members) = &value {
            value = Value::Object(
                SHELL_MEMBERS
                    .iter()
                    .filter_map(|member| {
                        members
                            .get(*member)
                            .map(|supplied| ((*member).to_owned(), supplied.clone()))
                    })
                    .collect(),
            );
        }
    } else {
        if !type_reported {
            problems.push(format!(
                "request.{name} must be {}.",
                match name {
                    "yield_time_ms" | "timeout_ms" => "an integer",
                    "tty" | "force" => "a boolean",
                    "command" | "cwd" | "session_id" | "chars" => "a string",
                    "action" | "profile" => "an advertised value",
                    _ => "an object matching its schema",
                }
            ));
        }
        repairable = false;
    }
    (value, repairable)
}

fn utf8_prefix(text: &str, max_bytes: usize) -> &str {
    &text[..text.floor_char_boundary(max_bytes.min(text.len()))]
}

fn correction_json(problems: &[String], candidate: Option<Map<String, Value>>) -> String {
    let mut error = Map::new();
    error.insert("code".to_owned(), json!("invalid_shell_request"));
    error.insert("executed".to_owned(), json!(false));
    error.insert("problems".to_owned(), json!(problems));
    if let Some(candidate) = candidate {
        error.insert(
            "instruction".to_owned(),
            json!("Call shell once using retry_with exactly."),
        );
        error.insert(
            "retry_with".to_owned(),
            json!({ "request": Value::Object(candidate) }),
        );
    }
    json!({ "error": Value::Object(error) }).to_string()
}

#[cfg(test)]
mod tests;
