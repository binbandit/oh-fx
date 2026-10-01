mod auto_upgrade;
mod cli;
mod cli_ask;
mod context;
mod upgrade_command;

use std::io::{self, Write};
use std::process::ExitCode;

use cli::Command;
use signal_hook::consts::SIGPIPE;

fn main() -> ExitCode {
    match cli::parse(std::env::args_os().skip(1)) {
        Ok(command) => run(command),
        Err(message) => {
            let _ = writeln!(io::stderr(), "{message}");
            ExitCode::FAILURE
        }
    }
}

fn run(command: Command) -> ExitCode {
    match command {
        Command::Version => print_fast(&format!("{}\n", ofx_upgrade::VERSION)),
        Command::Help => {
            auto_upgrade::announce_and_schedule();
            print_fast(cli::HELP)
        }
        Command::Upgrade(options) => upgrade_command::run(options),
        Command::UpgradeHelp => print(cli::UPGRADE_HELP),
        Command::Ask(arguments) => cli_ask::run(&arguments),
        Command::AskHelp => print(cli::ASK_HELP),
    }
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

fn print(text: &str) -> ExitCode {
    match write_stdout(text) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => write_failed(),
    }
}

fn print_fast(text: &str) -> ExitCode {
    match write_stdout(text) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => die_by_signal(SIGPIPE),
        Err(_) => write_failed(),
    }
}
