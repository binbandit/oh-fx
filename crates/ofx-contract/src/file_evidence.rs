use std::fmt::Write;

use crate::types::ToolResultStatus;

const CONTEXT_HEADER: &str = "Session file evidence from previous tool execution. Re-read stale paths before relying on exact contents:";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FileEvidenceAction {
    Read,
    Write,
    Edit,
    Delete,
    Rename,
    Copy,
    Search,
    List,
    #[default]
    Unknown,
}

impl FileEvidenceAction {
    pub const ALL: [Self; 9] = [
        Self::Read,
        Self::Write,
        Self::Edit,
        Self::Delete,
        Self::Rename,
        Self::Copy,
        Self::Search,
        Self::List,
        Self::Unknown,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Edit => "edit",
            Self::Delete => "delete",
            Self::Rename => "rename",
            Self::Copy => "copy",
            Self::Search => "search",
            Self::List => "list",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEvidence {
    pub path: String,
    pub new_path: Option<String>,
    pub tool_call_id: String,
    pub tool_name: String,
    pub action: FileEvidenceAction,
    pub status: ToolResultStatus,
    pub model_view_covers_full_file: bool,
    pub stale: bool,
}

pub fn file_evidence_context(files: &[FileEvidence]) -> String {
    let mut out = CONTEXT_HEADER.to_owned();
    for file in files {
        let _ = write!(
            out,
            "\n- action={} status={} path={}",
            file.action.label(),
            file.status.label(),
            file.path
        );
        if let Some(new_path) = &file.new_path {
            let _ = write!(out, " new_path={new_path}");
        }
        if file.model_view_covers_full_file {
            out.push_str(" model_view=full");
        }
        if file.stale {
            out.push_str(" stale=true");
        }
        let _ = write!(out, " tool={}", file.tool_name);
    }
    out
}

#[cfg(test)]
mod tests;
