use std::ffi::OsString;

use ofx_auth::parse_login_provider;
use ofx_config::ProviderId;
use ofx_contract::UsageScope;
use ofx_session::{ListScope, ResumeContinuation, is_valid_session_id};
use ofx_text::parse_unsigned;

use super::arg_stream::{ArgStream, ValueForm, merge_toggle, non_blank, requests_json};
use super::failure::{ArgumentErrorCode, CliError};
use crate::command_specs::TopLevelKind;

const SESSION_LIST_MAX_LIMIT: usize = 100;
const SESSION_LIST_DEFAULT_LIMIT: usize = 100;
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

#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub struct SessionListArgs {
    pub format: OutputFormat,
    pub scope: ListScope,
    pub limit: usize,
    pub cursor: Option<ResumeContinuation>,
}

pub(crate) fn parse_session_list(args: Vec<OsString>) -> Result<SessionListArgs, CliError> {
    let error = argument_error(
        TopLevelKind::Sessions,
        ArgumentErrorCode::LocalSurface,
        &args,
    );
    let mut stream = ArgStream::new(args);
    let mut json = false;
    let mut all = false;
    let mut limit = None;
    let mut cursor = None;
    while stream.peek().is_some() {
        let repeated = if stream.take_flag("--json") {
            std::mem::replace(&mut json, true)
        } else if stream.take_flag("--all") {
            std::mem::replace(&mut all, true)
        } else if let Some(value) = stream.take_option("limit", ValueForm::Separate) {
            let parsed = value
                .ok()
                .and_then(|value| value.to_str().and_then(session_limit))
                .ok_or_else(&error)?;
            limit.replace(parsed).is_some()
        } else if let Some(value) = stream.take_option("cursor", ValueForm::Separate) {
            let parsed = value
                .ok()
                .and_then(|value| value.to_str().and_then(session_cursor))
                .ok_or_else(&error)?;
            cursor.replace(parsed).is_some()
        } else {
            return Err(error());
        };
        if repeated {
            return Err(error());
        }
    }
    Ok(SessionListArgs {
        format: format_for(json),
        scope: if all {
            ListScope::AllWorkspaces
        } else {
            ListScope::CurrentWorkspace
        },
        limit: limit.unwrap_or(SESSION_LIST_DEFAULT_LIMIT),
        cursor,
    })
}

fn format_for(json: bool) -> OutputFormat {
    if json {
        OutputFormat::Json
    } else {
        OutputFormat::Text
    }
}

fn session_limit(raw: &str) -> Option<usize> {
    parse_unsigned(raw).filter(|limit: &usize| (1..=SESSION_LIST_MAX_LIMIT).contains(limit))
}

fn session_cursor(raw: &str) -> Option<ResumeContinuation> {
    if raw.is_empty() || raw.len() > SESSION_CURSOR_MAX_BYTES {
        return None;
    }
    let fields: Vec<&str> = raw.split(':').collect();
    let ["v1", updated, id] = fields.as_slice() else {
        return None;
    };
    let updated_at_ms = updated.parse::<i64>().ok()?;
    (is_valid_session_id(id) && format!("v1:{updated_at_ms}:{id}") == raw).then(|| {
        ResumeContinuation {
            updated_at_ms,
            id: (*id).to_owned(),
        }
    })
}

#[derive(Debug, Clone, Copy)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub struct UsageArgs {
    pub format: OutputFormat,
    pub scope: UsageScope,
}

pub(crate) fn parse_usage(args: Vec<OsString>) -> Result<UsageArgs, CliError> {
    let error = argument_error(TopLevelKind::Usage, ArgumentErrorCode::Usage, &args);
    let mut stream = ArgStream::new(args);
    let mut json = false;
    let mut period = false;
    let mut scope = UsageScope::Days30;
    while stream.peek().is_some() {
        let seen = if stream.take_flag("--json") {
            &mut json
        } else if let Some(value) = stream.take_option("period", ValueForm::Separate) {
            scope = value
                .ok()
                .and_then(|value| value.to_str().and_then(UsageScope::parse_cli_value))
                .ok_or_else(&error)?;
            &mut period
        } else {
            return Err(error());
        };
        if std::mem::replace(seen, true) {
            return Err(error());
        }
    }
    Ok(UsageArgs {
        format: format_for(json),
        scope,
    })
}

#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub enum WorkspaceAction {
    Add(OsString),
    Remove(OsString),
    Clear,
}

#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub struct WorkspaceArgs {
    pub format: OutputFormat,
    pub action: Option<WorkspaceAction>,
}

pub(crate) fn parse_workspace(args: Vec<OsString>) -> Result<WorkspaceArgs, CliError> {
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
    let action = match positional.as_slice() {
        [] => None,
        [action] if action == "list" => None,
        [action] if action == "clear" => Some(WorkspaceAction::Clear),
        [action, path] if action == "add" && !path.is_empty() => {
            Some(WorkspaceAction::Add(path.clone()))
        }
        [action, path] if action == "remove" && !path.is_empty() => {
            Some(WorkspaceAction::Remove(path.clone()))
        }
        _ => return Err(error()),
    };
    Ok(WorkspaceArgs {
        format: format_for(json),
        action,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionTarget {
    Last,
    Id(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionAction {
    Detail(SessionTarget),
    Migrate,
    Recover,
}

#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub struct SessionArgs {
    pub format: OutputFormat,
    pub action: SessionAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionVerb {
    Detail,
    Migrate,
    Recover,
}

impl SessionVerb {
    fn error_code(self) -> ArgumentErrorCode {
        match self {
            Self::Detail => ArgumentErrorCode::SessionDetail,
            Self::Migrate => ArgumentErrorCode::SessionMigration,
            Self::Recover => ArgumentErrorCode::SessionRecovery,
        }
    }

    fn action(self, operand: SessionOperand) -> SessionAction {
        match self {
            Self::Detail if !operand.exact && operand.value == "last" => {
                SessionAction::Detail(SessionTarget::Last)
            }
            Self::Detail => SessionAction::Detail(SessionTarget::Id(operand.value)),
            Self::Migrate => SessionAction::Migrate,
            Self::Recover => SessionAction::Recover,
        }
    }
}

struct SessionOperand {
    value: String,
    exact: bool,
}

pub(crate) fn parse_session(args: Vec<OsString>) -> Result<SessionArgs, CliError> {
    let verb = match args.first() {
        Some(first) if first == "recover" => SessionVerb::Recover,
        Some(first) if first == "migrate" => SessionVerb::Migrate,
        _ => SessionVerb::Detail,
    };
    let error = argument_error(TopLevelKind::Session, verb.error_code(), &args);
    let mut stream = ArgStream::new(args);
    if verb != SessionVerb::Detail {
        stream.next();
    }
    let mut json = false;
    let mut operand = None;
    while stream.peek().is_some() {
        if stream.take_flag("--json") {
            json = true;
            continue;
        }
        if verb == SessionVerb::Migrate && stream.take_flag("--allow-large") {
            continue;
        }
        if operand.is_some() {
            return Err(error());
        }
        operand = Some(take_session_operand(&mut stream).ok_or_else(&error)?);
    }
    let operand = operand.ok_or_else(error)?;
    Ok(SessionArgs {
        format: format_for(json),
        action: verb.action(operand),
    })
}

fn take_session_operand(stream: &mut ArgStream) -> Option<SessionOperand> {
    let exact = stream.take_flag("--id");
    let raw = stream.next()?;
    let value = non_blank(&raw)?.to_string_lossy().into_owned();
    Some(SessionOperand { value, exact })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpOperation {
    Auth(String),
    Unported,
}

pub(crate) fn parse_mcp(args: &[OsString]) -> Result<McpOperation, CliError> {
    let usage = || CliError::Usage(TopLevelKind::Mcp);
    let [operation, rest @ ..] = args else {
        return Err(usage());
    };
    let named = |tokens: &[OsString]| matches!(tokens, [name] if !name.is_empty());
    match (operation.to_str(), rest) {
        (Some("auth"), [name]) if !name.is_empty() => {
            Ok(McpOperation::Auth(name.to_string_lossy().into_owned()))
        }
        (Some("auth"), _) => Err(CliError::McpAuthUsage),
        (Some("add"), _) => validate_mcp_add(rest).map(|()| McpOperation::Unported),
        (Some("trust"), _) if is_valid_mcp_trust(rest) => Ok(McpOperation::Unported),
        (Some("path"), []) => Ok(McpOperation::Unported),
        (Some("list"), _) if rest.is_empty() || matches!(rest, [flag] if flag == "--connect") => {
            Ok(McpOperation::Unported)
        }
        (Some("remove" | "logout"), _) if named(rest) => Ok(McpOperation::Unported),
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
