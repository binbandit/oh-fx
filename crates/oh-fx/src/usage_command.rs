use std::env;
use std::io::{self, Write};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use ofx_app::UsageSnapshot;
use ofx_cli::{OutputFormat, TopLevelKind, UsageArgs, command_failure_json};
use ofx_config::ProfilePaths;
use ofx_contract::{UsageReport, UsageScope};
use ofx_session::ProfileUsage;

const HOME_NOT_SET: &str = "HomeNotSet";

pub(crate) fn run(args: UsageArgs) -> ExitCode {
    crate::auto_upgrade::announce_and_schedule();
    let failure = crate::command_write_failure(TopLevelKind::Usage);
    match collect(args.scope) {
        Ok(report) => crate::print(
            UsageSnapshot { report: &report }
                .render(args.format)
                .as_bytes(),
            failure,
        ),
        Err(code) => {
            let message = failure_message(&code);
            match args.format {
                OutputFormat::Text => {
                    let _ = writeln!(io::stderr(), "oh-fx usage: {message}");
                    ExitCode::FAILURE
                }
                OutputFormat::Json => crate::fail(
                    &command_failure_json(TopLevelKind::Usage, message, &code),
                    failure,
                ),
            }
        }
    }
}

fn collect(scope: UsageScope) -> Result<UsageReport, String> {
    if env::var_os("HOME").is_none() {
        return Err(HOME_NOT_SET.to_owned());
    }
    let paths = ProfilePaths::from_environment().ok_or_else(|| HOME_NOT_SET.to_owned())?;
    ProfileUsage::open(&paths.data)
        .and_then(|mut usage| usage.report(scope, now_ms()))
        .map_err(|error| error.to_string())
}

fn failure_message(code: &str) -> &'static str {
    match code {
        HOME_NOT_SET => "HOME is not set",
        "DurablePathUnsafe" | "PrivateStatePermissionsUnsupported" => {
            "local usage storage is unsafe"
        }
        _ => "local usage data is unavailable",
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}
