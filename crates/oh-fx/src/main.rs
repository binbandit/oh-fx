mod auto_upgrade;
mod cli;
mod cli_ask;
mod context;
mod help;
mod upgrade_command;

use std::env;
use std::ffi::OsString;
use std::io::{self, Write};
use std::process::ExitCode;

use ofx_cli::{CliError, Invocation, TopLevelKind};
use signal_hook::consts::SIGPIPE;

const BACKGROUND_UPGRADE_ARGS: [&str; 2] = ["upgrade", "--background"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteFailure {
    Reported,
    ReportedUnlessPipeClosed,
    Unreported,
}

fn main() -> ExitCode {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    if args == BACKGROUND_UPGRADE_ARGS {
        return upgrade_command::run_in_background();
    }
    match ofx_cli::parse_args(args) {
        Ok(invocation) => run(invocation),
        Err(error) => report_error(&error),
    }
}

fn run(invocation: Invocation) -> ExitCode {
    match invocation {
        Invocation::TopLevelHelp => {
            auto_upgrade::announce_and_schedule();
            print(&help::top_level(), WriteFailure::Unreported)
        }
        Invocation::CommandHelp(kind) => print(
            &ofx_cli::render_command_help(kind),
            command_write_failure(kind),
        ),
        Invocation::Version => print(
            &format!("{}\n", ofx_upgrade::VERSION),
            WriteFailure::ReportedUnlessPipeClosed,
        ),
        Invocation::Interactive => unavailable("interactive mode"),
        Invocation::Command(TopLevelKind::Ask, args) => cli_ask::run(&cli::AskArguments::new(args)),
        Invocation::Command(TopLevelKind::Upgrade, args) => match cli::upgrade_json(&args) {
            Ok(json) => upgrade_command::run(json),
            Err(usage) => {
                let _ = writeln!(io::stderr(), "{usage}");
                ExitCode::FAILURE
            }
        },
        Invocation::Command(kind, _) => unavailable(kind.token()),
    }
}

fn report_error(error: &CliError) -> ExitCode {
    let _ = io::stderr().write_all(error.report(ofx_upgrade::VERSION).as_bytes());
    ExitCode::FAILURE
}

fn unavailable(feature: &str) -> ExitCode {
    auto_upgrade::announce_and_schedule();
    let _ = writeln!(io::stderr(), "oh-fx: {feature} is not available yet");
    ExitCode::FAILURE
}

fn command_write_failure(kind: TopLevelKind) -> WriteFailure {
    if ignores_sigpipe(kind) {
        WriteFailure::Reported
    } else {
        WriteFailure::ReportedUnlessPipeClosed
    }
}

fn ignores_sigpipe(kind: TopLevelKind) -> bool {
    matches!(
        kind,
        TopLevelKind::Ask
            | TopLevelKind::Acp
            | TopLevelKind::Pr
            | TopLevelKind::Issue
            | TopLevelKind::Login
            | TopLevelKind::Logout
            | TopLevelKind::Setup
            | TopLevelKind::Status
            | TopLevelKind::Models
            | TopLevelKind::Provider
            | TopLevelKind::Doctor
            | TopLevelKind::Teams
            | TopLevelKind::Credits
            | TopLevelKind::Upgrade
            | TopLevelKind::Slack
    )
}

pub(crate) fn user_agent() -> String {
    format!("oh-fx/{}", ofx_upgrade::VERSION)
}

pub(crate) fn write_stdout(text: &str) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(text.as_bytes())?;
    stdout.flush()
}

pub(crate) fn write_failed() -> ExitCode {
    let _ = io::stderr().write_all(b"oh-fx: WriteFailed\n");
    ExitCode::FAILURE
}

pub(crate) fn die_by_signal(signal: i32) -> ExitCode {
    let _ = signal_hook::low_level::emulate_default_handler(signal);
    ExitCode::from(u8::try_from(128 + signal).unwrap_or(u8::MAX))
}

fn print(text: &str, failure: WriteFailure) -> ExitCode {
    match write_stdout(text) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error)
            if error.kind() == io::ErrorKind::BrokenPipe && failure != WriteFailure::Reported =>
        {
            die_by_signal(SIGPIPE)
        }
        Err(_) if failure == WriteFailure::Unreported => ExitCode::FAILURE,
        Err(_) => write_failed(),
    }
}
