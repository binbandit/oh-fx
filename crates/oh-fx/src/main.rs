mod auto_upgrade;
mod cli;
mod upgrade_command;

use std::process::ExitCode;

use cli::Command;

fn main() -> ExitCode {
    match cli::parse(std::env::args_os().skip(1)) {
        Ok(command) => run(command),
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

fn run(command: Command) -> ExitCode {
    match command {
        Command::Version => {
            println!("{}", ofx_upgrade::VERSION);
            ExitCode::SUCCESS
        }
        Command::Help => {
            auto_upgrade::announce_and_schedule();
            print!("{}", cli::HELP);
            ExitCode::SUCCESS
        }
        Command::Upgrade(options) => upgrade_command::run(options),
        Command::UpgradeHelp => {
            print!("{}", cli::UPGRADE_HELP);
            ExitCode::SUCCESS
        }
    }
}
