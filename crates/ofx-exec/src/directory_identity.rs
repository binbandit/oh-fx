use std::ffi::OsStr;
use std::fs::{self, Metadata};
use std::os::unix::fs::MetadataExt;

pub(crate) const DIRECTORY_CHANGED: &str = "CommandAuthorityContextMismatch";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DirectoryIdentity {
    device: u64,
    inode: u64,
}

impl DirectoryIdentity {
    pub fn of(metadata: &Metadata) -> Self {
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

    use super::*;

    #[test]
    fn identities_survive_the_supervisor_argument_and_reject_anything_else() {
        let directory = tempfile::tempdir().unwrap();
        let identity = DirectoryIdentity::of(&fs::metadata(directory.path()).unwrap());
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
        let nested = directory.path().join("nested");
        fs::create_dir(&nested).unwrap();
        assert_ne!(
            DirectoryIdentity::of(&fs::metadata(&nested).unwrap()),
            identity
        );
    }
}
