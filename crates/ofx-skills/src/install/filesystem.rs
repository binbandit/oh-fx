use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path};

use crate::io::FileFreshness;
use rustix::fs::{self, AtFlags, Dir, FileType, Mode, OFlags};

#[derive(Clone, Copy)]
pub(super) struct CopySource<'a> {
    pub(super) directory: &'a OwnedFd,
    pub(super) skill: Option<FileFreshness>,
}

pub(super) const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

pub(super) fn path_directory(path: &Path, create: bool) -> io::Result<OwnedFd> {
    path_directory_avoiding(path, create, None)
}

pub(super) fn path_directory_avoiding(
    path: &Path,
    create: bool,
    source: Option<&OwnedFd>,
) -> io::Result<OwnedFd> {
    if path.as_os_str().is_empty() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut directory = fs::open("/", DIRECTORY, Mode::empty())?;
    for component in absolute.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => {
                if source
                    .is_some_and(|source| super::same_directory(&directory, source).unwrap_or(true))
                {
                    return Err(io::ErrorKind::InvalidInput.into());
                }
                if create {
                    make_directory(&directory, name, Mode::from_raw_mode(0o777))?;
                }
                directory = fs::openat(&directory, name, DIRECTORY, Mode::empty())?;
            }
            Component::ParentDir => {
                directory = fs::openat(&directory, "..", DIRECTORY, Mode::empty())?;
            }
            Component::Prefix(_) => return Err(io::ErrorKind::InvalidInput.into()),
        }
    }
    if source.is_some_and(|source| super::same_directory(&directory, source).unwrap_or(true)) {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    Ok(directory)
}

pub(super) fn make_directory(parent: &OwnedFd, name: &OsStr, mode: Mode) -> io::Result<()> {
    match fs::mkdirat(parent, name, mode) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn entries(directory: &OwnedFd) -> io::Result<Vec<(std::ffi::OsString, FileType)>> {
    let mut result = Vec::new();
    for entry in Dir::read_from(directory)? {
        let entry = entry?;
        let bytes = entry.file_name().to_bytes();
        if bytes != b"." && bytes != b".." {
            result.push((OsStr::from_bytes(bytes).to_owned(), entry.file_type()));
        }
    }
    Ok(result)
}

pub(super) fn copy_tree(source: CopySource<'_>, destination: &OwnedFd) -> io::Result<()> {
    let directory = source.directory;
    let mut copied_skill = false;
    for (name, kind) in entries(directory)? {
        if name.as_bytes().starts_with(b".git") {
            continue;
        }
        match kind {
            FileType::Directory => {
                let input = fs::openat(directory, &name, DIRECTORY, Mode::empty())?;
                fs::mkdirat(destination, &name, Mode::from_raw_mode(0o777))?;
                let output = fs::openat(destination, &name, DIRECTORY, Mode::empty())?;
                copy_tree(
                    CopySource {
                        directory: &input,
                        skill: None,
                    },
                    &output,
                )?;
            }
            FileType::RegularFile => {
                let input = fs::openat(
                    directory,
                    &name,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                    Mode::empty(),
                )?;
                let stat = fs::fstat(&input)?;
                if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                let output = fs::openat(
                    destination,
                    &name,
                    OFlags::WRONLY
                        | OFlags::CREATE
                        | OFlags::EXCL
                        | OFlags::NOFOLLOW
                        | OFlags::CLOEXEC,
                    Mode::from_raw_mode(stat.st_mode & 0o777),
                )?;
                let mut input = File::from(input);
                let expected = (name == "SKILL.md").then_some(source.skill).flatten();
                if expected.is_some_and(|expected| {
                    input
                        .metadata()
                        .map_or(true, |metadata| FileFreshness::of(&metadata) != expected)
                }) {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                let mut output = File::from(output);
                io::copy(&mut input, &mut output)?;
                if expected.is_some_and(|expected| {
                    input
                        .metadata()
                        .map_or(true, |metadata| FileFreshness::of(&metadata) != expected)
                }) {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                output.sync_all()?;
                copied_skill |= name == "SKILL.md";
            }
            _ => {}
        }
    }
    if source.skill.is_some() && !copied_skill {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(())
}

pub(super) fn remove_tree(parent: &OwnedFd, name: &OsStr) -> io::Result<()> {
    let directory = fs::openat(parent, name, DIRECTORY, Mode::empty())?;
    for (child, kind) in entries(&directory)? {
        if kind == FileType::Directory {
            remove_tree(&directory, &child)?;
        } else {
            fs::unlinkat(&directory, &child, AtFlags::empty())?;
        }
    }
    fs::unlinkat(parent, name, AtFlags::REMOVEDIR)?;
    Ok(())
}
