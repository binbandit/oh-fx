use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceMenu {
    pub primary: PathBuf,
    pub saved_suppressed: bool,
    pub limit: usize,
    pub entries: Vec<WorkspaceMenuEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceMenuEntry {
    pub path: PathBuf,
    pub saved: bool,
    pub command_line: bool,
    pub access: DirectoryAccess,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectoryAccess {
    Active,
    Inactive,
    Unavailable,
}

impl DirectoryAccess {
    pub fn of(available: bool, active: bool) -> Self {
        match (available, active) {
            (false, _) => Self::Unavailable,
            (true, true) => Self::Active,
            (true, false) => Self::Inactive,
        }
    }
}
