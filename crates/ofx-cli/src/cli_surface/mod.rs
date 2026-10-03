mod arg_stream;
mod command_args;
mod failure;
mod launch_modifiers;
mod model_overrides;
mod resume;
mod workflow_args;

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;

use ofx_config::ProviderId;

use crate::cli_ask::{AskArgs, parse_ask};
use crate::cli_replay::parse_replay;
use crate::command_specs::TopLevelKind;

pub(crate) use arg_stream::{ArgStream, ValueForm, non_blank, requests_json};
pub use command_args::OutputFormat;
pub(crate) use failure::Report;
pub use failure::{CliError, command_failure_json};
pub use launch_modifiers::LaunchModifiers;
pub(crate) use model_overrides::{ModelOverride, ModelOverrides};

use launch_modifiers::parse_launch_modifiers;
use resume::{
    InvalidResumeArgs, RESUME_ID_ALIAS_PREFIX, resume_alias_target, resume_subcommand_target,
};
pub use resume::{RequestedResume, UPGRADE_RELAUNCH_ARG};
pub use workflow_args::WorkflowArgs;
use workflow_args::parse_workflow_args;

#[derive(Debug)]
pub enum Invocation {
    Interactive(LaunchModifiers),
    Resume(LaunchModifiers, RequestedResume),
    TopLevelHelp(HelpLayout),
    CommandHelp(TopLevelKind),
    Version,
    Command(CommandLaunch),
}

#[derive(Debug, Clone, Copy)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub enum HelpLayout {
    Terminal,
    Plain,
}

#[derive(Debug)]
pub struct CommandLaunch {
    pub modifiers: LaunchModifiers,
    pub command: Command,
}

#[derive(Debug)]
pub enum Command {
    Ask(AskArgs),
    Acp,
    Pr(WorkflowArgs),
    Issue(WorkflowArgs),
    Login(Option<ProviderId>),
    Logout(Option<ProviderId>),
    Setup,
    Status(OutputFormat),
    Permissions(OutputFormat),
    Mcp,
    Models(OutputFormat),
    Provider(ProviderId),
    Doctor(OutputFormat),
    Teams,
    Session(OutputFormat),
    Sessions(OutputFormat),
    Credits(OutputFormat),
    Usage(OutputFormat),
    Upgrade(OutputFormat),
    Replay(OutputFormat),
    Workspace(OutputFormat),
}

impl Command {
    pub fn kind(&self) -> TopLevelKind {
        match self {
            Self::Ask(_) => TopLevelKind::Ask,
            Self::Acp => TopLevelKind::Acp,
            Self::Pr(_) => TopLevelKind::Pr,
            Self::Issue(_) => TopLevelKind::Issue,
            Self::Login(_) => TopLevelKind::Login,
            Self::Logout(_) => TopLevelKind::Logout,
            Self::Setup => TopLevelKind::Setup,
            Self::Status(_) => TopLevelKind::Status,
            Self::Permissions(_) => TopLevelKind::Permissions,
            Self::Mcp => TopLevelKind::Mcp,
            Self::Models(_) => TopLevelKind::Models,
            Self::Provider(_) => TopLevelKind::Provider,
            Self::Doctor(_) => TopLevelKind::Doctor,
            Self::Teams => TopLevelKind::Teams,
            Self::Session(_) => TopLevelKind::Session,
            Self::Sessions(_) => TopLevelKind::Sessions,
            Self::Credits(_) => TopLevelKind::Credits,
            Self::Usage(_) => TopLevelKind::Usage,
            Self::Upgrade(_) => TopLevelKind::Upgrade,
            Self::Replay(_) => TopLevelKind::Replay,
            Self::Workspace(_) => TopLevelKind::Workspace,
        }
    }

    pub fn output_format(&self) -> OutputFormat {
        match self {
            Self::Status(format)
            | Self::Permissions(format)
            | Self::Models(format)
            | Self::Doctor(format)
            | Self::Session(format)
            | Self::Sessions(format)
            | Self::Credits(format)
            | Self::Usage(format)
            | Self::Upgrade(format)
            | Self::Replay(format)
            | Self::Workspace(format) => *format,
            Self::Ask(args) if args.output.json => OutputFormat::Json,
            Self::Ask(_)
            | Self::Acp
            | Self::Pr(_)
            | Self::Issue(_)
            | Self::Login(_)
            | Self::Logout(_)
            | Self::Setup
            | Self::Mcp
            | Self::Provider(_)
            | Self::Teams => OutputFormat::Text,
        }
    }
}

pub fn parse_args<I, T>(args: I) -> Result<Invocation, CliError>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
    if args
        .first()
        .and_then(|first| first.to_str())
        .is_some_and(|first| TopLevelKind::Help.spec().matches(first))
    {
        return Ok(Invocation::TopLevelHelp(HelpLayout::Terminal));
    }
    let mut stream = ArgStream::new(args);
    let modifiers = parse_launch_modifiers(&mut stream)?;
    let Some(first) = stream.next() else {
        return Ok(Invocation::Interactive(modifiers));
    };
    let rest: Vec<OsString> = stream.collect();
    match first.to_str().and_then(TopLevelKind::from_token) {
        Some(kind) if kind != TopLevelKind::Help && requests_command_help(&rest) => {
            let workspace = supports_workspace_modifiers(kind) || launches_session(kind, &rest);
            check_noninteractive(&modifiers, workspace, Some(kind))?;
            Ok(Invocation::CommandHelp(kind))
        }
        Some(kind) => parse_command(kind, &first, rest, modifiers),
        None => parse_unclassified(first, &rest, modifiers),
    }
}

fn requests_command_help(args: &[OsString]) -> bool {
    args.iter().any(|arg| arg == "--help" || arg == "-h")
}

fn check_noninteractive(
    modifiers: &LaunchModifiers,
    supports_workspace_modifiers: bool,
    kind: Option<TopLevelKind>,
) -> Result<(), CliError> {
    if modifiers.has_workspace_modifiers() && !supports_workspace_modifiers {
        return Err(CliError::WorkspaceModifiersUnsupported);
    }
    let acp_ultrafast = kind == Some(TopLevelKind::Acp) && modifiers.has_only_ultrafast_override();
    if modifiers.has_model_overrides() && !acp_ultrafast {
        return Err(CliError::ModelModifiersUnsupported);
    }
    Ok(())
}

fn supports_workspace_modifiers(kind: TopLevelKind) -> bool {
    matches!(
        kind,
        TopLevelKind::Ask | TopLevelKind::Acp | TopLevelKind::Pr | TopLevelKind::Issue
    )
}

fn launches_session(kind: TopLevelKind, rest: &[OsString]) -> bool {
    match kind {
        TopLevelKind::Resume => true,
        TopLevelKind::Session => rest.first().is_some_and(|arg| arg == "resume"),
        _ => false,
    }
}

fn launch_session(
    request: Result<RequestedResume, InvalidResumeArgs>,
    modifiers: LaunchModifiers,
) -> Result<Invocation, CliError> {
    let target = request.map_err(|InvalidResumeArgs| CliError::Usage(TopLevelKind::Resume))?;
    Ok(Invocation::Resume(modifiers, target))
}

fn parse_unclassified(
    first: OsString,
    rest: &[OsString],
    modifiers: LaunchModifiers,
) -> Result<Invocation, CliError> {
    if first
        .as_bytes()
        .starts_with(RESUME_ID_ALIAS_PREFIX.as_bytes())
    {
        return launch_session(resume_alias_target(&first, rest), modifiers);
    }
    check_noninteractive(&modifiers, false, None)?;
    if first != "--version" && first != "-v" {
        return Err(CliError::UnknownSubcommand(first));
    }
    if rest.is_empty() {
        Ok(Invocation::Version)
    } else {
        Err(CliError::VersionUsage)
    }
}

fn parse_command(
    kind: TopLevelKind,
    first: &OsStr,
    rest: Vec<OsString>,
    modifiers: LaunchModifiers,
) -> Result<Invocation, CliError> {
    let session = launches_session(kind, &rest);
    if !session {
        check_noninteractive(&modifiers, supports_workspace_modifiers(kind), Some(kind))?;
    }
    let command = match kind {
        TopLevelKind::Help => return Ok(Invocation::TopLevelHelp(HelpLayout::Plain)),
        TopLevelKind::Resume if first.as_bytes().starts_with(b"-") => {
            return launch_session(resume_alias_target(first, &rest), modifiers);
        }
        TopLevelKind::Resume => {
            return launch_session(resume_subcommand_target(&rest), modifiers);
        }
        TopLevelKind::Session if session => {
            return launch_session(resume_subcommand_target(&rest[1..]), modifiers);
        }
        TopLevelKind::Mcp if rest.is_empty() => {
            return Ok(Invocation::CommandHelp(kind));
        }
        TopLevelKind::Ask => Command::Ask(parse_ask(rest)?),
        TopLevelKind::Acp => command_args::validate_acp(rest).map(|()| Command::Acp)?,
        TopLevelKind::Pr => Command::Pr(parse_workflow_args(rest)),
        TopLevelKind::Issue => Command::Issue(parse_workflow_args(rest)),
        TopLevelKind::Login => Command::Login(command_args::parse_login(kind, &rest)?),
        TopLevelKind::Logout => Command::Logout(command_args::parse_login(kind, &rest)?),
        TopLevelKind::Setup => {
            command_args::require_no_args(kind, &rest).map(|()| Command::Setup)?
        }
        TopLevelKind::Teams => {
            command_args::require_no_args(kind, &rest).map(|()| Command::Teams)?
        }
        TopLevelKind::Status => Command::Status(command_args::parse_output_format(kind, &rest)?),
        TopLevelKind::Permissions => {
            Command::Permissions(command_args::parse_output_format(kind, &rest)?)
        }
        TopLevelKind::Models => Command::Models(command_args::parse_output_format(kind, &rest)?),
        TopLevelKind::Doctor => Command::Doctor(command_args::parse_output_format(kind, &rest)?),
        TopLevelKind::Credits => Command::Credits(command_args::parse_output_format(kind, &rest)?),
        TopLevelKind::Mcp => command_args::validate_mcp(&rest).map(|()| Command::Mcp)?,
        TopLevelKind::Provider => Command::Provider(command_args::parse_provider(&rest)?),
        TopLevelKind::Session => Command::Session(command_args::parse_session(rest)?),
        TopLevelKind::Sessions => Command::Sessions(command_args::parse_session_list(rest)?),
        TopLevelKind::Usage => Command::Usage(command_args::parse_usage(rest)?),
        TopLevelKind::Upgrade => Command::Upgrade(command_args::parse_upgrade(&rest)?),
        TopLevelKind::Replay => Command::Replay(parse_replay(rest)?),
        TopLevelKind::Workspace => Command::Workspace(command_args::parse_workspace(rest)?),
    };
    Ok(Invocation::Command(CommandLaunch { modifiers, command }))
}

#[cfg(test)]
mod tests;
