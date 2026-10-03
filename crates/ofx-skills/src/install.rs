mod filesystem;
mod transaction;

use std::fs::File;
use std::io;
use std::os::fd::OwnedFd;
use std::path::Path;

use rustix::fs::{self, FileType, Mode, OFlags};

use crate::skill_contract::{
    MAX_NAME_BYTES, MetadataPrefixError, SKILL_FILE_NAME, parse_skill_file, read_metadata_prefix,
    resolve_metadata,
};
use filesystem::{DIRECTORY, entries, path_directory};

#[derive(Debug, Default)]
pub struct InstallResult {
    pub installed: Vec<String>,
}

pub fn install_local(
    root: &Path,
    source: &Path,
    filter: Option<&str>,
) -> io::Result<InstallResult> {
    let filter = filter.filter(|value| !value.is_empty());
    let input = path_directory(source, false)?;
    let source_path = source.canonicalize()?;
    let verified = path_directory(&source_path, false)?;
    if !same_directory(&input, &verified)? {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let parent_path = root.parent().ok_or(io::ErrorKind::InvalidInput)?;
    let root_name = root.file_name().ok_or(io::ErrorKind::InvalidInput)?;
    preflight_root(root, &input)?;
    let parent = filesystem::path_directory_avoiding(parent_path, true, Some(&input))?;
    filesystem::make_directory(&parent, root_name, Mode::from_raw_mode(0o777))?;
    let output = fs::openat(&parent, root_name, DIRECTORY, Mode::empty())?;
    let locks = transaction::lock_directory(&parent)?;
    let mut result = InstallResult::default();
    let fallback = source_path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or(io::ErrorKind::InvalidInput)?;
    install_candidate(&input, &output, &locks, fallback, filter, &mut result)?;
    walk(&input, &output, &locks, None, filter, &mut result)?;
    Ok(result)
}

fn walk(
    input: &OwnedFd,
    output: &OwnedFd,
    locks: &OwnedFd,
    candidate: Option<&str>,
    filter: Option<&str>,
    result: &mut InstallResult,
) -> io::Result<()> {
    for (name, kind) in entries(input)? {
        if kind == FileType::Directory {
            let directory = fs::openat(input, &name, DIRECTORY, Mode::empty())?;
            walk(&directory, output, locks, name.to_str(), filter, result)?;
        } else if kind == FileType::RegularFile
            && name == SKILL_FILE_NAME
            && let Some(name) = candidate.filter(|name| !name.starts_with('.'))
        {
            install_candidate(input, output, locks, name, filter, result)?;
        }
    }
    Ok(())
}

fn install_candidate(
    input: &OwnedFd,
    output: &OwnedFd,
    locks: &OwnedFd,
    name: &str,
    filter: Option<&str>,
    result: &mut InstallResult,
) -> io::Result<()> {
    if !valid_name(name) {
        return Ok(());
    }
    let file = match fs::openat(
        input,
        SKILL_FILE_NAME,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(file) => File::from(file),
        Err(
            error @ (rustix::io::Errno::MFILE
            | rustix::io::Errno::NFILE
            | rustix::io::Errno::NOMEM
            | rustix::io::Errno::IO),
        ) => {
            return Err(error.into());
        }
        Err(_) => return Ok(()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Ok(());
    }
    let Ok(size) = usize::try_from(metadata.len()) else {
        return Ok(());
    };
    let freshness = crate::io::FileFreshness::of(&metadata);
    let prefix = match read_metadata_prefix(&file, size) {
        Ok(prefix) => prefix,
        Err(MetadataPrefixError::Operational(kind))
            if !matches!(
                kind,
                io::ErrorKind::PermissionDenied
                    | io::ErrorKind::NotFound
                    | io::ErrorKind::UnexpectedEof
            ) =>
        {
            return Err(kind.into());
        }
        Err(_) => return Ok(()),
    };
    let Ok(metadata) = resolve_metadata(&parse_skill_file(&prefix), name.as_bytes()) else {
        return Ok(());
    };
    if filter.is_some_and(|filter| filter != name && filter != metadata.name) {
        return Ok(());
    }
    if crate::io::FileFreshness::of(&file.metadata()?) != freshness {
        return Err(io::ErrorKind::InvalidData.into());
    }
    transaction::replace(
        filesystem::CopySource {
            directory: input,
            skill: Some(freshness),
        },
        output,
        locks,
        name,
    )?;
    result.installed.push(metadata.name);
    Ok(())
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\'])
}

#[cfg(test)]
mod tests;

fn same_directory(left: &OwnedFd, right: &OwnedFd) -> io::Result<bool> {
    let left = fs::fstat(left)?;
    let right = fs::fstat(right)?;
    Ok(left.st_dev == right.st_dev && left.st_ino == right.st_ino)
}

fn preflight_root(root: &Path, source: &OwnedFd) -> io::Result<()> {
    use std::path::Component;
    let absolute = if root.is_absolute() {
        root.to_owned()
    } else {
        std::env::current_dir()?.join(root)
    };
    let mut current = fs::open("/", DIRECTORY, Mode::empty())?;
    for component in absolute.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => match fs::openat(&current, name, DIRECTORY, Mode::empty()) {
                Ok(directory) => {
                    current = directory;
                    if same_directory(&current, source)? {
                        return Err(io::ErrorKind::InvalidInput.into());
                    }
                }
                Err(rustix::io::Errno::NOENT) => return Ok(()),
                Err(error) => return Err(error.into()),
            },
            Component::ParentDir => {
                current = fs::openat(&current, "..", DIRECTORY, Mode::empty())?;
            }
            Component::Prefix(_) => return Err(io::ErrorKind::InvalidInput.into()),
        }
    }
    let mut ancestor = fs::openat(source, ".", DIRECTORY, Mode::empty())?;
    loop {
        if same_directory(&ancestor, &current)? {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let parent = fs::openat(&ancestor, "..", DIRECTORY, Mode::empty())?;
        if same_directory(&ancestor, &parent)? {
            break;
        }
        ancestor = parent;
    }
    Ok(())
}
