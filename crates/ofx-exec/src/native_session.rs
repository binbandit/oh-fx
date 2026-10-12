mod control;
mod descriptor_budget;
mod launch;
mod launcher;
mod marker;
mod output;
mod processes;
mod registry;
mod session;

pub(crate) use control::{CONTROL_MODE, MarkerKind};
pub use launcher::{is_terminal_launcher_invocation, run_terminal_launcher};
pub use marker::{is_terminal_control_invocation, run_terminal_control};
pub use registry::{
    CloseResult, InspectResult, Outcome, ReadResult, ScreenResult, SignalResult, StartRequest,
    Terminals, WriteResult,
};
