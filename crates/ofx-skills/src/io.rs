use std::fs::{File, Metadata};
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileFreshness {
    device: u64,
    inode: u64,
    size: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl FileFreshness {
    pub(crate) fn of(metadata: &Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }
}

pub(crate) fn read_positional_all(
    file: &File,
    buffer: &mut [u8],
    offset: u64,
) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        let position = offset.saturating_add(u64::try_from(filled).unwrap_or(u64::MAX));
        match file.read_at(&mut buffer[filled..], position) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::time::{Duration, SystemTime};

    use super::*;
    use crate::test_fixture::Fixture;

    #[test]
    fn file_freshness_notices_a_size_or_timestamp_change() {
        let fixture = Fixture::new();
        fixture.write("SKILL.md", "one\n");
        let path = fixture.path("SKILL.md");
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        let freshness = || FileFreshness::of(&file.metadata().unwrap());
        file.set_modified(SystemTime::UNIX_EPOCH).unwrap();
        let before = freshness();
        assert_eq!(before, freshness());

        file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_nanos(1))
            .unwrap();
        let touched = freshness();
        assert_ne!(before, touched);

        file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1))
            .unwrap();
        assert_ne!(touched, freshness());

        let resized = file.metadata().unwrap();
        fs::write(&path, "one\ntwo\n").unwrap();
        file.set_modified(resized.modified().unwrap()).unwrap();
        assert_ne!(FileFreshness::of(&resized), freshness());
    }
}
