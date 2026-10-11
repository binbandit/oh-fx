mod control;
mod launch;
mod launcher;
mod marker;
mod output;
mod registry;
mod session;

pub(crate) use control::{CONTROL_MODE, MarkerKind};
pub use control::{ControlKind, StartupFailure};
pub use launch::{
    ControlFrame, LaunchError, LaunchedTerminal, Liveness, TerminalLaunch, launch_terminal,
    read_control_frame,
};
pub use launcher::{is_terminal_launcher_invocation, run_terminal_launcher};
pub use marker::{is_terminal_control_invocation, run_terminal_control};
pub use registry::{
    CloseResult, InspectResult, Outcome, ReadResult, ScreenResult, StartRequest, Terminals,
    WriteResult,
};
