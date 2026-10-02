use ofx_config::ProfilePaths;
use ofx_contract::{Notice, NoticeTone};

pub(crate) fn announce_and_schedule() -> Option<Notice> {
    let paths = ProfilePaths::from_environment()?;
    let notice = ofx_upgrade::version_change_since_last_run(&paths.state).map(updated_notice);
    ofx_upgrade::schedule_background_upgrade(&paths.state);
    notice
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
