use ofx_contract::FileChangeStats;
use serde::Serialize;

use super::{MAX_TEXT_BYTES, WireTag, is_valid_identity, is_valid_path, wire_tag};

const CONTENT_HANDLE_PREFIX: &str = "diff-";
const CONTENT_HANDLE_SUFFIX: &str = ".json";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PresentationKind {
    Added,
    Edited,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LineKind {
    Context,
    Addition,
    Deletion,
    Elision,
    Notice,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(serde::Deserialize), serde(deny_unknown_fields))]
pub(crate) struct PresentationLine {
    #[serde(with = "wire_tag")]
    pub(crate) kind: LineKind,
    #[cfg_attr(test, serde(default))]
    pub(crate) old_line: Option<u32>,
    #[cfg_attr(test, serde(default))]
    pub(crate) new_line: Option<u32>,
    pub(crate) text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(serde::Deserialize), serde(deny_unknown_fields))]
pub(crate) struct LifecycleId {
    pub(crate) turn_id: u64,
    pub(crate) call_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(serde::Deserialize), serde(deny_unknown_fields))]
pub(crate) struct CommittedFilePresentation {
    pub(crate) path: String,
    #[serde(with = "wire_tag")]
    pub(crate) kind: PresentationKind,
    pub(crate) lines: Vec<PresentationLine>,
    pub(crate) additions: u64,
    pub(crate) deletions: u64,
    pub(crate) truncated: bool,
    #[cfg_attr(test, serde(default))]
    pub(crate) previous_content: Option<String>,
    #[cfg_attr(test, serde(default))]
    pub(crate) after_content: Option<String>,
    #[cfg_attr(test, serde(default))]
    pub(crate) lifecycle_id: Option<LifecycleId>,
    #[cfg_attr(test, serde(default))]
    pub(crate) content_handle: Option<String>,
}

impl CommittedFilePresentation {
    pub(crate) fn stats(&self) -> FileChangeStats {
        let count = |lines: u64| usize::try_from(lines).unwrap_or(usize::MAX);
        FileChangeStats::from_lines(count(self.additions), count(self.deletions))
    }

    pub(crate) fn is_valid(&self) -> bool {
        self.has_one_content_source()
            && is_valid_path(&self.path)
            && self
                .lines
                .iter()
                .all(|line| line.text.len() <= MAX_TEXT_BYTES)
            && [&self.previous_content, &self.after_content]
                .into_iter()
                .flatten()
                .all(|content| content.len() <= MAX_TEXT_BYTES)
            && self
                .lifecycle_id
                .as_ref()
                .is_none_or(|id| is_valid_identity(&id.call_id))
            && self.content_handle.as_deref().is_none_or(is_valid_identity)
    }

    fn has_one_content_source(&self) -> bool {
        self.content_handle.as_deref().is_none_or(|handle| {
            self.previous_content.is_none()
                && self.after_content.is_none()
                && handle.starts_with(CONTENT_HANDLE_PREFIX)
                && handle.ends_with(CONTENT_HANDLE_SUFFIX)
        })
    }
}

impl WireTag for PresentationKind {
    const ALL: &'static [Self] = &[Self::Added, Self::Edited];

    fn tag(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Edited => "edited",
        }
    }
}

impl WireTag for LineKind {
    const ALL: &'static [Self] = &[
        Self::Context,
        Self::Addition,
        Self::Deletion,
        Self::Elision,
        Self::Notice,
    ];

    fn tag(self) -> &'static str {
        match self {
            Self::Context => "context",
            Self::Addition => "addition",
            Self::Deletion => "deletion",
            Self::Elision => "elision",
            Self::Notice => "notice",
        }
    }
}
