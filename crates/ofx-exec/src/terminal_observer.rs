#[cfg(test)]
mod tests;

use std::path::Path;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::command_contract::CommandStatus;
use crate::managed_execution::{
    ExecutionError, ManagedExecutions, SnapshotState, TtyCommand, TtyUpdate,
};
use crate::native_session::Terminals;
use crate::terminal_contracts::{
    Lifecycle, ReturnCondition, ReturnOutcome, SessionFacts, StructuredErrorCode, TerminalFailure,
};

const TIMEOUT_EXPIRED: &str = "TimeoutExpired";
const STATUS_PROBE: Duration = Duration::from_millis(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ObserveError {
    #[error("TerminalSessionNotFound")]
    TerminalSessionNotFound,
    #[error("TerminalSessionLost")]
    TerminalSessionLost,
    #[error("TerminalAuthorityLost")]
    TerminalAuthorityLost,
    #[error("Cancelled")]
    Cancelled,
    #[error("TerminalObservationFailed")]
    TerminalObservationFailed,
    #[error(transparent)]
    Execution(#[from] ExecutionError),
}

impl ObserveError {
    fn is_definitive_loss(self) -> bool {
        matches!(
            self,
            Self::TerminalSessionNotFound | Self::TerminalSessionLost | Self::TerminalAuthorityLost
        )
    }
}

impl From<TerminalFailure> for ObserveError {
    fn from(failure: TerminalFailure) -> Self {
        match failure.code {
            StructuredErrorCode::SessionNotFound => Self::TerminalSessionNotFound,
            StructuredErrorCode::SessionLost => Self::TerminalSessionLost,
            StructuredErrorCode::AuthorityDenied => Self::TerminalAuthorityLost,
            StructuredErrorCode::Cancelled => Self::Cancelled,
            _ => Self::TerminalObservationFailed,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub state: SnapshotState,
    pub output: Vec<u8>,
    pub raw_bytes: usize,
    pub output_incomplete: bool,
    pub timed_out: bool,
}

impl Observation {
    pub fn update<'a>(&'a self, execution_id: &'a str, published_running: bool) -> TtyUpdate<'a> {
        TtyUpdate {
            execution_id,
            state: self.state,
            output: &self.output,
            raw_bytes: self.raw_bytes,
            output_incomplete: self.output_incomplete,
            error_name: self.timed_out.then_some(TIMEOUT_EXPIRED),
            published_running,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncOutcome {
    Indexed,
    EndedWithOwner,
}

pub struct TerminalObserver<'a> {
    pub terminals: &'a Terminals,
    pub executions: &'a ManagedExecutions,
    pub owner: &'a str,
    pub max_output_bytes: usize,
    pub cancel: &'a CancellationToken,
}

impl TerminalObserver<'_> {
    pub async fn observe(
        &self,
        session_id: &str,
        current: SnapshotState,
    ) -> Result<Observation, ObserveError> {
        let read = self.terminals.read(self.owner, session_id)?;
        let observed = self
            .resolve_completed_status(session_id, snapshot_state(&read.session, None))
            .await;
        let output = if read.contains_escape {
            self.terminals.screen(self.owner, session_id)?.text
        } else {
            read.output
        };
        Ok(Observation {
            state: reconcile_observed_state(current, observed),
            output,
            raw_bytes: usize::try_from(read.total_bytes).unwrap_or(usize::MAX),
            output_incomplete: read.output_incomplete,
            timed_out: read.session.timed_out,
        })
    }

    pub async fn refresh(&self, execution_id: &str) -> Result<(), ObserveError> {
        let current = self
            .executions
            .state_for(execution_id)
            .ok_or(ExecutionError::ExecutionNotFound)?;
        let observed = match self.observe(execution_id, current).await {
            Ok(observed) => observed,
            Err(error) => {
                if error.is_definitive_loss() {
                    self.executions
                        .observe_tty_state(execution_id, SnapshotState::Lost);
                }
                return Err(error);
            }
        };
        self.executions
            .refresh_tty(&observed.update(execution_id, true))?;
        Ok(())
    }

    pub async fn sync_owned(
        &self,
        requested_session_id: &str,
    ) -> Result<SyncOutcome, ObserveError> {
        let mut outcome = SyncOutcome::Indexed;
        for facts in self.terminals.list(self.owner)? {
            if facts.lifecycle == Lifecycle::Closed
                || self.executions.backend_for(&facts.session_id).is_some()
            {
                continue;
            }
            if facts.lifecycle == Lifecycle::Lost {
                if facts.session_id == requested_session_id {
                    outcome = SyncOutcome::EndedWithOwner;
                }
                continue;
            }
            let inspected = self.terminals.inspect(self.owner, &facts.session_id)?;
            let Some(command) = inspected.command else {
                continue;
            };
            let capacity = self.executions.reserve_tty_capacity()?;
            let observed = self
                .observe(&facts.session_id, snapshot_state(&facts, None))
                .await?;
            self.executions.index_tty(
                Some(capacity),
                &TtyCommand {
                    command: &command,
                    cwd: Path::new(&inspected.cwd),
                    max_output_bytes: self.max_output_bytes,
                },
                &observed.update(&facts.session_id, true),
            )?;
        }
        Ok(outcome)
    }

    pub fn keep(&self, execution_id: &str, observation: &Observation) {
        let _ = self.executions.refresh_tty(&TtyUpdate {
            state: SnapshotState::Running,
            ..observation.update(execution_id, true)
        });
    }

    async fn resolve_completed_status(
        &self,
        session_id: &str,
        state: SnapshotState,
    ) -> SnapshotState {
        if state != SnapshotState::Completed(CommandStatus::Finished) {
            return state;
        }
        self.terminals
            .wait(
                self.owner,
                session_id,
                ReturnCondition::Exit,
                STATUS_PROBE,
                self.cancel,
            )
            .await
            .map_or(state, |waited| {
                snapshot_state(&waited.session, Some(waited.outcome))
            })
    }
}

pub fn snapshot_state(facts: &SessionFacts, outcome: Option<ReturnOutcome>) -> SnapshotState {
    let status = outcome.and_then(ReturnOutcome::status);
    match facts.lifecycle {
        Lifecycle::Starting | Lifecycle::Running => {
            status.map_or(SnapshotState::Running, SnapshotState::Completed)
        }
        Lifecycle::Exited => SnapshotState::Completed(status.unwrap_or(CommandStatus::Finished)),
        Lifecycle::Lost => SnapshotState::Lost,
        Lifecycle::Closed => SnapshotState::Stopped(status),
    }
}

fn reconcile_observed_state(current: SnapshotState, observed: SnapshotState) -> SnapshotState {
    match (current, observed) {
        (SnapshotState::Running, _) => observed,
        (SnapshotState::Completed(CommandStatus::Finished), SnapshotState::Completed(refined))
            if refined != CommandStatus::Finished =>
        {
            observed
        }
        _ => current,
    }
}
