use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Profile {
    Clean,
    User,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Environment {
    Clean(PathBuf),
    User(PathBuf),
}

impl Environment {
    pub(crate) fn shell_path(&self) -> &Path {
        match self {
            Self::Clean(path) | Self::User(path) => path,
        }
    }
}
