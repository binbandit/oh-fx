use std::io::{self, Write};

use ofx_config::ProfilePaths;

pub(crate) fn announce_and_schedule() {
    let Some(paths) = ProfilePaths::from_environment() else {
        return;
    };
    if let Some(version) = ofx_upgrade::version_change_since_last_run(&paths.state) {
        let _ = writeln!(
            io::stderr(),
            "✓ oh-fx has been updated to v{version} (notes: {})",
            ofx_upgrade::release_notes_url(version)
        );
    }
    ofx_upgrade::schedule_background_upgrade(&paths.state);
}
