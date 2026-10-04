use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;

use ofx_text::encode_terminal_safe;

use super::launch_modifiers::GlobalLaunchError;
use crate::cli_ask::AskError;
use crate::cli_replay::ReplayError;
use crate::command_specs::{
    HelpStyle, PRODUCT_NAME, TOP_LEVEL_HELP_DEFAULT_WIDTH, TopLevelKind, render_top_level_help,
};

const GLOBAL_LAUNCH_USAGE: &str = "usage: oh-fx [--context-limit NAME=BYTES|off] [--add-dir PATH]... [--no-additional-dirs] [--provider <name>] [--model <id>] [--effort <level>] [--fast|--no-fast] [--ultrafast|--no-ultrafast] [--provider-order <a,b,...>] [--provider-strict|--no-provider-strict] <command>";
const ECHOED_ARGUMENT_BYTES: usize = 160;

#[derive(Debug, Clone, Copy)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub enum ArgumentErrorCode {
    LocalSurface,
    Usage,
    SessionDetail,
    SessionMigration,
    SessionRecovery,
    Workspace,
    Upgrade,
}

impl ArgumentErrorCode {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::LocalSurface => "InvalidLocalSurfaceArgs",
            Self::Usage => "InvalidUsageArgs",
            Self::SessionDetail => "InvalidSessionDetailArgs",
            Self::SessionMigration => "InvalidSessionMigrationArgs",
            Self::SessionRecovery => "InvalidSessionRecoveryArgs",
            Self::Workspace => "InvalidWorkspaceArgs",
            Self::Upgrade => "InvalidUpgradeArgs",
        }
    }

    fn has_json_failure(self) -> bool {
        !matches!(self, Self::Upgrade)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("unknown subcommand: {}", terminal_safe(.0))]
    UnknownSubcommand(OsString),
    #[error(transparent)]
    GlobalLaunch(#[from] GlobalLaunchError),
    #[error(
        "--add-dir and --no-additional-dirs are only supported for interactive, resume, ask, ACP, PR, and issue launches"
    )]
    WorkspaceModifiersUnsupported,
    #[error(
        "--provider, --model, --effort, --fast, --ultrafast, --provider-order, and --provider-strict apply to interactive sessions; for one-shot runs pass model flags after `oh-fx ask`"
    )]
    ModelModifiersUnsupported,
    #[error("usage: oh-fx --version")]
    VersionUsage,
    #[error("usage: oh-fx {}", .0.spec().usage())]
    Usage(TopLevelKind),
    #[error("invalid arguments")]
    InvalidArguments {
        command: TopLevelKind,
        code: ArgumentErrorCode,
    },
    #[error("{}", .0.name())]
    Unhandled(ArgumentErrorCode),
    #[error("expected gateway, codex, grok, or a configured name")]
    UnknownProvider,
    #[error(
        "usage: oh-fx mcp add NAME COMMAND [ARGS...] | oh-fx mcp add --transport http NAME URL"
    )]
    McpAddUsage,
    #[error("usage: oh-fx mcp auth NAME")]
    McpAuthUsage,
    #[error(transparent)]
    Ask(#[from] AskError),
    #[error(transparent)]
    Replay(#[from] ReplayError),
}

#[derive(Debug)]
pub struct Report {
    pub stdout: String,
    pub stderr: String,
}

impl Report {
    pub(crate) fn stdout(text: String) -> Self {
        Self {
            stdout: text,
            stderr: String::new(),
        }
    }

    pub(crate) fn stderr(text: String) -> Self {
        Self {
            stdout: String::new(),
            stderr: text,
        }
    }
}

impl CliError {
    pub fn report(&self, version: &str) -> Report {
        match self {
            Self::UnknownSubcommand(_) => Report::stderr(format!(
                "{PRODUCT_NAME}: {self}\n\n{}",
                render_top_level_help(TOP_LEVEL_HELP_DEFAULT_WIDTH, version, HelpStyle::Plain)
            )),
            Self::GlobalLaunch(error) => {
                Report::stderr(format!("{PRODUCT_NAME}: {error}\n{GLOBAL_LAUNCH_USAGE}\n"))
            }
            Self::WorkspaceModifiersUnsupported
            | Self::ModelModifiersUnsupported
            | Self::Unhandled(_) => Report::stderr(format!("{PRODUCT_NAME}: {self}\n")),
            Self::UnknownProvider => Report::stderr(format!("{PRODUCT_NAME} provider: {self}\n")),
            Self::VersionUsage | Self::Usage(_) | Self::McpAddUsage | Self::McpAuthUsage => {
                Report::stderr(format!("{self}\n"))
            }
            Self::Ask(error) => error.report(),
            Self::Replay(error) => error.report(),
            Self::InvalidArguments { command, code } => Report::stdout(command_failure_json(
                *command,
                "invalid arguments",
                code.name(),
            )),
        }
    }

    pub(crate) fn invalid_arguments(
        command: TopLevelKind,
        code: ArgumentErrorCode,
        json_requested: bool,
    ) -> Self {
        if !json_requested {
            Self::Usage(command)
        } else if code.has_json_failure() {
            Self::InvalidArguments { command, code }
        } else {
            Self::Unhandled(code)
        }
    }
}

fn terminal_safe(argument: &OsStr) -> String {
    encode_terminal_safe(argument.as_bytes(), ECHOED_ARGUMENT_BYTES).text
}

pub fn command_failure_json(command: TopLevelKind, message: &str, code: &str) -> String {
    let mut line =
        serde_json::json!({ "kind": command.token(), "error": message, "code": code }).to_string();
    line.push('\n');
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_failures_render_one_json_line() {
        assert_eq!(
            command_failure_json(
                TopLevelKind::Status,
                "invalid arguments",
                "InvalidLocalSurfaceArgs"
            ),
            "{\"kind\":\"status\",\"error\":\"invalid arguments\",\"code\":\"InvalidLocalSurfaceArgs\"}\n"
        );
        assert_eq!(
            command_failure_json(TopLevelKind::Replay, "a\"b\\c\nd\u{1}", "X"),
            "{\"kind\":\"replay\",\"error\":\"a\\\"b\\\\c\\nd\\u0001\",\"code\":\"X\"}\n"
        );
    }

    #[test]
    fn unknown_subcommands_echo_a_terminal_safe_bounded_token() {
        let echoed = |raw: &[u8]| {
            CliError::UnknownSubcommand(OsStr::from_bytes(raw).to_os_string()).to_string()
        };
        assert_eq!(echoed(b"wat"), "unknown subcommand: wat");
        assert_eq!(
            echoed(b"\x1b]0;pwned\x07"),
            "unknown subcommand: \\x1b]0;pwned\\x07"
        );
        assert_eq!(echoed(b"b\xffd"), "unknown subcommand: b\\xffd");
        assert_eq!(
            echoed("a\u{202e}b".as_bytes()),
            "unknown subcommand: a\\u{202e}b"
        );
        let long = echoed(&[b'x'; 200]);
        assert_eq!(
            long,
            format!(
                "unknown subcommand: {}...",
                "x".repeat(ECHOED_ARGUMENT_BYTES - 3)
            )
        );
    }
}
