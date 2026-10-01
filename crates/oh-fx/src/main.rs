mod auto_upgrade;
mod cli_ask;
mod context;
mod help;
mod upgrade_command;

use std::env;
use std::ffi::OsString;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::process::ExitCode;

use ofx_cli::{
    CliError, Command, CommandLaunch, HelpLayout, Invocation, OutputFormat, TopLevelKind,
};
use signal_hook::consts::SIGPIPE;

const AUTH_MODE_VARIABLE: &str = "OH_FX_AUTH_MODE";
const NOT_AVAILABLE_CODE: &str = "NotAvailableYet";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteFailure {
    Reported,
    ReportedUnlessPipeClosed,
    Unreported,
}

fn main() -> ExitCode {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    if args == ofx_upgrade::BACKGROUND_UPGRADE_ARGS {
        return upgrade_command::run_in_background();
    }
    let parsed = ofx_cli::parse_args(args);
    let fast_help = matches!(parsed, Ok(Invocation::TopLevelHelp(HelpLayout::Terminal)));
    if !fast_help && !auth_mode_is_valid() {
        let _ = io::stderr().write_all(
            format!("oh-fx: {AUTH_MODE_VARIABLE} must be local or host-managed\n").as_bytes(),
        );
        return ExitCode::FAILURE;
    }
    match parsed {
        Ok(invocation) => run(invocation),
        Err(CliError::Ask(error)) => cli_ask::report_argument_error(error),
        Err(error) => report_error(&error),
    }
}

fn auth_mode_is_valid() -> bool {
    ofx_auth::is_valid_auth_mode(
        env::var_os(AUTH_MODE_VARIABLE)
            .as_deref()
            .map(OsStrExt::as_bytes),
    )
}

fn run(invocation: Invocation) -> ExitCode {
    match invocation {
        Invocation::TopLevelHelp(layout) => {
            auto_upgrade::announce_and_schedule();
            let failure = match layout {
                HelpLayout::Terminal => WriteFailure::Unreported,
                HelpLayout::Plain => WriteFailure::ReportedUnlessPipeClosed,
            };
            print(&help::top_level(layout), failure)
        }
        Invocation::CommandHelp(kind) => print(
            &ofx_cli::render_command_help(kind),
            command_write_failure(kind),
        ),
        Invocation::Version => print(
            &format!("{}\n", ofx_upgrade::VERSION),
            WriteFailure::ReportedUnlessPipeClosed,
        ),
        Invocation::Resume => unavailable("resume"),
        Invocation::Interactive => unavailable("interactive mode"),
        Invocation::Command(CommandLaunch { modifiers, command }) => match command {
            Command::Ask(args) => cli_ask::run(&args, &modifiers),
            Command::Upgrade(format) => upgrade_command::run(matches!(format, OutputFormat::Json)),
            other => unavailable_command(&other),
        },
    }
}

fn report_error(error: &CliError) -> ExitCode {
    let report = error.report(ofx_upgrade::VERSION);
    let _ = io::stderr().write_all(report.stderr.as_bytes());
    let failure = match error {
        CliError::InvalidArguments { command, .. } => result_write_failure(*command),
        CliError::Replay(_) => result_write_failure(TopLevelKind::Replay),
        _ => return ExitCode::FAILURE,
    };
    fail(&report.stdout, failure)
}

fn unavailable_command(command: &Command) -> ExitCode {
    let kind = command.kind();
    let exit = unavailable(kind.token());
    if !matches!(command.output_format(), OutputFormat::Json) {
        return exit;
    }
    let message = format!("{} is not available yet", kind.token());
    fail(
        &ofx_cli::command_failure_json(kind, &message, NOT_AVAILABLE_CODE),
        result_write_failure(kind),
    )
}

fn unavailable(feature: &str) -> ExitCode {
    auto_upgrade::announce_and_schedule();
    write_unavailable(feature);
    ExitCode::FAILURE
}

pub(crate) fn write_unavailable(feature: &str) {
    let _ = writeln!(io::stderr(), "oh-fx: {feature} is not available yet");
}

fn command_write_failure(kind: TopLevelKind) -> WriteFailure {
    if ignores_sigpipe(kind) {
        WriteFailure::Reported
    } else {
        WriteFailure::ReportedUnlessPipeClosed
    }
}

fn result_write_failure(kind: TopLevelKind) -> WriteFailure {
    if matches!(kind, TopLevelKind::Replay) {
        WriteFailure::Unreported
    } else {
        command_write_failure(kind)
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
    written(write_stdout(text), failure, ExitCode::SUCCESS)
}

fn fail(text: &str, failure: WriteFailure) -> ExitCode {
    written(write_stdout(text), failure, ExitCode::FAILURE)
}

fn written(result: io::Result<()>, failure: WriteFailure, exit: ExitCode) -> ExitCode {
    match result {
        Ok(()) => exit,
        Err(error)
            if error.kind() == io::ErrorKind::BrokenPipe && failure != WriteFailure::Reported =>
        {
            die_by_signal(SIGPIPE)
        }
        Err(_) if failure == WriteFailure::Unreported => ExitCode::FAILURE,
        Err(_) => write_failed(),
    }
}
