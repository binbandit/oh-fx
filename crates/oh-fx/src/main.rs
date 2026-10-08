mod ask_session;
mod auto_upgrade;
mod cli_ask;
mod command_echo;
mod doctor_command;
mod help;
mod login_command;
mod models_command;
mod permission_prompt;
mod permissions_command;
mod provider_activation;
mod provider_command;
mod question_call_record;
mod sessions_command;
mod shell_call_record;
mod status_command;
mod upgrade_command;
mod usage_command;
mod workspace_command;

use std::env;
use std::ffi::OsString;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::process::ExitCode;

use ofx_auth::AUTH_MODE_VARIABLE;
use ofx_cli::{
    CliError, Command, CommandLaunch, HelpLayout, Invocation, LaunchModifiers, OutputFormat,
    RequestedResume, TopLevelKind,
};
use rustix::io::Errno;
use signal_hook::consts::SIGPIPE;

const NOT_AVAILABLE_CODE: &str = "NotAvailableYet";
const VERSION_LINE: [u8; ofx_upgrade::VERSION.len() + 1] = version_line();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteFailure {
    Reported,
    ReportedUnlessPipeClosed,
    Unreported,
}

fn main() -> ExitCode {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    if ofx_exec::is_foreground_session_invocation(&args) {
        ofx_exec::run_foreground_session(&args);
    }
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
            print(help::top_level(layout).as_bytes(), failure)
        }
        Invocation::CommandHelp(kind) => print(
            ofx_cli::render_command_help(kind).as_bytes(),
            command_write_failure(kind),
        ),
        Invocation::Version => print(&VERSION_LINE, WriteFailure::ReportedUnlessPipeClosed),
        Invocation::Interactive(modifiers) => run_interactive(&modifiers, None),
        Invocation::Resume(modifiers, resume) => run_interactive(&modifiers, Some(&resume)),
        Invocation::Command(CommandLaunch { modifiers, command }) => match command {
            Command::Ask(args) => cli_ask::run(&args, &modifiers),
            Command::Upgrade(format) => upgrade_command::run(matches!(format, OutputFormat::Json)),
            Command::Doctor(format) => doctor_command::run(format, &modifiers),
            Command::Sessions(args) => sessions_command::run(&args, &modifiers),
            Command::Login(provider) => login_command::login(provider.as_ref()),
            Command::Logout(provider) => login_command::logout(provider.as_ref()),
            Command::Models(format) => models_command::run(format),
            Command::Permissions(format) => permissions_command::run(format),
            Command::Provider(target) => provider_command::run(target),
            Command::Status(format) => status_command::run(format),
            Command::Usage(format) => usage_command::run(format),
            Command::Workspace(args) => workspace_command::run(&args),
            other => unavailable_command(&other),
        },
    }
}

fn run_interactive(modifiers: &LaunchModifiers, resume: Option<&RequestedResume>) -> ExitCode {
    let unsupported = [
        (modifiers.overrides_provider(), "--provider"),
        (modifiers.selects_sessions_v2(), "--sessions-v2"),
    ];
    let unavailable_flag = cli_ask::unsupported_launch_modifier(modifiers)
        .or_else(|| cli_ask::first_requested(unsupported))
        .or_else(cli_ask::sessions_v2_variable);
    match unavailable_flag {
        Some(flag) => unavailable(flag),
        None => ofx_app::run_interactive(modifiers, resume),
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

pub(crate) fn unavailable_command(command: &Command) -> ExitCode {
    auto_upgrade::announce_and_schedule();
    not_available(command)
}

pub(crate) fn not_available(command: &Command) -> ExitCode {
    let kind = command.kind();
    write_unavailable(kind.token());
    if !matches!(command.output_format(), OutputFormat::Json) {
        return ExitCode::FAILURE;
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

pub(crate) fn command_write_failure(kind: TopLevelKind) -> WriteFailure {
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
    )
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

const fn version_line() -> [u8; ofx_upgrade::VERSION.len() + 1] {
    let mut line = [b'\n'; ofx_upgrade::VERSION.len() + 1];
    let (version, _) = line.split_at_mut(ofx_upgrade::VERSION.len());
    version.copy_from_slice(ofx_upgrade::VERSION.as_bytes());
    line
}

fn write_stdout_unbuffered(mut bytes: &[u8]) -> io::Result<()> {
    let stdout = rustix::stdio::stdout();
    while !bytes.is_empty() {
        match rustix::io::write(stdout, bytes) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(count) => bytes = &bytes[count..],
            Err(Errno::INTR) => {}
            Err(Errno::BADF) => return Ok(()),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub(crate) fn print(bytes: &[u8], failure: WriteFailure) -> ExitCode {
    written(write_stdout_unbuffered(bytes), failure, ExitCode::SUCCESS)
}

pub(crate) fn fail(text: &str, failure: WriteFailure) -> ExitCode {
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
