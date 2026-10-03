use std::fmt::Write as _;

use ofx_config::{LegacyCleanup, ProfilePaths, SettingsWriteError, SettingsWriteFailure};
use ofx_contract::{Notice, NoticeTone};

const HOME_NOT_SET: &str = "HomeNotSet";

#[derive(Debug)]
pub(crate) enum Unsaved {
    HomeNotSet,
    Failed(SettingsWriteFailure),
}

pub(crate) fn save(
    preferences: Option<&ProfilePaths>,
    commit: impl FnOnce(&ProfilePaths) -> Result<(), SettingsWriteFailure>,
) -> Result<(), Unsaved> {
    let paths = preferences.ok_or(Unsaved::HomeNotSet)?;
    commit(paths).map_err(Unsaved::Failed)
}

pub(crate) fn unsaved_notice(topic: &str, unsaved: &Unsaved) -> Notice {
    Notice::new(
        NoticeTone::Error,
        topic,
        unsaved_settings_body(unsaved, true),
    )
}

pub(crate) fn not_saved_notice(topic: &str, unsaved: &Unsaved) -> Notice {
    Notice::new(
        NoticeTone::Error,
        topic,
        unsaved_settings_body(unsaved, false),
    )
}

fn unsaved_settings_body(unsaved: &Unsaved, runtime_changed: bool) -> String {
    let (error, cleanup) = match unsaved {
        Unsaved::HomeNotSet => (HOME_NOT_SET.to_owned(), None),
        Unsaved::Failed(failure) => (failure.error.to_string(), Some(&failure.cleanup)),
    };
    let mut body = if matches!(
        unsaved,
        Unsaved::Failed(SettingsWriteFailure {
            error: SettingsWriteError::CommitIndeterminate,
            ..
        })
    ) {
        format!("user settings persistence uncertain (scope=user, error={error})")
    } else {
        let applied = if runtime_changed {
            "active for this process but "
        } else {
            ""
        };
        format!("{applied}not saved to user settings ({error})")
    };
    if let Some(cleanup) = cleanup {
        append_legacy_cleanup(&mut body, cleanup);
    }
    body
}

pub(crate) fn append_legacy_cleanup(body: &mut String, cleanup: &LegacyCleanup) {
    if cleanup.fields_removed > 0 {
        let plural = |count: usize| if count == 1 { "" } else { "s" };
        let _ = write!(
            body,
            "; normalized {} legacy value{} across {} workspace{}",
            cleanup.fields_removed,
            plural(cleanup.fields_removed),
            cleanup.workspaces_changed,
            plural(cleanup.workspaces_changed),
        );
    }
    for path in &cleanup.recovery_paths {
        let _ = write!(body, "; recovery={}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn unsaved_settings_name_an_uncertain_commit_and_the_legacy_values_it_removed() {
        let cleanup = LegacyCleanup {
            fields_removed: 2,
            workspaces_changed: 1,
            recovery_paths: vec![PathBuf::from(
                "/config/backups/settings.json.preference-migration.permission_mode.json",
            )],
        };
        assert_eq!(
            unsaved_settings_body(
                &Unsaved::Failed(SettingsWriteFailure {
                    error: SettingsWriteError::CommitIndeterminate,
                    cleanup: cleanup.clone(),
                }),
                true
            ),
            "user settings persistence uncertain (scope=user, error=SettingsCommitIndeterminate); normalized 2 legacy values across 1 workspace; recovery=/config/backups/settings.json.preference-migration.permission_mode.json"
        );
        assert_eq!(
            unsaved_settings_body(
                &Unsaved::Failed(SettingsWriteFailure {
                    error: SettingsWriteError::LockBusy,
                    cleanup: LegacyCleanup {
                        fields_removed: 1,
                        workspaces_changed: 2,
                        recovery_paths: Vec::new(),
                    },
                }),
                true
            ),
            "active for this process but not saved to user settings (SettingsLockBusy); normalized 1 legacy value across 2 workspaces"
        );
        assert_eq!(
            not_saved_notice("startup-scrollback", &Unsaved::HomeNotSet).body,
            "not saved to user settings (HomeNotSet)"
        );
        let homeless = unsaved_notice("fast", &Unsaved::HomeNotSet);
        assert_eq!(
            (
                homeless.tone,
                homeless.topic.as_str(),
                homeless.body.as_str()
            ),
            (
                NoticeTone::Error,
                "fast",
                "active for this process but not saved to user settings (HomeNotSet)"
            )
        );
    }
}
