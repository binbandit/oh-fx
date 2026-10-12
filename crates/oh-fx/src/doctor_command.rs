use std::io::{self, Write};
use std::process::ExitCode;

use ofx_app::Doctor;
use ofx_cli::{LaunchModifiers, OutputFormat, TopLevelKind};

pub(crate) fn run(format: OutputFormat, modifiers: &LaunchModifiers) -> ExitCode {
    crate::auto_upgrade::announce_and_schedule();
    let sessions_v2 = crate::sessions_command::selects_sessions_v2(modifiers);
    match Doctor::collect(ofx_auth::host_managed_auth(), sessions_v2) {
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
