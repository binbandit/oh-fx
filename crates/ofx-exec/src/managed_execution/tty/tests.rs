use std::path::Path;

use super::*;
use crate::command_contract::CommandStatus;
use crate::command_runner::SessionSupervisor;
use crate::managed_execution::{MAX_TOMBSTONES, Persistence};

const EXITED: SnapshotState = SnapshotState::Completed(CommandStatus::ExitCode(0));
const FINISHED: SnapshotState = SnapshotState::Completed(CommandStatus::Finished);

fn executions() -> ManagedExecutions {
    ManagedExecutions::new(SessionSupervisor::new("/nonexistent/supervisor"))
}

fn command() -> TtyCommand<'static> {
    TtyCommand {
        command: "interactive",
        cwd: Path::new("/work"),
        max_output_bytes: 64,
    }
}

fn update<'a>(id: &'a str, state: SnapshotState, output: &'a [u8]) -> TtyUpdate<'a> {
    TtyUpdate {
        execution_id: id,
        state,
        output,
        raw_bytes: output.len(),
        output_incomplete: false,
        error_name: None,
        published_running: true,
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

#[test]
fn a_terminal_execution_delivers_each_refreshed_delta_once() {
    let executions = executions();
    let started = executions
        .register_tty(
            None,
            &command(),
            &update("tty-cursor", SnapshotState::Running, b""),
        )
        .unwrap();
    assert_eq!(started.backend, Backend::Tty);
    assert_eq!(started.persistence, Persistence::Session);
    assert_eq!(started.state, SnapshotState::Running);
    assert!(started.retained);
    assert_eq!(started.cwd, Path::new("/work"));
    executions
        .refresh_tty(&update("tty-cursor", SnapshotState::Running, b"ab"))
        .unwrap();
    let delivered = executions
        .update_tty(&update("tty-cursor", SnapshotState::Running, b""))
        .unwrap();
    assert_eq!(delivered.output_delta, b"ab");
    assert_eq!(delivered.stdout_bytes, 2);
    let again = executions
        .update_tty(&update("tty-cursor", SnapshotState::Running, b""))
        .unwrap();
    assert!(again.output_delta.is_empty());
    assert_eq!(again.stdout_bytes, 2);
    assert_eq!(executions.backend_for("tty-cursor"), Some(Backend::Tty));
    assert_eq!(
        executions.state_for("tty-cursor"),
        Some(SnapshotState::Running)
    );
    assert_eq!(
        executions.command("tty-cursor").as_deref(),
        Some("interactive")
    );
    let cancel = tokio_util::sync::CancellationToken::new();
    assert_eq!(
        block_on(executions.wait("tty-cursor", std::time::Duration::ZERO, &cancel)),
        Err(ExecutionError::TtyEffectRequired)
    );
    assert_eq!(
        block_on(executions.stop("tty-cursor", true)),
        Err(ExecutionError::TtyEffectRequired)
    );
}

#[test]
fn the_first_update_keeps_the_output_head_and_counts_every_byte() {
    let executions = executions();
    let output = [b'x'; 100];
    let first = executions
        .register_tty(
            None,
            &command(),
            &TtyUpdate {
                output_incomplete: true,
                error_name: Some("TimeoutExpired"),
                ..update("tty-head", SnapshotState::Running, &output)
            },
        )
        .unwrap();
    assert_eq!(first.output_delta, [b'x'; 64]);
    assert!(first.output_truncated);
    assert!(first.output_incomplete);
    assert_eq!(first.stdout_bytes, 100);
    assert_eq!(first.error_name, Some("TimeoutExpired"));
    let quiet = executions
        .register_tty(
            None,
            &command(),
            &TtyUpdate {
                output_incomplete: true,
                ..update("tty-gap", SnapshotState::Running, b"ok")
            },
        )
        .unwrap();
    assert!(!quiet.output_truncated);
    assert!(quiet.output_incomplete);
    let later = executions
        .update_tty(&TtyUpdate {
            output_incomplete: true,
            ..update("tty-gap", SnapshotState::Running, b"")
        })
        .unwrap();
    assert!(later.output_truncated);
    assert!(later.output_incomplete);
    let screen = executions
        .register_tty(
            None,
            &command(),
            &TtyUpdate {
                raw_bytes: 100,
                ..update("tty-screen", SnapshotState::Running, b"0123456789")
            },
        )
        .unwrap();
    assert_eq!(screen.output_delta, b"0123456789");
    assert_eq!(screen.stdout_bytes, 100);
    let repainted = executions
        .update_tty(&TtyUpdate {
            raw_bytes: 40,
            ..update("tty-screen", SnapshotState::Running, b"abcd")
        })
        .unwrap();
    assert_eq!(repainted.output_delta, b"abcd");
    assert_eq!(repainted.stdout_bytes, 140);
}

#[test]
fn a_settled_state_stays_settled_while_observed_states_still_refine_it() {
    let executions = executions();
    executions
        .register_tty(
            None,
            &command(),
            &update("tty-state", SnapshotState::Running, b""),
        )
        .unwrap();
    executions.observe_tty_state("tty-state", FINISHED);
    assert_eq!(executions.state_for("tty-state"), Some(FINISHED));
    executions
        .refresh_tty(&update("tty-state", SnapshotState::Running, b""))
        .unwrap();
    assert_eq!(executions.state_for("tty-state"), Some(FINISHED));
    executions.observe_tty_state("tty-state", EXITED);
    assert_eq!(executions.state_for("tty-state"), Some(EXITED));
}

#[test]
fn an_unpublished_terminal_that_ended_is_forgotten_once_delivered() {
    let executions = executions();
    let ended = executions
        .register_tty(
            None,
            &command(),
            &TtyUpdate {
                published_running: false,
                ..update("tty-quick", EXITED, b"done")
            },
        )
        .unwrap();
    assert!(!ended.retained);
    assert_eq!(ended.output_delta, b"done");
    assert_eq!(executions.backend_for("tty-quick"), None);
    assert_eq!(
        executions.update_tty(&update("tty-quick", EXITED, b"")),
        Err(ExecutionError::ExecutionNotFound)
    );
}

#[test]
fn thirty_two_ended_terminals_stay_retained_and_the_oldest_is_evicted() {
    let executions = executions();
    for index in 0..=MAX_TOMBSTONES {
        let id = format!("tty-tombstone-{index}");
        let ended = executions
            .register_tty(None, &command(), &update(&id, EXITED, b""))
            .unwrap();
        assert!(ended.retained);
        assert!(executions.is_tombstone(&id));
    }
    assert!(!executions.is_tombstone("tty-tombstone-0"));
    assert_eq!(executions.tombstone_snapshot("tty-tombstone-0"), None);
    let newest = format!("tty-tombstone-{MAX_TOMBSTONES}");
    executions.observe_tty_state(&newest, SnapshotState::Lost);
    let retained = executions.tombstone_snapshot(&newest).unwrap();
    assert_eq!(retained.state, EXITED);
    assert_eq!(
        executions.update_tty(&update(&newest, EXITED, b"")),
        Err(ExecutionError::InvalidBackend)
    );
    assert_eq!(
        executions.register_tty(None, &command(), &update(&newest, EXITED, b"")),
        Err(ExecutionError::InvalidBackend)
    );
}

#[test]
fn reserved_terminal_capacity_counts_against_the_live_limit() {
    let executions = executions();
    let mut reserved: Vec<TtyCapacity> = (0..MAX_LIVE_ENTRIES)
        .map(|_| executions.reserve_tty_capacity().unwrap())
        .collect();
    assert!(matches!(
        executions.reserve_tty_capacity(),
        Err(ExecutionError::ExecutionCapacityExceeded)
    ));
    executions
        .register_tty(
            reserved.pop(),
            &command(),
            &update("tty-0", SnapshotState::Running, b""),
        )
        .unwrap();
    assert_eq!(
        executions.register_tty(
            None,
            &command(),
            &update("tty-1", SnapshotState::Running, b"")
        ),
        Err(ExecutionError::ExecutionCapacityExceeded)
    );
    reserved.pop();
    executions
        .register_tty(
            None,
            &command(),
            &update("tty-1", SnapshotState::Running, b""),
        )
        .unwrap();
    executions
        .register_tty(
            reserved.pop(),
            &command(),
            &update("tty-0", SnapshotState::Running, b""),
        )
        .unwrap();
    executions
        .register_tty(
            None,
            &command(),
            &update("tty-2", SnapshotState::Running, b""),
        )
        .unwrap();
    assert_eq!(
        executions.register_tty(
            None,
            &command(),
            &update("tty-3", SnapshotState::Running, b"")
        ),
        Err(ExecutionError::ExecutionCapacityExceeded)
    );
    let other = executions.separate();
    assert_eq!(
        executions.register_tty(
            Some(other.reserve_tty_capacity().unwrap()),
            &command(),
            &update("tty-3", SnapshotState::Running, b""),
        ),
        Err(ExecutionError::InvalidCapacityReservation)
    );
    assert!(other.reserve_tty_capacity().is_ok());
}

#[test]
fn an_external_wait_is_exclusive_until_released_and_can_be_preempted() {
    let executions = executions();
    executions
        .register_tty(
            None,
            &command(),
            &update("tty-wait", SnapshotState::Running, b""),
        )
        .unwrap();
    let waiter = executions.reserve_external_wait("tty-wait").unwrap();
    assert_eq!(
        executions.reserve_external_wait("tty-wait"),
        Err(ExecutionError::ExecutionBusy)
    );
    assert!(!executions.external_wait_preempted("tty-wait", waiter));
    executions.preempt_wait("tty-wait");
    assert!(executions.external_wait_preempted("tty-wait", waiter));
    executions.release_external_wait("tty-wait", waiter);
    let next = executions.reserve_external_wait("tty-wait").unwrap();
    assert_ne!(next, waiter);
    assert!(!executions.external_wait_preempted("tty-wait", next));
    assert_eq!(
        executions.reserve_external_wait("tty-missing"),
        Err(ExecutionError::ExecutionNotFound)
    );
    assert!(executions.external_wait_preempted("tty-missing", next));
}

#[test]
fn a_stopping_runtime_refuses_new_terminals_but_leaves_running_ones_alone() {
    let executions = executions();
    executions
        .register_tty(
            None,
            &command(),
            &update("tty-live", SnapshotState::Running, b""),
        )
        .unwrap();
    assert!(
        block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(1), executions.shutdown()).await
        })
        .is_ok(),
        "the shutdown waited for a terminal execution"
    );
    assert!(matches!(
        executions.reserve_tty_capacity(),
        Err(ExecutionError::RuntimeStopping)
    ));
    assert_eq!(
        executions.register_tty(
            None,
            &command(),
            &update("tty-late", SnapshotState::Running, b"")
        ),
        Err(ExecutionError::RuntimeStopping)
    );
    assert_eq!(
        executions.state_for("tty-live"),
        Some(SnapshotState::Running)
    );
    let settled = executions
        .update_tty(&update("tty-live", FINISHED, b""))
        .unwrap();
    assert_eq!(settled.state, FINISHED);
}

#[test]
fn an_indexed_terminal_keeps_its_output_for_the_next_delivery() {
    let executions = executions();
    executions
        .index_tty(
            Some(executions.reserve_tty_capacity().unwrap()),
            &command(),
            &update("tty-index", EXITED, b"saved"),
        )
        .unwrap();
    assert_eq!(executions.state_for("tty-index"), Some(EXITED));
    assert!(!executions.is_tombstone("tty-index"));
    executions
        .index_tty(
            None,
            &command(),
            &update("tty-index", SnapshotState::Running, b"+"),
        )
        .unwrap();
    let delivered = executions
        .update_tty(&update("tty-index", SnapshotState::Running, b""))
        .unwrap();
    assert_eq!(delivered.output_delta, b"saved+");
    assert_eq!(delivered.state, EXITED);
    assert_eq!(delivered.stdout_bytes, 6);
    assert!(delivered.retained);
    assert!(executions.is_tombstone("tty-index"));
}
