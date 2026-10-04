use ofx_cli::RequestedResume;
use ofx_session::{ResumeTarget, SessionError, SessionStore};

use super::failure_line;
use crate::app_bootstrap_runtime::Profile;
use crate::app_session_runtime::{ResumeFailure, ResumedSession, Resumption};

const NO_REMEMBERED_SESSION: &str = "oh-fx: no remembered session for this workspace; choose one with oh-fx -r or oh-fx --resume <id>";
const REMEMBERED_SESSION_UNAVAILABLE: &str = "oh-fx: the remembered session ID could not be read; choose one with oh-fx -r or oh-fx --resume <id>";

pub(super) fn open_requested(
    store: Result<&SessionStore, &SessionError>,
    profile: &mut Profile,
    requested: &RequestedResume,
) -> Result<Option<Resumption>, String> {
    let target = match requested {
        RequestedResume::Pick => return available(store).map(|_| None),
        RequestedResume::Last => ResumeTarget::Last,
        RequestedResume::Id(id) => ResumeTarget::Id(id.clone()),
        RequestedResume::Remembered => remembered_target(available(store)?)?,
    };
    let session =
        ResumedSession::open(available(store)?, profile, &target).map_err(
            |failure| match failure {
                ResumeFailure::Session(error) => session_failure_line(error),
                ResumeFailure::Selection(error) => failure_line(&error),
            },
        )?;
    Ok(Some(Resumption {
        session,
        remember: *requested != RequestedResume::Remembered,
    }))
}

fn available<'a>(
    store: Result<&'a SessionStore, &SessionError>,
) -> Result<&'a SessionStore, String> {
    store.map_err(|error| failure_line(error))
}

fn remembered_target(store: &SessionStore) -> Result<ResumeTarget, String> {
    match store.remembered_session_id() {
        Ok(Some(id)) => Ok(ResumeTarget::Id(id)),
        Ok(None) => Err(NO_REMEMBERED_SESSION.to_owned()),
        Err(_) => Err(REMEMBERED_SESSION_UNAVAILABLE.to_owned()),
    }
}

fn session_failure_line(error: SessionError) -> String {
    let line = match error {
        SessionError::NoSavedSessions => "oh-fx: no saved sessions for this workspace.",
        SessionError::NoReadableSessions => {
            "oh-fx: no readable saved sessions for this workspace, and some saved sessions are unreadable; run `oh-fx doctor` for recovery guidance."
        }
        SessionError::SessionNotFound => "oh-fx: saved session not found.",
        SessionError::OneOffSessionNotResumable => {
            "oh-fx: subagent child sessions cannot be resumed directly; message the named agent from its parent session"
        }
        SessionError::SessionBusy => {
            "oh-fx: another oh-fx process may be using this session (running or suspended); check other terminals or run jobs, then use fg or quit that process"
        }
        SessionError::SessionLockUnsupported => {
            "oh-fx: the filesystem cannot provide the required session lock"
        }
        SessionError::InvalidSessionFormat => {
            "oh-fx: saved session is unreadable. Run `oh-fx doctor`; if it is recoverable, use `oh-fx session recover <id>`."
        }
        SessionError::UnsupportedSessionSchema => {
            "oh-fx: saved session uses an unsupported version and cannot be resumed by this oh-fx build."
        }
        other => return failure_line(&other),
    };
    line.to_owned()
}
