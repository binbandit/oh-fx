use std::env;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

use ofx_config::ProfilePaths;

pub(crate) fn announce_and_schedule() {
    let Some(paths) = ProfilePaths::from_environment() else {
        return;
    };
    if let Some(version) = ofx_upgrade::version_change_since_last_run(&paths.state) {
        eprintln!(
            "✓ oh-fx has been updated to v{version} (notes: {})",
            ofx_upgrade::release_notes_url(version)
        );
    }
    if ofx_upgrade::claim_auto_upgrade_check(&paths.state) {
        spawn_background_upgrade();
    }
}

fn spawn_background_upgrade() {
    let Ok(executable) = env::current_exe() else {
        return;
    };
    let _ = Command::new(executable)
        .args(["upgrade", "--background"])
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn();
}
