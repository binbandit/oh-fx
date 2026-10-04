use std::fmt;
use std::path::PathBuf;

use ofx_contract::{Notice, NoticeTone, UiEvent};
use ofx_session::SessionError;
use ofx_upgrade::UpgradeError;

use super::relaunch::Relaunch;
use super::session_upgrader::Readiness;

const UPGRADE_TOPIC: &str = "upgrade";

#[derive(Debug)]
pub(crate) enum Unresumable {
    Unavailable,
    Session(SessionError),
}

impl fmt::Display for Unresumable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => formatter.write_str("SessionPersistenceUnavailable"),
            Self::Session(error) => write!(formatter, "{error}"),
        }
    }
}

pub(crate) trait ResumeHandoff {
    fn prepare_resume_handoff(&self) -> Result<(), Unresumable>;

    fn request_resume_handoff(&mut self, relaunch: Relaunch);
}

#[derive(Clone, Default)]
pub(crate) struct UpgradeShortcut {
    readiness: Option<Readiness>,
    relaunch: Relaunch,
}

impl UpgradeShortcut {
    pub(crate) fn new(readiness: Option<Readiness>, relaunch: Relaunch) -> Self {
        Self {
            readiness,
            relaunch,
        }
    }

    pub(crate) fn apply(
        &self,
        handoff: Option<&mut dyn ResumeHandoff>,
        emit: &dyn Fn(UiEvent),
        ultrafast_requested: bool,
    ) -> bool {
        self.apply_with(
            handoff,
            emit,
            ultrafast_requested,
            ofx_upgrade::installed_executable,
        )
    }

    fn apply_with(
        &self,
        handoff: Option<&mut dyn ResumeHandoff>,
        emit: &dyn Fn(UiEvent),
        ultrafast_requested: bool,
        executable: impl FnOnce() -> Result<PathBuf, UpgradeError>,
    ) -> bool {
        let notice = |tone, body: String| {
            emit(UiEvent::Notice {
                notice: Notice::new(tone, UPGRADE_TOPIC, body),
            });
        };
        let Some(readiness) = &self.readiness else {
            notice(NoticeTone::Neutral, "auto-upgrade is disabled".to_owned());
            return false;
        };
        if !readiness.ready() {
            notice(
                NoticeTone::Neutral,
                "no installed upgrade is ready".to_owned(),
            );
            return false;
        }
        let handoff = match prepare(handoff) {
            Ok(handoff) => handoff,
            Err(error) => {
                notice(
                    NoticeTone::Error,
                    format!(
                        "upgrade paused because this conversation is not safely resumable: {error}; run `oh-fx doctor` for recovery guidance"
                    ),
                );
                return false;
            }
        };
        let executable = match executable() {
            Ok(executable) => executable,
            Err(error) => {
                notice(
                    NoticeTone::Error,
                    format!(
                        "upgrade installed, but the executable path could not be resolved: {error}; restart oh-fx manually"
                    ),
                );
                return false;
            }
        };
        self.relaunch.request(executable, ultrafast_requested);
        handoff.request_resume_handoff(self.relaunch.clone());
        emit(UiEvent::ExitRequested);
        true
    }
}

fn prepare(handoff: Option<&mut dyn ResumeHandoff>) -> Result<&mut dyn ResumeHandoff, Unresumable> {
    let handoff = handoff.ok_or(Unresumable::Unavailable)?;
    handoff.prepare_resume_handoff()?;
    Ok(handoff)
}

#[cfg(test)]
mod tests;
