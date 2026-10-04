mod live_release;
mod session_upgrader;

use std::time::Duration;

use ofx_config::ProfilePaths;
use ofx_contract::{Notice, NoticeTone, UiEvent};
use ofx_tui::UiEventSender;

use live_release::LiveRelease;
#[cfg(test)]
pub(crate) use session_upgrader::{CheckOutcome, ReleaseCheck};
pub(crate) use session_upgrader::{SessionUpgrader, Timing};

const SESSION_TIMING: Timing = Timing {
    initial_delay: Duration::from_secs(10),
    interval: Duration::from_mins(5),
};

pub(crate) fn announce_update() -> Option<Notice> {
    let paths = ProfilePaths::from_environment()?;
    ofx_upgrade::version_change_since_last_run(&paths.state).map(updated_notice)
}

pub(crate) fn start_session_upgrader(events: UiEventSender) -> Option<SessionUpgrader> {
    if !ofx_upgrade::auto_upgrade_allowed() {
        return None;
    }
    let paths = ProfilePaths::from_environment()?;
    let release = LiveRelease::installed(paths.state)?;
    SessionUpgrader::start(release, SESSION_TIMING, move |label| {
        events.send(UiEvent::UpgradeStatus { label });
    })
    .ok()
}

fn updated_notice(version: &str) -> Notice {
    Notice::new(
        NoticeTone::Success,
        "",
        format!("oh-fx has been updated to v{version}"),
    )
    .with_link("notes", ofx_upgrade::release_notes_url(version))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_notices_link_the_release_notes() {
        let notice = updated_notice("0.1.0-dev.42");
        assert_eq!(notice.tone, NoticeTone::Success);
        assert_eq!(notice.body, "oh-fx has been updated to v0.1.0-dev.42");
        let link = notice.link.unwrap();
        assert_eq!(link.label, "notes");
        assert!(link.url.ends_with("/tag/v0.1.0-dev.42"));
    }
}
