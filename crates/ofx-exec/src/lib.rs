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
mod terminal_recovery;
mod terminal_store;

pub use command_contract::{CommandStatus, StatusProjection};
pub use command_environment::{Environment, Profile};
pub use command_runner::{
    SessionSupervisor, is_foreground_session_invocation, run_foreground_session,
};
pub use held_directory::HeldDirectory;
pub use managed_execution::{
    ExecutionError, ManagedExecutions, Snapshot, SnapshotState, StartCaptured,
};
pub use native_session::{
    ControlFrame, ControlKind, InspectResult, LaunchError, LaunchedTerminal, Liveness, Outcome,
    StartRequest, StartupFailure, TerminalLaunch, Terminals, is_terminal_control_invocation,
    is_terminal_launcher_invocation, launch_terminal, read_control_frame, run_terminal_control,
    run_terminal_launcher,
};
pub use output_echo::OutputEcho;
pub use shell_resolver::{ResolveError, configured_login_shell, environment};
pub use terminal_contracts::{
    Action, Dimensions, Lifecycle, ReturnCondition, ReturnOutcome, SessionFacts, ShellSpec,
    StructuredErrorCode, TerminalFailure,
};
pub use terminal_store::{
    DurableTerminal, NewTerminal, StoreError, TerminalRecord, TerminalStore, Termination,
};
