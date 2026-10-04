use std::env;
use std::fs;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use crate::build_identity;

const CHECK_INTERVAL: Duration = Duration::from_mins(5);
const CHECK_MARKER: &str = "upgrade-check";
const LAST_VERSION_FILE: &str = "last-version";
const DISABLE_VARIABLE: &str = "OH_FX_AUTO_UPGRADE";

pub const BACKGROUND_UPGRADE_ARGS: [&str; 2] = ["upgrade", "--background"];

pub fn schedule_background_upgrade(state_directory: &Path) {
    if claim_auto_upgrade_check(state_directory) {
        spawn_background_upgrade();
    }
}

fn spawn_background_upgrade() {
    let Ok(executable) = env::current_exe() else {
        return;
    };
    let _ = Command::new(executable)
        .args(BACKGROUND_UPGRADE_ARGS)
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn();
}

pub fn auto_upgrade_allowed() -> bool {
    auto_upgrade_enabled(env::var(DISABLE_VARIABLE).ok().as_deref())
        && build_identity::release_version().is_some()
}

fn claim_auto_upgrade_check(state_directory: &Path) -> bool {
    if !auto_upgrade_allowed() {
        return false;
    }
    let marker = state_directory.join(CHECK_MARKER);
    if checked_recently(fs::metadata(&marker).and_then(|metadata| metadata.modified())) {
        return false;
    }
    fs::create_dir_all(state_directory)
        .and_then(|()| fs::write(&marker, b""))
        .is_ok()
}

pub fn version_change_since_last_run(state_directory: &Path) -> Option<&'static str> {
    build_identity::release_version()?;
    let last_version_file = state_directory.join(LAST_VERSION_FILE);
    let last_version = fs::read_to_string(&last_version_file).ok();
    if last_version.as_deref() == Some(build_identity::VERSION) {
        return None;
    }
    fs::create_dir_all(state_directory)
        .and_then(|()| fs::write(&last_version_file, build_identity::VERSION))
        .ok()?;
    last_version.map(|_| build_identity::VERSION)
}

fn auto_upgrade_enabled(setting: Option<&str>) -> bool {
    !setting.is_some_and(|value| value == "0" || value.eq_ignore_ascii_case("false"))
}

fn checked_recently(last_check: io::Result<SystemTime>) -> bool {
    last_check
        .ok()
        .and_then(|time| time.elapsed().ok())
        .is_some_and(|elapsed| elapsed < CHECK_INTERVAL)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disables_only_for_zero_or_false() {
        assert!(auto_upgrade_enabled(None));
        assert!(auto_upgrade_enabled(Some("1")));
        assert!(auto_upgrade_enabled(Some(" false")));
        assert!(!auto_upgrade_enabled(Some("0")));
        assert!(!auto_upgrade_enabled(Some("FALSE")));
    }

    #[test]
    fn treats_missing_or_stale_markers_as_due() {
        assert!(!checked_recently(Err(io::ErrorKind::NotFound.into())));
        assert!(checked_recently(Ok(SystemTime::now())));
        let stale = SystemTime::now() - CHECK_INTERVAL - Duration::from_secs(1);
        assert!(!checked_recently(Ok(stale)));
    }
}
