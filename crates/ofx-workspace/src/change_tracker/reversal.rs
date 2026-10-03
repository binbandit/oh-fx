use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::OwnedFd;
use std::time::{SystemTime, UNIX_EPOCH};

use rustix::fs::{AtFlags, FileType, Mode, OFlags, openat, renameat, statat, unlinkat};
use rustix::io::Errno;

use super::FileOperation;
use crate::path_error::PathError;
use crate::pathing::{
    FileIdentity, descriptor_identity, entry_identity, open_child_directory, open_directory,
};

const STAGE_FLAGS: OFlags = OFlags::RDWR
    .union(OFlags::CREATE)
    .union(OFlags::EXCL)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const DEFAULT_FILE_MODE: Mode = Mode::RUSR
    .union(Mode::WUSR)
    .union(Mode::RGRP)
    .union(Mode::WGRP)
    .union(Mode::ROTH)
    .union(Mode::WOTH);
const WRITE_BITS: Mode = Mode::WUSR.union(Mode::WGRP).union(Mode::WOTH);

pub(super) enum Reversal {
    Restored,
    Deleted,
}

pub(super) struct Unavailable;

impl From<Errno> for Unavailable {
    fn from(_: Errno) -> Self {
        Self
    }
}

impl From<io::Error> for Unavailable {
    fn from(_: io::Error) -> Self {
        Self
    }
}

impl From<PathError> for Unavailable {
    fn from(_: PathError) -> Self {
        Self
    }
}

pub(super) fn reverse(operation: &FileOperation) -> Result<Reversal, Unavailable> {
    let (parent, name) = open_verified_parent(operation)?;
    match &operation.previous_content {
        Some(content) => restore(&parent, name, content).map(|()| Reversal::Restored),
        None => delete(&parent, name).map(|()| Reversal::Deleted),
    }
}

fn open_verified_parent(operation: &FileOperation) -> Result<(OwnedFd, &OsStr), Unavailable> {
    let Some((name, parents)) = operation.target.components.split_last() else {
        return Err(Unavailable);
    };
    if parents.len() != operation.parent_identities.len() {
        return Err(Unavailable);
    }
    let mut current = verified(
        open_directory(&operation.target.anchor)?,
        operation.anchor_identity,
    )?;
    for (component, expected) in parents.iter().zip(&operation.parent_identities) {
        current = verified(open_child_directory(&current, component)?, *expected)?;
    }
    Ok((current, name))
}

fn verified(directory: OwnedFd, expected: FileIdentity) -> Result<OwnedFd, Unavailable> {
    if descriptor_identity(&directory)? == expected {
        Ok(directory)
    } else {
        Err(Unavailable)
    }
}

fn restore(parent: &OwnedFd, name: &OsStr, content: &[u8]) -> Result<(), Unavailable> {
    let mode = match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile => {
            let mode = Mode::from_raw_mode(stat.st_mode);
            if !mode.intersects(WRITE_BITS) {
                return Err(Unavailable);
            }
            mode
        }
        Ok(_) | Err(Errno::NOENT) => DEFAULT_FILE_MODE,
        Err(_) => return Err(Unavailable),
    };
    let stage = stage_name(name);
    let descriptor = openat(parent, &stage, STAGE_FLAGS, mode)?;
    let identity = descriptor_identity(&descriptor).ok();
    let placed = write_and_place(parent, &stage, name, File::from(descriptor), content);
    if placed.is_err() && identity.is_some() && entry_identity(parent, &stage).ok() == identity {
        let _ = unlinkat(parent, &stage, AtFlags::empty());
    }
    placed
}

fn write_and_place(
    parent: &OwnedFd,
    stage: &OsStr,
    name: &OsStr,
    mut file: File,
    content: &[u8],
) -> Result<(), Unavailable> {
    file.write_all(content)?;
    file.sync_all()?;
    drop(file);
    renameat(parent, stage, parent, name)?;
    Ok(())
}

fn delete(parent: &OwnedFd, name: &OsStr) -> Result<(), Unavailable> {
    match unlinkat(parent, name, AtFlags::empty()) {
        Ok(()) | Err(Errno::NOENT) => Ok(()),
        Err(_) => Err(Unavailable),
    }
}

fn stage_name(name: &OsStr) -> OsString {
    let mut stage = name.to_owned();
    stage.push(format!(".tmp.{}", nano_timestamp()));
    stage
}

fn nano_timestamp() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos())
}
