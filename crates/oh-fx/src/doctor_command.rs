use std::io::{self, Write};
use std::process::ExitCode;

use ofx_app::Doctor;
use ofx_cli::{Command, LaunchModifiers, OutputFormat, TopLevelKind};

pub(crate) fn run(format: OutputFormat, modifiers: &LaunchModifiers) -> ExitCode {
    if modifiers.selects_sessions_v2() || crate::cli_ask::sessions_v2_variable().is_some() {
        return crate::unavailable_command(&Command::Doctor(format));
    }
    crate::auto_upgrade::announce_and_schedule();
    match Doctor::collect(ofx_auth::host_managed_auth()) {
        Ok(doctor) => crate::print(
            doctor.render(format).as_bytes(),
            crate::command_write_failure(TopLevelKind::Doctor),
        ),
        Err(error) => {
            let _ = writeln!(io::stderr(), "oh-fx: {error}");
            ExitCode::FAILURE
        }
    }
}
