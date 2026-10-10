#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedResult {
    pub output_handle: Option<String>,
    pub preview: Option<String>,
    pub stored_output_bytes: u64,
    pub truncated: bool,
    pub created_at_ms: i64,
    pub provider_native: bool,
    pub committed_file_presentation: Option<CommittedFilePresentation>,
    pub command_output_replay: Option<CommandOutputReplay>,
    pub tool_images: ToolImages,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommittedFilePresentation {
    pub path: String,
    pub kind: FilePresentationKind,
    pub lines: Vec<FilePresentationLine>,
    pub additions: u64,
    pub deletions: u64,
    pub truncated: bool,
    pub previous_content: Option<String>,
    pub after_content: Option<String>,
    pub lifecycle_id: Option<ToolLifecycleId>,
    pub content_handle: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilePresentationKind {
    Added,
    Edited,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePresentationLine {
    pub kind: FilePresentationLineKind,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilePresentationLineKind {
    Context,
    Addition,
    Deletion,
    Elision,
    Notice,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolLifecycleId {
    pub turn_id: u64,
    pub call_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandOutputReplay {
    Available { handle: String, framed_bytes: u64 },
    Unavailable,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ToolImages {
    #[default]
    None,
    Stored(String),
    Inline(Vec<ToolImage>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolImage {
    pub data: String,
    pub mime_type: String,
    pub source_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageAttachment {
    pub id: u64,
    pub path: String,
    pub media_type: String,
    pub snapshot_path: Option<String>,
    pub snapshot_sha256: Option<String>,
    pub inline_data: Option<Vec<u8>>,
    pub source_ref: Option<String>,
}
