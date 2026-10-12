use std::collections::HashSet;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use rustix::fs::{AtFlags, FileType, Mode, OFlags, open, statat};

use ofx_text::is_terminal_safe;
use ofx_trace::trace_log;

use super::{CORE, Candidate, CandidateKind, MAX_INDEXED_FILES, accepted_candidate};
use crate::path_error::PathError;
use crate::workspace_files::{
    CandidatePaths, DiscoveryOptions, UntrackedFiles, discover_listing_directories,
    discover_listing_files,
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
        if discover_root(root, index, stop, &mut seen, &mut candidates)? {
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
    root_index: usize,
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
    let files = match discover_listing_files(root, &options) {
        Ok(files) => files,
        Err(error) => {
            checkpoint(stop)?;
            trace_log!(
                CORE,
                "file index root discovery omitted root={root_index} err={error}"
            );
            return Ok(false);
        }
    };
    checkpoint(stop)?;
    let directories = match discover_listing_directories(root, &options) {
        Ok(listing) => listing.files,
        Err(error) => {
            checkpoint(stop)?;
            trace_log!(
                CORE,
                "file index root directory discovery omitted root={root_index} err={error}"
            );
            CandidatePaths::default()
        }
    };
    checkpoint(stop)?;
    let root_directory = match open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(root_directory) => root_directory,
        Err(errno) => {
            trace_log!(
                CORE,
                "file index root admission omitted root={root_index} err={}",
                PathError::from(std::io::Error::from(errno))
            );
            return Ok(false);
        }
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
        let Some(path) = admitted(&root_directory, path, kind) else {
            continue;
        };
        let absolute = root.join(path);
        let candidate = Candidate {
            path: if root_index == 0 {
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

fn admitted(root_directory: impl AsFd, path: &[u8], kind: CandidateKind) -> Option<&str> {
    if path
        .split(|byte| *byte == b'/')
        .any(|component| component == GIT_METADATA_NAME.as_bytes())
    {
        return None;
    }
    let Some(path) = std::str::from_utf8(path)
        .ok()
        .filter(|path| is_terminal_safe(path.as_bytes()))
    else {
        trace_log!(
            CORE,
            "file index omitted unsafe candidate bytes={} kind={}",
            path.len(),
            kind.label()
        );
        return None;
    };
    if kind == CandidateKind::Directory {
        return Some(path);
    }
    let stat = statat(root_directory, path, AtFlags::SYMLINK_NOFOLLOW)
        .inspect_err(|errno| {
            trace_log!(
                CORE,
                "file index omitted missing candidate bytes={} err={}",
                path.len(),
                PathError::from(std::io::Error::from(*errno))
            );
        })
        .ok()?;
    matches!(
        FileType::from_raw_mode(stat.st_mode),
        FileType::RegularFile | FileType::Symlink
    )
    .then_some(path)
}

fn checkpoint(stop: &AtomicBool) -> Result<(), DiscoveryError> {
    if stop.load(Ordering::SeqCst) {
        Err(DiscoveryError::Canceled)
    } else {
        Ok(())
    }
}
