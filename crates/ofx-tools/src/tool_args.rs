use ofx_contract::{ToolArgValue, ToolArgs, ToolArgsError, ToolOutput, parse_tool_args_object};

pub(crate) fn parse_arguments(tool_name: &str, args_json: &str) -> Result<ToolArgs, ToolOutput> {
    parse_tool_args_object(args_json).map_err(|error| {
        ToolOutput::failure(match error {
            ToolArgsError::InvalidJson => format!("{tool_name} arguments must be valid JSON"),
            ToolArgsError::NotObject => format!("{tool_name} arguments must be an object"),
        })
    })
}

pub(crate) fn required_string(
    tool_name: &str,
    arguments: &ToolArgs,
    key: &str,
) -> Result<String, ToolOutput> {
    optional_string(tool_name, arguments, key)?
        .ok_or_else(|| ToolOutput::failure(format!("{tool_name} requires string field \"{key}\"")))
}

pub(crate) fn optional_string(
    tool_name: &str,
    arguments: &ToolArgs,
    key: &str,
) -> Result<Option<String>, ToolOutput> {
    match arguments.get(key) {
        None => Ok(None),
        Some(ToolArgValue::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(ToolOutput::failure(format!(
            "{tool_name} field \"{key}\" must be a string"
        ))),
    }
}

pub(crate) fn optional_integer(
    tool_name: &str,
    arguments: &ToolArgs,
    key: &str,
    minimum: i64,
    description: &str,
) -> Result<Option<usize>, ToolOutput> {
    if arguments.get(key).is_none() {
        return Ok(None);
    }
    arguments
        .optional_int(key)
        .filter(|integer| *integer >= minimum)
        .and_then(|integer| usize::try_from(integer).ok())
        .map(Some)
        .ok_or_else(|| {
            ToolOutput::failure(format!(
                "{tool_name} field \"{key}\" must be a {description} integer"
            ))
        })
}
