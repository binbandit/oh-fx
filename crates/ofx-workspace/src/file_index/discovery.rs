use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use rustix::fs::{AtFlags, FileType, Mode, OFlags, open, statat};

use ofx_text::is_terminal_safe;

use super::{Candidate, CandidateKind, MAX_INDEXED_FILES, accepted_candidate};
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
    roots: &[PathBuf],
    stop: &AtomicBool,
) -> Result<Vec<Candidate>, DiscoveryError> {
    checkpoint(stop)?;
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    let mut succeeded = 0;
    for (index, root) in roots.iter().enumerate() {
        checkpoint(stop)?;
        if candidates.len() >= MAX_INDEXED_FILES {
            break;
        }
        if discover_root(root, index == 0, stop, &mut seen, &mut candidates)? {
            succeeded += 1;
        }
    }
    if succeeded == 0 && !roots.is_empty() {
        return Err(DiscoveryError::Failed);
    }
    Ok(candidates)
}

fn discover_root(
    root: &Path,
    relative: bool,
    stop: &AtomicBool,
    seen: &mut HashSet<PathBuf>,
    candidates: &mut Vec<Candidate>,
) -> Result<bool, DiscoveryError> {
    let options = DiscoveryOptions {
        candidate_cap: MAX_INDEXED_FILES - candidates.len(),
        untracked: UntrackedFiles::Include,
        include_hidden: true,
        sort_paths: true,
        ..DiscoveryOptions::default()
    };
    let Some(files) = discover_listing_files(root, &options) else {
        return Ok(false);
    };
    checkpoint(stop)?;
    let directories = discover_listing_directories(root, &options)
        .map(|listing| listing.files)
        .unwrap_or_default();
    checkpoint(stop)?;
    let Ok(root_directory) = open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    ) else {
        return Ok(false);
    };
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
            || !is_terminal_safe(path.as_bytes())
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
        let absolute = root.join(path);
        let candidate = Candidate {
            path: if relative {
                path.to_owned()
            } else {
                absolute.to_string_lossy().into_owned()
            },
            kind,
        };
        if accepted_candidate(&candidate) && seen.insert(absolute) {
            candidates.push(candidate);
        }
    }
    Ok(true)
}

fn checkpoint(stop: &AtomicBool) -> Result<(), DiscoveryError> {
    if stop.load(Ordering::SeqCst) {
        Err(DiscoveryError::Canceled)
    } else {
        Ok(())
    }
}
