mod command_contract;
mod command_environment;
mod command_runner;
mod held_directory;
mod managed_execution;
mod native_session;
mod output_echo;
#[cfg(target_os = "linux")]
mod process_tree;
mod self_exe;
mod shell_resolver;
mod terminal_contracts;
mod terminal_observer;
mod terminal_recovery;
mod terminal_store;

pub use command_contract::{CommandStatus, StatusProjection};
pub use command_environment::{Environment, Profile};
pub use command_runner::{
    SessionSupervisor, is_foreground_session_invocation, run_foreground_session,
};
pub use held_directory::HeldDirectory;
pub use managed_execution::{
    Backend, ExecutionError, ManagedExecutions, Persistence, Snapshot, SnapshotState,
    StartCaptured, TtyCapacity, TtyCommand, TtyUpdate,
};
pub use native_session::{
    CloseResult, InspectResult, Outcome, ReadResult, ScreenResult, SignalResult, StartRequest,
    Terminals, WriteResult, is_terminal_control_invocation, is_terminal_launcher_invocation,
    run_terminal_control, run_terminal_launcher,
};
pub use output_echo::OutputEcho;
pub use shell_resolver::{
    ResolveError, check_terminal_shell, configured_login_shell, environment, profile_shell,
};
pub use terminal_contracts::{
    Action, ClosePolicy, Lifecycle, ReturnCondition, ReturnOutcome, SessionFacts, ShellSpec,
    StructuredErrorCode, TerminalFailure, TerminalSignal,
};
pub use terminal_observer::{
    Observation, ObserveError, SyncOutcome, TerminalObserver, snapshot_state,
};
pub use terminal_store::{StoreError, TerminalStore};
