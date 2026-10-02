use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use rustix::fs::{AtFlags, FileType, Mode, OFlags, open, statat};

use super::{Candidate, CandidateKind, MAX_INDEXED_FILES, accepted_candidate, is_terminal_safe};
use crate::workspace_files::{
    DiscoveryOptions, UntrackedFiles, discover_listing_directories, discover_listing_files,
};

const GIT_METADATA_NAME: &str = ".git";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DiscoveryError {
    Canceled,
    Failed,
}

pub(super) fn discover_scope(
    root: &Path,
    stop: &AtomicBool,
) -> Result<Vec<Candidate>, DiscoveryError> {
    checkpoint(stop)?;
    let options = DiscoveryOptions {
        candidate_cap: MAX_INDEXED_FILES,
        untracked: UntrackedFiles::Include,
        include_hidden: true,
        sort_paths: true,
        ..DiscoveryOptions::default()
    };
    let files = discover_listing_files(root, &options).ok_or(DiscoveryError::Failed)?;
    checkpoint(stop)?;
    let directories = discover_listing_directories(root, &options)
        .map(|listing| listing.files)
        .unwrap_or_default();
    checkpoint(stop)?;
    let root_directory = open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| DiscoveryError::Failed)?;

    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    let mut files = files.files.iter().peekable();
    let mut directories = directories.iter().peekable();
    loop {
        let (path, kind) = match (files.peek(), directories.peek()) {
            (None, None) => break,
            (Some(file), Some(directory)) if file <= directory => {
                (files.next(), CandidateKind::File)
            }
            (Some(_), None) => (files.next(), CandidateKind::File),
            _ => (directories.next(), CandidateKind::Directory),
        };
        let Some(path) = path else {
            break;
        };
        checkpoint(stop)?;
        if candidates.len() >= MAX_INDEXED_FILES {
            break;
        }
        let Ok(path) = std::str::from_utf8(path) else {
            continue;
        };
        if path
            .split('/')
            .any(|component| component == GIT_METADATA_NAME)
            || !is_terminal_safe(path)
        {
            continue;
        }
        if kind == CandidateKind::File
            && !statat(&root_directory, path, AtFlags::SYMLINK_NOFOLLOW).is_ok_and(|stat| {
                matches!(
                    FileType::from_raw_mode(stat.st_mode),
                    FileType::RegularFile | FileType::Symlink
                )
            })
        {
            continue;
        }
        let candidate = Candidate {
            path: path.to_owned(),
            kind,
        };
        if accepted_candidate(&candidate) && seen.insert(candidate.path.clone()) {
            candidates.push(candidate);
        }
    }
    Ok(candidates)
}

fn checkpoint(stop: &AtomicBool) -> Result<(), DiscoveryError> {
    if stop.load(Ordering::SeqCst) {
        Err(DiscoveryError::Canceled)
    } else {
        Ok(())
    }
}
