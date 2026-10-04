use std::io::{self, Write};
use std::process::ExitCode;

use ofx_app::StartupStatus;
use ofx_cli::{OutputFormat, TopLevelKind};

pub(crate) fn run(format: OutputFormat) -> ExitCode {
    crate::auto_upgrade::announce_and_schedule();
    let status = match StartupStatus::load(ofx_auth::host_managed_auth()) {
        Ok(status) => status,
        Err(error) => {
            let _ = writeln!(io::stderr(), "oh-fx: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut stderr = io::stderr().lock();
    for diagnostic in status.diagnostics() {
        let _ = writeln!(stderr, "oh-fx: {diagnostic}");
    }
    drop(stderr);
    crate::print(
        status.render(format).as_bytes(),
        crate::command_write_failure(TopLevelKind::Status),
    )
}
