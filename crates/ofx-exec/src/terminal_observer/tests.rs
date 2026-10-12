use super::*;
use crate::command_runner::SessionSupervisor;
use crate::terminal_contracts::Action;
use crate::terminal_store::TerminalStore;

fn facts(lifecycle: Lifecycle) -> SessionFacts {
    SessionFacts {
        session_id: "shell-AAAAAAAAAAAAAAAAAAAAAA".to_owned(),
        lifecycle,
        timed_out: false,
    }
}

fn exited(code: i64) -> SnapshotState {
    SnapshotState::Completed(CommandStatus::ExitCode(code))
}

#[test]
fn observed_status_refines_only_an_unknown_completed_state() {
    let finished = SnapshotState::Completed(CommandStatus::Finished);
    assert_eq!(reconcile_observed_state(finished, exited(0)), exited(0));
    assert_eq!(reconcile_observed_state(exited(1), exited(0)), exited(1));
    assert_eq!(
        reconcile_observed_state(finished, SnapshotState::Running),
        finished
    );
    assert_eq!(
        reconcile_observed_state(SnapshotState::Stopped(None), exited(0)),
        SnapshotState::Stopped(None)
    );
    assert_eq!(reconcile_observed_state(finished, finished), finished);
    assert_eq!(
        reconcile_observed_state(finished, SnapshotState::Lost),
        finished
    );
    assert_eq!(
        reconcile_observed_state(SnapshotState::Running, SnapshotState::Lost),
        SnapshotState::Lost
    );
}

#[test]
fn a_terminals_lifecycle_and_outcome_map_to_upstreams_snapshot_states() {
    assert_eq!(
        snapshot_state(&facts(Lifecycle::Starting), None),
        SnapshotState::Running
    );
    assert_eq!(
        snapshot_state(&facts(Lifecycle::Running), Some(ReturnOutcome::Started)),
        SnapshotState::Running
    );
    assert_eq!(
        snapshot_state(&facts(Lifecycle::Running), Some(ReturnOutcome::Exited(3))),
        exited(3)
    );
    assert_eq!(
        snapshot_state(&facts(Lifecycle::Exited), None),
        SnapshotState::Completed(CommandStatus::Finished)
    );
    assert_eq!(
        snapshot_state(&facts(Lifecycle::Exited), Some(ReturnOutcome::Signal(9))),
        SnapshotState::Completed(CommandStatus::Signal(9))
    );
    assert_eq!(
        snapshot_state(
            &facts(Lifecycle::Exited),
            Some(ReturnOutcome::SafetyCeiling)
        ),
        SnapshotState::Completed(CommandStatus::Finished)
    );
    assert_eq!(
        snapshot_state(&facts(Lifecycle::Lost), Some(ReturnOutcome::Exited(0))),
        SnapshotState::Lost
    );
    assert_eq!(
        snapshot_state(&facts(Lifecycle::Closed), None),
        SnapshotState::Stopped(None)
    );
    assert_eq!(
        snapshot_state(&facts(Lifecycle::Closed), Some(ReturnOutcome::Signal(15))),
        SnapshotState::Stopped(Some(CommandStatus::Signal(15)))
    );
}

#[test]
fn terminal_failures_map_to_upstreams_observation_errors() {
    let failure = |code| ObserveError::from(TerminalFailure::new(Action::Read, code, None));
    assert_eq!(
        failure(StructuredErrorCode::SessionNotFound),
        ObserveError::TerminalSessionNotFound
    );
    assert_eq!(
        failure(StructuredErrorCode::SessionLost),
        ObserveError::TerminalSessionLost
    );
    assert_eq!(
        failure(StructuredErrorCode::AuthorityDenied),
        ObserveError::TerminalAuthorityLost
    );
    assert_eq!(
        failure(StructuredErrorCode::Cancelled),
        ObserveError::Cancelled
    );
    for code in [
        StructuredErrorCode::ScreenUnavailable,
        StructuredErrorCode::InvalidRequest,
        StructuredErrorCode::InvalidLifecycle,
        StructuredErrorCode::CapacityExceeded,
    ] {
        assert_eq!(failure(code), ObserveError::TerminalObservationFailed);
    }
    assert!(ObserveError::TerminalSessionNotFound.is_definitive_loss());
    assert!(ObserveError::TerminalSessionLost.is_definitive_loss());
    assert!(ObserveError::TerminalAuthorityLost.is_definitive_loss());
    assert!(!ObserveError::Cancelled.is_definitive_loss());
    assert!(!ObserveError::TerminalObservationFailed.is_definitive_loss());
    assert_eq!(
        ObserveError::from(ExecutionError::InvalidBackend).to_string(),
        "InvalidBackend"
    );
    assert_eq!(
        ObserveError::TerminalSessionLost.to_string(),
        "TerminalSessionLost"
    );
}

#[test]
fn kept_output_waits_for_the_next_delivery_without_changing_the_state() {
    let directory = tempfile::tempdir().unwrap();
    let terminals = Terminals::new(TerminalStore::new(directory.path()).unwrap());
    let executions = ManagedExecutions::new(SessionSupervisor::new("/nonexistent/supervisor"));
    let cancel = CancellationToken::new();
    let observer = TerminalObserver {
        terminals: &terminals,
        executions: &executions,
        owner: "owner-session",
        max_output_bytes: 64,
        cancel: &cancel,
    };
    let quiet = |execution_id| TtyUpdate {
        execution_id,
        state: SnapshotState::Running,
        output: b"",
        raw_bytes: 0,
        output_incomplete: false,
        error_name: None,
        published_running: true,
    };
    executions
        .register_tty(
            None,
            &TtyCommand {
                command: "interactive",
                cwd: Path::new("/work"),
                max_output_bytes: 64,
            },
            &quiet("shell-AAAAAAAAAAAAAAAAAAAAAA"),
        )
        .unwrap();
    let drained = Observation {
        state: exited(0),
        output: b"tail".to_vec(),
        raw_bytes: 9,
        output_incomplete: false,
        timed_out: true,
    };
    observer.keep("shell-AAAAAAAAAAAAAAAAAAAAAA", &drained);
    assert_eq!(
        executions.state_for("shell-AAAAAAAAAAAAAAAAAAAAAA"),
        Some(SnapshotState::Running)
    );
    executions.observe_tty_state("shell-AAAAAAAAAAAAAAAAAAAAAA", SnapshotState::Stopped(None));
    let delivered = executions
        .update_tty(&quiet("shell-AAAAAAAAAAAAAAAAAAAAAA"))
        .unwrap();
    assert_eq!(delivered.output_delta, b"tail");
    assert_eq!(delivered.stdout_bytes, 9);
    assert_eq!(delivered.error_name, Some("TimeoutExpired"));
    assert_eq!(delivered.state, SnapshotState::Stopped(None));
}
