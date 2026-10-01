#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandStatus {
    ExitCode(i64),
    Signal(u32),
    Indeterminate,
}
