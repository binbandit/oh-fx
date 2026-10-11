mod control;
mod launcher;
mod marker;

pub use launcher::{is_terminal_launcher_invocation, run_terminal_launcher};
pub use marker::{is_terminal_control_invocation, run_terminal_control};
