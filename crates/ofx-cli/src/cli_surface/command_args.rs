use std::ffi::{OsStr, OsString};

use ofx_auth::parse_login_provider;
use ofx_config::ProviderId;
use ofx_session::is_valid_session_id;
use ofx_text::parse_unsigned;

use super::arg_stream::{ArgStream, ValueForm, merge_toggle, non_blank, requests_json};
use super::failure::{ArgumentErrorCode, CliError};
use crate::command_specs::TopLevelKind;

const SESSION_LIST_MAX_LIMIT: usize = 100;
const SESSION_CURSOR_MAX_BYTES: usize = 320;

#[derive(Debug, Clone, Copy, Default)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
}

fn argument_error(
    command: TopLevelKind,
    code: ArgumentErrorCode,
    args: &[OsString],
) -> impl Fn() -> CliError + use<> {
    let json = requests_json(args);
    move || CliError::invalid_arguments(command, code, json)
}

pub(crate) fn parse_output_format(
    command: TopLevelKind,
    args: &[OsString],
) -> Result<OutputFormat, CliError> {
    let error = argument_error(command, ArgumentErrorCode::LocalSurface, args);
    match args {
        [] => Ok(OutputFormat::Text),
        _ if args.iter().all(|arg| arg == "--json") => Ok(OutputFormat::Json),
        _ => Err(error()),
    }
}

pub(crate) fn validate_acp(args: Vec<OsString>) -> Result<(), CliError> {
    let mut stream = ArgStream::new(args);
    let mut model = false;
    let mut log_file = false;
    let mut ultrafast = None;
    let invalid = || CliError::Usage(TopLevelKind::Acp);
    while stream.peek().is_some() {
        if let Some(enabled) = stream.take_toggle("--ultrafast", "--no-ultrafast") {
            ultrafast = Some(merge_toggle(ultrafast, enabled, invalid())?);
            continue;
        }
        let seen = if let Some(value) = stream.take_option("model", ValueForm::Separate) {
            value.map_err(|_| invalid())?;
            &mut model
        } else if let Some(value) = stream.take_option("log-file", ValueForm::Separate) {
            value.map_err(|_| invalid())?;
            &mut log_file
        } else {
            return Err(invalid());
        };
        if std::mem::replace(seen, true) {
            return Err(invalid());
        }
    }
    Ok(())
}

pub(crate) fn parse_login(
    command: TopLevelKind,
    args: &[OsString],
) -> Result<Option<ProviderId>, CliError> {
    match args {
        [] => Ok(None),
        [provider] => provider
            .to_str()
            .and_then(parse_login_provider)
            .map(Some)
            .ok_or(CliError::Usage(command)),
        _ => Err(CliError::Usage(command)),
    }
}

pub(crate) fn parse_provider(args: &[OsString]) -> Result<ProviderId, CliError> {
    let [name] = args else {
        return Err(CliError::Usage(TopLevelKind::Provider));
    };
    name.to_str()
        .and_then(ProviderId::parse)
        .ok_or(CliError::UnknownProvider)
}

pub(crate) fn require_no_args(command: TopLevelKind, args: &[OsString]) -> Result<(), CliError> {
    if args.is_empty() {
        Ok(())
    } else {
        Err(CliError::Usage(command))
    }
}

pub(crate) fn parse_upgrade(args: &[OsString]) -> Result<OutputFormat, CliError> {
    match args {
        [] => Ok(OutputFormat::Text),
        [flag] if flag == "--json" => Ok(OutputFormat::Json),
        _ => Err(CliError::invalid_arguments(
            TopLevelKind::Upgrade,
            ArgumentErrorCode::Upgrade,
            requests_json(args),
        )),
    }
}

pub(crate) fn parse_session_list(args: Vec<OsString>) -> Result<OutputFormat, CliError> {
    let error = argument_error(
        TopLevelKind::Sessions,
        ArgumentErrorCode::LocalSurface,
        &args,
    );
    let mut stream = ArgStream::new(args);
    let mut json = false;
    let mut all = false;
    let mut limit = false;
    let mut cursor = false;
    while stream.peek().is_some() {
        let seen = if stream.take_flag("--json") {
            &mut json
        } else if stream.take_flag("--all") {
            &mut all
        } else if let Some(value) = stream.take_option("limit", ValueForm::Separate) {
            if !value.is_ok_and(|value| text_matches(&value, is_valid_session_limit)) {
                return Err(error());
            }
            &mut limit
        } else if let Some(value) = stream.take_option("cursor", ValueForm::Separate) {
            if !value.is_ok_and(|value| text_matches(&value, is_valid_session_cursor)) {
                return Err(error());
            }
            &mut cursor
        } else {
            return Err(error());
        };
        if std::mem::replace(seen, true) {
            return Err(error());
        }
    }
    Ok(format_for(json))
}

fn text_matches(value: &OsStr, valid: impl FnOnce(&str) -> bool) -> bool {
    value.to_str().is_some_and(valid)
}

fn format_for(json: bool) -> OutputFormat {
    if json {
        OutputFormat::Json
    } else {
        OutputFormat::Text
    }
}

fn is_valid_session_limit(raw: &str) -> bool {
    parse_unsigned(raw).is_some_and(|limit: usize| (1..=SESSION_LIST_MAX_LIMIT).contains(&limit))
}

fn is_valid_session_cursor(raw: &str) -> bool {
    if raw.is_empty() || raw.len() > SESSION_CURSOR_MAX_BYTES {
        return false;
    }
    let fields: Vec<&str> = raw.split(':').collect();
    let ["v1", updated, id] = fields.as_slice() else {
        return false;
    };
    is_valid_session_id(id)
        && updated
            .parse::<i64>()
            .is_ok_and(|updated_at_ms| format!("v1:{updated_at_ms}:{id}") == raw)
}

pub(crate) fn parse_usage(args: Vec<OsString>) -> Result<OutputFormat, CliError> {
    let error = argument_error(TopLevelKind::Usage, ArgumentErrorCode::Usage, &args);
    let mut stream = ArgStream::new(args);
    let mut json = false;
    let mut period = false;
    while stream.peek().is_some() {
        let seen = if stream.take_flag("--json") {
            &mut json
        } else if let Some(value) = stream.take_option("period", ValueForm::Separate) {
            if !value.is_ok_and(|value| matches!(value.to_str(), Some("24h" | "7d" | "30d"))) {
                return Err(error());
            }
            &mut period
        } else {
            return Err(error());
        };
        if std::mem::replace(seen, true) {
            return Err(error());
        }
    }
    Ok(format_for(json))
}

pub(crate) fn parse_workspace(args: Vec<OsString>) -> Result<OutputFormat, CliError> {
    let error = argument_error(TopLevelKind::Workspace, ArgumentErrorCode::Workspace, &args);
    let mut json = false;
    let mut positional = Vec::new();
    for arg in args {
        if arg == "--json" {
            if std::mem::replace(&mut json, true) {
                return Err(error());
            }
        } else if positional.len() >= 2 {
            return Err(error());
        } else {
            positional.push(arg);
        }
    }
    match positional.as_slice() {
        [] => {}
        [action] if action == "list" || action == "clear" => {}
        [action, path] if (action == "add" || action == "remove") && !path.is_empty() => {}
        _ => return Err(error()),
    }
    Ok(format_for(json))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionAction {
    Detail,
    Migrate,
    Recover,
}

impl SessionAction {
    fn error_code(self) -> ArgumentErrorCode {
        match self {
            Self::Detail => ArgumentErrorCode::SessionDetail,
            Self::Migrate => ArgumentErrorCode::SessionMigration,
            Self::Recover => ArgumentErrorCode::SessionRecovery,
        }
    }
}

pub(crate) fn parse_session(args: Vec<OsString>) -> Result<OutputFormat, CliError> {
    let action = match args.first() {
        Some(first) if first == "recover" => SessionAction::Recover,
        Some(first) if first == "migrate" => SessionAction::Migrate,
        _ => SessionAction::Detail,
    };
    let error = argument_error(TopLevelKind::Session, action.error_code(), &args);
    let mut stream = ArgStream::new(args);
    if action != SessionAction::Detail {
        stream.next();
    }
    let mut json = false;
    let mut operand = false;
    while stream.peek().is_some() {
        if stream.take_flag("--json") {
            json = true;
            continue;
        }
        if action == SessionAction::Migrate && stream.take_flag("--allow-large") {
            continue;
        }
        if operand || !take_session_operand(&mut stream) {
            return Err(error());
        }
        operand = true;
    }
    if operand {
        Ok(format_for(json))
    } else {
        Err(error())
    }
}

fn take_session_operand(stream: &mut ArgStream) -> bool {
    stream.take_flag("--id");
    stream.next().is_some_and(|raw| non_blank(&raw).is_some())
}

pub(crate) fn validate_mcp(args: &[OsString]) -> Result<(), CliError> {
    let usage = || CliError::Usage(TopLevelKind::Mcp);
    let [operation, rest @ ..] = args else {
        return Err(usage());
    };
    let named = |tokens: &[OsString]| matches!(tokens, [name] if !name.is_empty());
    match operation.to_str() {
        Some("add") => validate_mcp_add(rest),
        Some("trust") if is_valid_mcp_trust(rest) => Ok(()),
        Some("path") if rest.is_empty() => Ok(()),
        Some("list") if rest.is_empty() || matches!(rest, [flag] if flag == "--connect") => Ok(()),
        Some("remove" | "logout" | "auth") if named(rest) => Ok(()),
        Some("auth") => Err(CliError::McpAuthUsage),
        _ => Err(usage()),
    }
}

fn validate_mcp_add(tokens: &[OsString]) -> Result<(), CliError> {
    match tokens {
        [transport, rest @ ..] if transport == "--transport" => match rest {
            [kind, _, _] if kind == "http" => Ok(()),
            _ => Err(CliError::McpAddUsage),
        },
        [_, _, ..] => Ok(()),
        _ => Err(CliError::McpAddUsage),
    }
}

fn is_valid_mcp_trust(tokens: &[OsString]) -> bool {
    match tokens {
        [action] => action == "approve-all" || action == "reset",
        [action, name] => (action == "approve" || action == "reject") && !name.is_empty(),
        _ => false,
    }
}

#[cfg(test)]
mod tests;
