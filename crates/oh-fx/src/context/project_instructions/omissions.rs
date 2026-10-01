use std::cmp::Ordering;
use std::fmt::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use ofx_config::{EMERGENCY_CEILING_BYTES, utf8_prefix_length};
use ofx_text::{lowercase_hex, write_scalar};
use sha2::{Digest, Sha256};

use super::sort_with;

const SOURCE_PREFIX_BYTES: usize = 256;
const RETAINED_RECORDS: usize = 32;
const DIGEST_BYTES: usize = 12;
const REASON_COUNT: usize = 8;
const RECORD_DOMAIN: &[u8] = b"fx.context.omission-record\x00";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum OmissionReason {
    HomeUnavailable,
    HomeOutsideWorkspace,
    UnsafeTarget,
    Unreadable,
    Oversized,
    NonRegular,
    Symlink,
    SelectionCap,
}

const REASONS: [OmissionReason; REASON_COUNT] = [
    OmissionReason::HomeUnavailable,
    OmissionReason::HomeOutsideWorkspace,
    OmissionReason::UnsafeTarget,
    OmissionReason::Unreadable,
    OmissionReason::Oversized,
    OmissionReason::NonRegular,
    OmissionReason::Symlink,
    OmissionReason::SelectionCap,
];

impl OmissionReason {
    fn label(self) -> &'static str {
        match self {
            Self::HomeUnavailable => "home unavailable",
            Self::HomeOutsideWorkspace => "workspace is not below home",
            Self::UnsafeTarget => "unsafe target",
            Self::Unreadable => "unreadable rule file",
            Self::Oversized => "oversized rule file",
            Self::NonRegular => "non-regular rule file",
            Self::Symlink => "symlinked rule file",
            Self::SelectionCap => "selection cap",
        }
    }

    fn repair(self) -> String {
        match self {
            Self::HomeUnavailable => "set HOME to an accessible directory".to_owned(),
            Self::HomeOutsideWorkspace => "open a workspace below HOME".to_owned(),
            Self::UnsafeTarget => "use an absolute local target".to_owned(),
            Self::Unreadable => "make the rule file readable UTF-8".to_owned(),
            Self::Oversized => {
                format!("keep the rule file smaller than {EMERGENCY_CEILING_BYTES} bytes")
            }
            Self::NonRegular => "replace the source with a regular file".to_owned(),
            Self::Symlink => "replace the symlink with a regular file".to_owned(),
            Self::SelectionCap => "reduce applicable AGENTS.md files".to_owned(),
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

pub(super) struct Omissions {
    records: Vec<(PathBuf, OmissionReason)>,
    summarized: usize,
    reason_counts: [usize; REASON_COUNT],
    hasher: Sha256,
}

impl Default for Omissions {
    fn default() -> Self {
        Self {
            records: Vec::new(),
            summarized: 0,
            reason_counts: [0; REASON_COUNT],
            hasher: Sha256::new(),
        }
    }
}

impl Omissions {
    pub(super) fn add(&mut self, source: &Path, reason: OmissionReason, notices: &mut Vec<String>) {
        if self
            .records
            .iter()
            .any(|(recorded, recorded_reason)| *recorded_reason == reason && recorded == source)
        {
            return;
        }
        if self.records.len() == RETAINED_RECORDS {
            self.summarize(source, reason);
            return;
        }
        self.records.push((source.to_path_buf(), reason));
        if reason == OmissionReason::HomeOutsideWorkspace {
            return;
        }
        let mut notice = format!(
            "[context] project instructions action=omitted reason={} source=\"",
            reason.label()
        );
        let truncated = write_bounded_source(&mut notice, source);
        notice.push('"');
        if truncated {
            let _ = write!(
                notice,
                " source_bytes={} source_sha256={}",
                source_bytes(source).len(),
                source_digest(source)
            );
        }
        notice.push_str("; repair=");
        notice.push_str(&reason.repair());
        notices.push(notice);
    }

    pub(super) fn render(mut self, out: &mut String, notices: &mut Vec<String>) {
        sort_with(&mut self.records, record_order);
        for (source, reason) in &self.records {
            append_omission(out, source, *reason);
        }
        if self.summarized == 0 {
            return;
        }
        let digest = hex_prefix(&self.hasher.finalize());
        separate(out);
        let _ = write!(
            out,
            "<project-rules-omitted-summary omitted_count=\"{}\" reasons=\"{}\" records_sha256=\"{digest}\" />",
            self.summarized,
            reason_counts(&self.reason_counts, false)
        );
        let hidden = self.reason_counts[OmissionReason::HomeOutsideWorkspace.index()];
        let actionable = self.summarized - hidden;
        if actionable > 0 {
            notices.push(format!(
                "[context] project instructions action=omitted summary: {actionable} additional records; reasons=\"{}\" records_sha256={digest}; repair=review the listed omission reasons",
                reason_counts(&self.reason_counts, true)
            ));
        }
    }

    fn summarize(&mut self, source: &Path, reason: OmissionReason) {
        let bytes = source_bytes(source);
        self.summarized += 1;
        self.reason_counts[reason.index()] += 1;
        self.hasher.update(RECORD_DOMAIN);
        self.hasher.update([reason as u8]);
        self.hasher.update((bytes.len() as u64).to_be_bytes());
        self.hasher.update(bytes);
    }
}

pub(super) fn append_omission(out: &mut String, source: &Path, reason: OmissionReason) {
    separate(out);
    out.push_str("<project-rules-omitted from=\"");
    let truncated = write_bounded_source(out, source);
    out.push('"');
    if truncated {
        let _ = write!(
            out,
            " source_bytes=\"{}\" source_sha256=\"{}\"",
            source_bytes(source).len(),
            source_digest(source)
        );
    }
    out.push_str(" reason=\"");
    write_scalar(out, reason.label());
    out.push_str("\" />");
}

pub(super) fn write_path(out: &mut String, path: &Path) {
    write_scalar(out, &path.to_string_lossy());
}

pub(super) fn separate(out: &mut String) {
    if !out.is_empty() {
        out.push_str("\n\n");
    }
}

fn write_bounded_source(out: &mut String, source: &Path) -> bool {
    let bytes = source_bytes(source);
    if bytes.len() <= SOURCE_PREFIX_BYTES {
        write_path(out, source);
        return false;
    }
    let prefix = &bytes[..utf8_prefix_length(bytes, SOURCE_PREFIX_BYTES)];
    write_scalar(out, &String::from_utf8_lossy(prefix));
    out.push_str("...");
    true
}

fn reason_counts(counts: &[usize; REASON_COUNT], actionable_only: bool) -> String {
    REASONS
        .iter()
        .filter(|reason| {
            counts[reason.index()] > 0
                && !(actionable_only && **reason == OmissionReason::HomeOutsideWorkspace)
        })
        .map(|reason| format!("{}:{}", reason.label(), counts[reason.index()]))
        .collect::<Vec<_>>()
        .join(", ")
}

fn source_bytes(source: &Path) -> &[u8] {
    source.as_os_str().as_bytes()
}

fn source_digest(source: &Path) -> String {
    hex_prefix(&Sha256::digest(source_bytes(source)))
}

fn record_order(left: &(PathBuf, OmissionReason), right: &(PathBuf, OmissionReason)) -> Ordering {
    source_bytes(&left.0)
        .cmp(source_bytes(&right.0))
        .then(left.1.cmp(&right.1))
}

fn hex_prefix(digest: &[u8]) -> String {
    lowercase_hex(&digest[..DIGEST_BYTES])
}
