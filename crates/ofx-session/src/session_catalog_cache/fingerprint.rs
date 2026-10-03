use std::os::fd::AsFd;

use ofx_config::PrivateDir;
use rustix::fs::{self, AtFlags, FileType, Stat};
use rustix::io::Errno;
use sha2::{Digest, Sha256};

use crate::session_layout::is_valid_session_id;

pub(crate) type Fingerprint = [u8; 32];

const CLASSIFIED_FILES: [&str; 5] = [
    "session.json",
    "events.jsonl",
    "authority.json",
    "authority.pending.json",
    "display.json",
];
const CHILD_DIR: &str = "subagent";
const CHILD_MARKERS: [&str; 2] = ["owner.json", "control.json"];
const NANOS_PER_SECOND: i128 = 1_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    BlockDevice,
    CharacterDevice,
    Directory,
    NamedPipe,
    SymLink,
    File,
    UnixDomainSocket,
    Unknown,
}

impl Kind {
    fn of(stat: &Stat) -> Self {
        match FileType::from_raw_mode(stat.st_mode) {
            FileType::BlockDevice => Self::BlockDevice,
            FileType::CharacterDevice => Self::CharacterDevice,
            FileType::Directory => Self::Directory,
            FileType::Fifo => Self::NamedPipe,
            FileType::Symlink => Self::SymLink,
            FileType::RegularFile => Self::File,
            FileType::Socket => Self::UnixDomainSocket,
            FileType::Unknown => Self::Unknown,
        }
    }

    fn ordinal(self) -> u128 {
        match self {
            Self::BlockDevice => 0,
            Self::CharacterDevice => 1,
            Self::Directory => 2,
            Self::NamedPipe => 3,
            Self::SymLink => 4,
            Self::File => 5,
            Self::UnixDomainSocket => 6,
            Self::Unknown => 10,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Stamp {
    pub(super) inode: u64,
    pub(super) nlink: u64,
    pub(super) size: u64,
    pub(super) kind: Kind,
    pub(super) mode: u32,
    pub(super) mtime_ns: i128,
    pub(super) ctime_ns: i128,
}

impl Stamp {
    fn of(stat: &Stat) -> Self {
        Self {
            inode: stat.st_ino,
            nlink: u64::from(stat.st_nlink),
            size: u64::from_ne_bytes(stat.st_size.to_ne_bytes()),
            kind: Kind::of(stat),
            mode: u32::from(stat.st_mode),
            mtime_ns: nanoseconds(stat.st_mtime.into(), stat.st_mtime_nsec.into()),
            ctime_ns: nanoseconds(stat.st_ctime.into(), stat.st_ctime_nsec.into()),
        }
    }

    fn is_single_file(&self) -> bool {
        self.kind == Kind::File && self.nlink == 1
    }

    fn hash_into(&self, digest: &mut Sha256) {
        for value in [
            u128::from(self.inode),
            u128::from(self.nlink),
            u128::from(self.size),
            self.kind.ordinal(),
            u128::from(self.mode),
            u128::from_ne_bytes(self.mtime_ns.to_ne_bytes()),
            u128::from_ne_bytes(self.ctime_ns.to_ne_bytes()),
        ] {
            digest.update(value.to_le_bytes());
        }
    }
}

pub(super) struct Observed {
    pub(super) directory: Stamp,
    pub(super) files: [Option<Stamp>; CLASSIFIED_FILES.len()],
    pub(super) child: Option<(Stamp, [Option<Stamp>; CHILD_MARKERS.len()])>,
}

impl Observed {
    pub(super) fn digest(&self) -> Fingerprint {
        let mut digest = Sha256::new();
        self.directory.hash_into(&mut digest);
        for file in &self.files {
            bind(&mut digest, file.as_ref());
        }
        bind(&mut digest, self.child.as_ref().map(|(stamp, _)| stamp));
        if let Some((_, markers)) = &self.child {
            for marker in markers {
                bind(&mut digest, marker.as_ref());
            }
        }
        digest.finalize().into()
    }
}

pub(crate) fn fingerprint(sessions: &PrivateDir, id: &str) -> Option<Fingerprint> {
    if !is_valid_session_id(id) {
        return None;
    }
    let directory = stamp(sessions, id).ok()??;
    if directory.kind != Kind::Directory {
        return None;
    }
    let mut files = [None; CLASSIFIED_FILES.len()];
    for (slot, name) in files.iter_mut().zip(CLASSIFIED_FILES) {
        *slot = stamp(sessions, &format!("{id}/{name}")).ok()?;
        if slot.is_some_and(|file| !file.is_single_file()) {
            return None;
        }
    }
    let child = match stamp(sessions, &format!("{id}/{CHILD_DIR}")).ok()? {
        Some(child) if child.kind == Kind::Directory => {
            let mut markers = [None; CHILD_MARKERS.len()];
            for (slot, name) in markers.iter_mut().zip(CHILD_MARKERS) {
                *slot = stamp(sessions, &format!("{id}/{CHILD_DIR}/{name}")).ok()?;
                if slot.is_some_and(|marker| !marker.is_single_file()) {
                    return None;
                }
            }
            Some((child, markers))
        }
        Some(_) => return None,
        None => None,
    };
    let after = stamp(sessions, id).ok()??;
    (after == directory).then(|| {
        Observed {
            directory,
            files,
            child,
        }
        .digest()
    })
}

fn bind(digest: &mut Sha256, stamp: Option<&Stamp>) {
    match stamp {
        Some(stamp) => {
            digest.update([1]);
            stamp.hash_into(digest);
        }
        None => digest.update([0]),
    }
}

fn stamp(sessions: &PrivateDir, path: &str) -> Result<Option<Stamp>, Errno> {
    match fs::statat(sessions.as_fd(), path, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => Ok(Some(Stamp::of(&stat))),
        Err(Errno::NOENT | Errno::NOTDIR) => Ok(None),
        Err(errno) => Err(errno),
    }
}

fn nanoseconds(seconds: i128, nanoseconds: i128) -> i128 {
    seconds * NANOS_PER_SECOND + nanoseconds
}
