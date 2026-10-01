use std::fs::{self, File, Metadata};
use std::io;
use std::path::Path;

use rustix::fs::{Mode, OFlags, fcntl_getfl, fcntl_setfl, open};
use rustix::io::Errno;

use crate::path_error::PathError;

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
    if !fs::symlink_metadata(path)?.is_file() {
        return Err(RegularFileError::NotRegularFile);
    }
    open_without_following(path)
}

fn open_without_following(path: &Path) -> Result<(File, Metadata), RegularFileError> {
    let file = File::from(open(path, OPEN_FLAGS, Mode::empty()).map_err(open_failure)?);
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

#[cfg(test)]
mod tests {
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
    }

    fn open_within(path: PathBuf, seconds: u64) -> Option<RegularFileError> {
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let _ = sender.send(open_without_following(&path).err());
        });
        receiver
            .recv_timeout(Duration::from_secs(seconds))
            .expect("the open returns instead of blocking")
    }

    #[test]
    fn opens_regular_files_and_leaves_them_blocking() {
        let fixture = Fixture::new();
        let path = fixture.file("a.txt", "inside\n");

        let (mut file, metadata) = open_regular_file(&path).unwrap();
        let mut content = String::new();
        file.read_to_string(&mut content).unwrap();

        assert!(metadata.is_file());
        assert_eq!(content, "inside\n");
        assert!(!fcntl_getfl(&file).unwrap().contains(OFlags::NONBLOCK));
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
            fifo,
        ] {
            assert_eq!(
                open_regular_file(&path).err(),
                Some(RegularFileError::NotRegularFile),
                "{}",
                path.display()
            );
        }
        assert_eq!(
            open_regular_file(&fixture.root.join("missing.txt")).err(),
            Some(RegularFileError::Path(PathError::FileNotFound))
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
