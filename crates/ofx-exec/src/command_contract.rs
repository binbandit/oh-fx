#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandStatus {
    ExitCode(i64),
    Signal(u32),
    Indeterminate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StatusProjection {
    pub exit_code: Option<i64>,
    pub signal: Option<u32>,
    pub termination_indeterminate: bool,
}

impl CommandStatus {
    pub fn project(self) -> StatusProjection {
        match self {
            Self::ExitCode(code) => StatusProjection {
                exit_code: Some(code),
                ..StatusProjection::default()
            },
            Self::Signal(signal) => StatusProjection {
                signal: Some(signal),
                ..StatusProjection::default()
            },
            Self::Indeterminate => StatusProjection {
                termination_indeterminate: true,
                ..StatusProjection::default()
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_project_to_exactly_one_outcome() {
        assert_eq!(
            CommandStatus::ExitCode(7).project(),
            StatusProjection {
                exit_code: Some(7),
                signal: None,
                termination_indeterminate: false,
            }
        );
        assert_eq!(CommandStatus::Signal(15).project().signal, Some(15));
        assert_eq!(CommandStatus::Signal(15).project().exit_code, None);
        assert!(
            CommandStatus::Indeterminate
                .project()
                .termination_indeterminate
        );
    }
}
