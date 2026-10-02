use std::ffi::OsStr;
use std::fs::{self, File, Metadata};
use std::io;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt;
#[cfg(target_os = "linux")]
use std::path::PathBuf;

pub(crate) const DIRECTORY_CHANGED: &str = "CommandAuthorityContextMismatch";

#[derive(Debug)]
pub struct HeldDirectory(File);

impl HeldDirectory {
    pub fn new(directory: OwnedFd) -> Self {
        Self(File::from(directory))
    }

    pub fn is_same_directory(&self, other: &Self) -> bool {
        matches!((self.identity(), other.identity()), (Ok(held), Ok(other)) if held == other)
    }

    pub(crate) fn identity(&self) -> io::Result<DirectoryIdentity> {
        self.0
            .metadata()
            .map(|metadata| DirectoryIdentity::of(&metadata))
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn descriptor_path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.0.as_raw_fd()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DirectoryIdentity {
    device: u64,
    inode: u64,
}

impl DirectoryIdentity {
    fn of(metadata: &Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }

    pub(crate) fn current() -> Option<Self> {
        fs::metadata(".").ok().map(|metadata| Self::of(&metadata))
    }

    pub(crate) fn argument(self) -> String {
        format!("{}:{}", self.device, self.inode)
    }

    pub(crate) fn parse(argument: &OsStr) -> Option<Self> {
        let (device, inode) = argument.to_str()?.split_once(':')?;
        Some(Self {
            device: device.parse().ok()?,
            inode: inode.parse().ok()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::Path;

    use super::*;

    fn held(directory: &Path) -> HeldDirectory {
        HeldDirectory::new(File::open(directory).unwrap().into())
    }

    #[test]
    fn identities_survive_the_supervisor_argument_and_reject_anything_else() {
        let directory = tempfile::tempdir().unwrap();
        let identity = held(directory.path()).identity().unwrap();
        assert_eq!(
            DirectoryIdentity::parse(&OsString::from(identity.argument())),
            Some(identity)
        );
        for invalid in ["", "1", "1:", ":1", "1:2:3", "-1:2", "a:2", "none"] {
            assert_eq!(
                DirectoryIdentity::parse(OsStr::new(invalid)),
                None,
                "{invalid}"
            );
        }
    }

    #[test]
    fn a_held_directory_keeps_its_identity_from_a_directory_recreated_at_its_path() {
        let parent = tempfile::tempdir().unwrap();
        let path = parent.path().join("build");
        fs::create_dir(&path).unwrap();
        let reviewed = held(&path);
        assert!(reviewed.is_same_directory(&held(&path)));
        for _ in 0..20 {
            fs::remove_dir(&path).unwrap();
            fs::create_dir(&path).unwrap();
            assert!(!reviewed.is_same_directory(&held(&path)));
        }
    }
}
