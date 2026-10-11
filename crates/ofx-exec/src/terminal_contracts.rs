use serde::{Deserialize, Serialize};

pub(crate) const MAX_COMMAND_BYTES: usize = 64 * 1024;
pub(crate) const MAX_AUTHORITY_TEXT_BYTES: usize = 4096;
pub(crate) const MAX_SHELL_PATH_BYTES: usize = 4096;
const MAX_DIMENSION: u16 = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dimensions {
    pub rows: u16,
    pub columns: u16,
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
