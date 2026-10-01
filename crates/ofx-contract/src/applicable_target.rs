use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TargetKind {
    File,
    Directory,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ApplicableTarget {
    pub path: PathBuf,
    pub kind: TargetKind,
}
