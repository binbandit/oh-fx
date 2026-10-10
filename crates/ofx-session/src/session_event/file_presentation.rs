use ofx_contract::{
    FileChangeStats, FilePresentationKind, FilePresentationLineKind, SavedFileChange,
};
use serde::Serialize;

use super::{MAX_TEXT_BYTES, WireTag, is_valid_identity, is_valid_path, wire_tag};

const CONTENT_HANDLE_PREFIX: &str = "diff-";
const CONTENT_HANDLE_SUFFIX: &str = ".json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(serde::Deserialize), serde(deny_unknown_fields))]
pub(crate) struct PresentationLine {
    #[serde(with = "wire_tag")]
    pub(crate) kind: FilePresentationLineKind,
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
    pub(crate) kind: FilePresentationKind,
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
    pub(crate) fn saved_change(&self) -> SavedFileChange {
        let count = |lines: u64| usize::try_from(lines).unwrap_or(usize::MAX);
        SavedFileChange {
            path: self.path.clone(),
            stats: FileChangeStats::from_lines(count(self.additions), count(self.deletions)),
        }
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
        has_one_content_source(
            self.content_handle.as_deref(),
            self.previous_content.is_some() || self.after_content.is_some(),
        )
    }
}

pub(crate) fn has_one_content_source(content_handle: Option<&str>, inline: bool) -> bool {
    content_handle.is_none_or(|handle| {
        !inline
            && handle.starts_with(CONTENT_HANDLE_PREFIX)
            && handle.ends_with(CONTENT_HANDLE_SUFFIX)
    })
}

impl From<&ofx_contract::CommittedFilePresentation> for CommittedFilePresentation {
    fn from(presentation: &ofx_contract::CommittedFilePresentation) -> Self {
        Self {
            path: presentation.path.clone(),
            kind: presentation.kind,
            lines: presentation
                .lines
                .iter()
                .map(|line| PresentationLine {
                    kind: line.kind,
                    old_line: line.old_line,
                    new_line: line.new_line,
                    text: line.text.clone(),
                })
                .collect(),
            additions: presentation.additions,
            deletions: presentation.deletions,
            truncated: presentation.truncated,
            previous_content: presentation.previous_content.clone(),
            after_content: presentation.after_content.clone(),
            lifecycle_id: presentation.lifecycle_id.as_ref().map(|id| LifecycleId {
                turn_id: id.turn_id,
                call_id: id.call_id.clone(),
            }),
            content_handle: presentation.content_handle.clone(),
        }
    }
}

impl WireTag for FilePresentationKind {
    const ALL: &'static [Self] = &[Self::Added, Self::Edited];

    fn tag(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Edited => "edited",
        }
    }
}

impl WireTag for FilePresentationLineKind {
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
