use std::ffi::OsStr;
use std::fs::{File, Metadata};
use std::io;
use std::os::fd::AsFd;
use std::path::Path;

use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use rustix::io::Errno;

use crate::path_error::PathError;

#[cfg(target_os = "linux")]
const DIRECTORY_ACCESS: OFlags = OFlags::PATH;
#[cfg(not(target_os = "linux"))]
const DIRECTORY_ACCESS: OFlags = OFlags::RDONLY;

pub(crate) const DIRECTORY_FLAGS: OFlags = DIRECTORY_ACCESS
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

const OPEN_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::CLOEXEC)
    .union(OFlags::NOCTTY);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegularFileError {
    NotRegularFile,
    Path(PathError),
}

impl From<io::Error> for RegularFileError {
    fn from(error: io::Error) -> Self {
        Self::Path(error.into())
    }
}

pub fn open_regular_file(path: &Path) -> Result<(File, Metadata), RegularFileError> {
    let (directory, name) = no_symlinks::open_parent(path)?;
    open_regular_file_at(directory, name)
}

pub fn open_regular_file_at(
    directory: impl AsFd,
    name: &OsStr,
) -> Result<(File, Metadata), RegularFileError> {
    verified(no_symlinks::open_regular_entry(directory, name)?)
}

fn verified(file: File) -> Result<(File, Metadata), RegularFileError> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(RegularFileError::NotRegularFile);
    }
    let flags = fcntl_getfl(&file).map_err(io::Error::from)?;
    fcntl_setfl(&file, flags.difference(OFlags::NONBLOCK)).map_err(io::Error::from)?;
    Ok((file, metadata))
}

fn open_failure(errno: Errno) -> RegularFileError {
    match errno {
        Errno::NXIO | Errno::LOOP | Errno::NOTDIR => RegularFileError::NotRegularFile,
        other => io::Error::from(other).into(),
    }
}

mod no_symlinks {
    use std::ffi::OsStr;
    use std::fs::File;
    use std::os::fd::{AsFd, OwnedFd};
    use std::path::{Component, Path};

    use rustix::fs::{AtFlags, FileType, Mode, openat, statat};

    use super::{DIRECTORY_FLAGS, OPEN_FLAGS, PathError, RegularFileError, open_failure};

    pub(super) fn open_regular_entry(
        directory: impl AsFd,
        name: &OsStr,
    ) -> Result<File, RegularFileError> {
        let entry = statat(&directory, name, AtFlags::SYMLINK_NOFOLLOW).map_err(open_failure)?;
        if FileType::from_raw_mode(entry.st_mode) != FileType::RegularFile {
            return Err(RegularFileError::NotRegularFile);
        }
        open_entry(directory, name)
    }

    pub(super) fn open_parent(path: &Path) -> Result<(OwnedFd, &OsStr), RegularFileError> {
        let mut components = path.components();
        if components.next() != Some(Component::RootDir) {
            return Err(RegularFileError::Path(PathError::InvalidPath));
        }
        let mut names = components
            .map(|component| match component {
                Component::Normal(name) => Ok(name),
                _ => Err(RegularFileError::Path(PathError::InvalidPath)),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let name = names.pop().ok_or(RegularFileError::NotRegularFile)?;
        let mut directory =
            rustix::fs::open("/", DIRECTORY_FLAGS, Mode::empty()).map_err(open_failure)?;
        for component in names {
            directory = openat(&directory, component, DIRECTORY_FLAGS, Mode::empty())
                .map_err(open_failure)?;
        }
        Ok((directory, name))
    }

    pub(super) fn open_entry(directory: impl AsFd, name: &OsStr) -> Result<File, RegularFileError> {
        let file = openat(directory, name, OPEN_FLAGS, Mode::empty()).map_err(open_failure)?;
        Ok(File::from(file))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Read;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use tempfile::TempDir;

    use super::*;

    struct Fixture {
        _temp: TempDir,
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = TempDir::new().unwrap();
            let root = fs::canonicalize(temp.path()).unwrap();
            Self { _temp: temp, root }
        }

        fn file(&self, name: &str, content: &str) -> PathBuf {
            let path = self.root.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, content).unwrap();
            path
        }

        fn fifo(&self, name: &str) -> PathBuf {
            let path = self.root.join(name);
            assert!(
                Command::new("mkfifo")
                    .arg(&path)
                    .status()
                    .unwrap()
                    .success()
            );
            path
        }

        fn swap_for_symlink(&self, name: &str, target: &Path) {
            let path = self.root.join(name);
            fs::rename(&path, self.root.join(format!("{name}.moved"))).unwrap();
            symlink(target, path).unwrap();
        }
    }

    fn content(mut file: File) -> String {
        let mut content = String::new();
        file.read_to_string(&mut content).unwrap();
        content
    }

    fn open_after_the_check(path: &Path) -> Result<(File, Metadata), RegularFileError> {
        let (directory, name) = no_symlinks::open_parent(path)?;
        verified(no_symlinks::open_entry(&directory, name)?)
    }

    fn open_within(path: PathBuf, seconds: u64) -> Option<RegularFileError> {
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let _ = sender.send(open_after_the_check(&path).err());
        });
        receiver
            .recv_timeout(Duration::from_secs(seconds))
            .expect("the open returns instead of blocking")
    }

    #[test]
    fn opens_regular_files_and_leaves_them_blocking() {
        let fixture = Fixture::new();
        let path = fixture.file("dir/a.txt", "inside\n");

        let (file, metadata) = open_regular_file(&path).unwrap();

        assert!(metadata.is_file());
        assert!(!fcntl_getfl(&file).unwrap().contains(OFlags::NONBLOCK));
        assert_eq!(content(file), "inside\n");
    }

    #[test]
    fn rejects_symlinks_directories_and_fifos_before_opening() {
        let fixture = Fixture::new();
        let target = fixture.file("target.txt", "target\n");
        symlink(&target, fixture.root.join("link.txt")).unwrap();
        fs::create_dir(fixture.root.join("dir")).unwrap();
        let fifo = fixture.fifo("pipe");

        for path in [
            fixture.root.join("link.txt"),
            fixture.root.join("dir"),
            PathBuf::from("/"),
            fifo,
        ] {
            assert_eq!(
                open_regular_file(&path).err(),
                Some(RegularFileError::NotRegularFile),
                "{}",
                path.display()
            );
        }
        for missing in ["missing.txt", "missing/a.txt"] {
            assert_eq!(
                open_regular_file(&fixture.root.join(missing)).err(),
                Some(RegularFileError::Path(PathError::FileNotFound)),
                "{missing}"
            );
        }
        assert_eq!(
            open_regular_file(Path::new("relative.txt")).err(),
            Some(RegularFileError::Path(PathError::InvalidPath))
        );
    }

    #[test]
    fn the_open_never_follows_a_symlink_swapped_in_after_the_check() {
        let fixture = Fixture::new();
        let outside = Fixture::new();
        let secret = outside.file("secret.txt", "outside secret\n");
        let swapped = fixture.root.join("a.txt");
        symlink(&secret, &swapped).unwrap();

        assert_eq!(
            open_within(swapped, 10),
            Some(RegularFileError::NotRegularFile)
        );
    }

    #[test]
    fn the_open_never_follows_a_parent_directory_swapped_for_a_symlink() {
        for (swapped, beneath) in [("dir", "file.txt"), ("top", "dir/file.txt")] {
            let fixture = Fixture::new();
            let outside = Fixture::new();
            let resolved = fixture.file(&format!("{swapped}/{beneath}"), "inside\n");
            outside.file(beneath, "outside secret\n");

            fixture.swap_for_symlink(swapped, &outside.root);

            assert_eq!(
                open_regular_file(&resolved).err(),
                Some(RegularFileError::NotRegularFile),
                "{}",
                resolved.display()
            );
        }
    }

    #[test]
    fn an_opened_parent_keeps_reading_the_checked_directory_after_a_swap() {
        let fixture = Fixture::new();
        let outside = Fixture::new();
        outside.file("file.txt", "outside secret\n");
        let resolved = fixture.file("dir/file.txt", "inside\n");

        let (directory, name) = no_symlinks::open_parent(&resolved).unwrap();
        fixture.swap_for_symlink("dir", &outside.root);
        let file = no_symlinks::open_entry(&directory, name).unwrap();

        assert_eq!(content(file), "inside\n");
    }

    #[test]
    fn the_open_never_blocks_on_a_fifo_swapped_in_after_the_check() {
        let fixture = Fixture::new();
        let swapped = fixture.fifo("a.txt");

        assert_eq!(
            open_within(swapped, 10),
            Some(RegularFileError::NotRegularFile)
        );
    }

    #[test]
    fn a_regular_file_used_as_a_directory_is_not_a_regular_file() {
        let fixture = Fixture::new();
        let file = fixture.file("a.txt", "inside\n");

        assert_eq!(
            open_within(file.join("x"), 10),
            Some(RegularFileError::NotRegularFile)
        );
    }
}
