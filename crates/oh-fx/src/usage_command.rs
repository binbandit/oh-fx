use std::env;
use std::io::{self, Write};
use std::process::ExitCode;

use ofx_cli::{OutputFormat, TopLevelKind, command_failure_json};

const HOME_NOT_SET: (&str, &str) = ("HomeNotSet", "HOME is not set");
const PROFILE_USAGE_UNAVAILABLE: (&str, &str) =
    ("ProfileUsageUnavailable", "local usage data is unavailable");

pub(crate) fn run(format: OutputFormat) -> ExitCode {
    crate::auto_upgrade::announce_and_schedule();
    let (code, message) = if env::var_os("HOME").is_none() {
        HOME_NOT_SET
    } else {
        PROFILE_USAGE_UNAVAILABLE
    };
    match format {
        OutputFormat::Text => {
            let _ = writeln!(io::stderr(), "oh-fx usage: {message}");
            ExitCode::FAILURE
        }
        OutputFormat::Json => crate::fail(
            &command_failure_json(TopLevelKind::Usage, message, code),
            crate::command_write_failure(TopLevelKind::Usage),
        ),
    }
}
