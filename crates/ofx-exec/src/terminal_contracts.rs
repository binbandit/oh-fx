use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::command_contract::CommandStatus;

pub(crate) const MAX_COMMAND_BYTES: usize = 64 * 1024;
pub(crate) const MAX_WRITE_BYTES: usize = 64 * 1024;
pub(crate) const MAX_AUTHORITY_TEXT_BYTES: usize = 4096;
pub(crate) const MAX_SHELL_PATH_BYTES: usize = 4096;
const MAX_DIMENSION: u16 = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Dimensions {
    pub(crate) rows: u16,
    pub(crate) columns: u16,
}

impl Dimensions {
    pub(crate) fn is_valid(self) -> bool {
        self.rows != 0
            && self.columns != 0
            && self.rows <= MAX_DIMENSION
            && self.columns <= MAX_DIMENSION
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    Starting,
    Running,
    Exited,
    Lost,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LifecycleEvent {
    ChildStarted,
    ChildExited,
    HostLost,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("InvalidLifecycleTransition")]
pub(crate) struct InvalidLifecycleTransition;

impl Lifecycle {
    #[must_use]
    pub fn is_live(self) -> bool {
        matches!(self, Self::Starting | Self::Running)
    }

    pub(crate) fn transition(
        self,
        event: LifecycleEvent,
    ) -> Result<Self, InvalidLifecycleTransition> {
        match (self, event) {
            (Self::Starting, LifecycleEvent::ChildStarted) => Ok(Self::Running),
            (Self::Starting | Self::Running, LifecycleEvent::ChildExited) => Ok(Self::Exited),
            (Self::Starting | Self::Running, LifecycleEvent::HostLost) => Ok(Self::Lost),
            (Self::Starting | Self::Running | Self::Exited | Self::Lost, LifecycleEvent::Close) => {
                Ok(Self::Closed)
            }
            _ => Err(InvalidLifecycleTransition),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Start,
    Read,
    Screen,
    Write,
    Wait,
    Inspect,
    List,
    Signal,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StructuredErrorCode {
    InvalidRequest,
    ShellUnavailable,
    PtyUnavailable,
    StartupFailed,
    ProcessIdentityUnavailable,
    SessionLost,
    SessionNotFound,
    InvalidLifecycle,
    AuthorityDenied,
    ScreenUnavailable,
    CapacityExceeded,
    Cancelled,
}

impl Action {
    pub fn name(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Read => "read",
            Self::Screen => "screen",
            Self::Write => "write",
            Self::Wait => "wait",
            Self::Inspect => "inspect",
            Self::List => "list",
            Self::Signal => "signal",
            Self::Close => "close",
        }
    }
}

impl StructuredErrorCode {
    pub fn name(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::ShellUnavailable => "shell_unavailable",
            Self::PtyUnavailable => "pty_unavailable",
            Self::StartupFailed => "startup_failed",
            Self::ProcessIdentityUnavailable => "process_identity_unavailable",
            Self::SessionLost => "session_lost",
            Self::SessionNotFound => "session_not_found",
            Self::InvalidLifecycle => "invalid_lifecycle",
            Self::AuthorityDenied => "authority_denied",
            Self::ScreenUnavailable => "screen_unavailable",
            Self::CapacityExceeded => "capacity_exceeded",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalFailure {
    pub action: Action,
    pub code: StructuredErrorCode,
    pub session_id: Option<String>,
}

impl TerminalFailure {
    pub(crate) fn new(action: Action, code: StructuredErrorCode, session_id: Option<&str>) -> Self {
        Self {
            action,
            code,
            session_id: session_id.map(str::to_owned),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellSpec {
    UserLogin,
    Executable { path: PathBuf, clean_start: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReturnCondition {
    Started,
    Exit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReturnOutcome {
    Started,
    SafetyCeiling,
    Cancelled,
    Exited(i32),
    Signal(u32),
}

impl ReturnOutcome {
    pub(crate) fn is_terminal(self) -> bool {
        matches!(self, Self::Exited(_) | Self::Signal(_))
    }

    pub fn status(self) -> Option<CommandStatus> {
        match self {
            Self::Exited(code) => Some(CommandStatus::ExitCode(code.into())),
            Self::Signal(signal) => Some(CommandStatus::Signal(signal)),
            Self::Started | Self::SafetyCeiling | Self::Cancelled => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalSignal {
    Hangup,
    Interrupt,
    Quit,
    Terminate,
    Kill,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClosePolicy {
    Graceful,
    Force,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionFacts {
    pub session_id: String,
    pub lifecycle: Lifecycle,
    pub timed_out: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_lifecycle_accepts_every_valid_transition_and_rejects_the_rest() {
        use Lifecycle::{Closed, Exited, Lost, Running, Starting};
        use LifecycleEvent::{ChildExited, ChildStarted, Close, HostLost};
        let valid = [
            (Starting, ChildStarted, Running),
            (Starting, ChildExited, Exited),
            (Starting, HostLost, Lost),
            (Starting, Close, Closed),
            (Running, ChildExited, Exited),
            (Running, HostLost, Lost),
            (Running, Close, Closed),
            (Exited, Close, Closed),
            (Lost, Close, Closed),
        ];
        for state in [Starting, Running, Exited, Lost, Closed] {
            for event in [ChildStarted, ChildExited, HostLost, Close] {
                let expected = valid
                    .iter()
                    .find(|(from, on, _)| *from == state && *on == event)
                    .map(|(_, _, to)| *to)
                    .ok_or(InvalidLifecycleTransition);
                assert_eq!(state.transition(event), expected, "{state:?} {event:?}");
            }
            assert_eq!(state.is_live(), matches!(state, Starting | Running));
        }
    }

    #[test]
    fn dimensions_accept_only_nonzero_sizes_up_to_the_maximum() {
        for (rows, columns, valid) in [
            (24, 80, true),
            (1, 1, true),
            (4096, 4096, true),
            (0, 80, false),
            (24, 0, false),
            (4097, 80, false),
            (24, 4097, false),
        ] {
            assert_eq!(
                Dimensions { rows, columns }.is_valid(),
                valid,
                "{rows}x{columns}"
            );
        }
    }
}
