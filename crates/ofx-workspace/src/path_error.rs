use std::io;

const EPERM: i32 = 1;
const EIO: i32 = 5;
const ENXIO: i32 = 6;
const ENOMEM: i32 = 12;
const ENODEV: i32 = 19;
const ENFILE: i32 = 23;
const EMFILE: i32 = 24;
#[cfg(target_os = "macos")]
const ELOOP: i32 = 62;
#[cfg(not(target_os = "macos"))]
const ELOOP: i32 = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    #[error("PathOutsideWorkspace")]
    PathOutsideWorkspace,
    #[error("FileNotFound")]
    FileNotFound,
    #[error("HomeNotSet")]
    HomeNotSet,
    #[error("InvalidPath")]
    InvalidPath,
    #[error("WorkspaceUnavailable")]
    WorkspaceUnavailable,
    #[error("AccessDenied")]
    AccessDenied,
    #[error("PermissionDenied")]
    PermissionDenied,
    #[error("NotDir")]
    NotDir,
    #[error("IsDir")]
    IsDir,
    #[error("SymLinkLoop")]
    SymLinkLoop,
    #[error("NameTooLong")]
    NameTooLong,
    #[error("BadPathName")]
    BadPathName,
    #[error("PathAlreadyExists")]
    PathAlreadyExists,
    #[error("InputOutput")]
    InputOutput,
    #[error("NoSpaceLeft")]
    NoSpaceLeft,
    #[error("ReadOnlyFileSystem")]
    ReadOnlyFileSystem,
    #[error("DeviceBusy")]
    DeviceBusy,
    #[error("FileBusy")]
    FileBusy,
    #[error("FileTooBig")]
    FileTooBig,
    #[error("WouldBlock")]
    WouldBlock,
    #[error("NoDevice")]
    NoDevice,
    #[error("SystemResources")]
    SystemResources,
    #[error("OutOfMemory")]
    OutOfMemory,
    #[error("ProcessFdQuotaExceeded")]
    ProcessFdQuotaExceeded,
    #[error("SystemFdQuotaExceeded")]
    SystemFdQuotaExceeded,
    #[error("Unexpected")]
    Unexpected,
}

impl PathError {
    pub fn is_access_denied(self) -> bool {
        matches!(self, Self::AccessDenied | Self::PermissionDenied)
    }

    pub(crate) fn from_realpath(error: &io::Error) -> Self {
        match error.raw_os_error() {
            Some(EPERM) => return Self::PermissionDenied,
            Some(EIO) => return Self::InputOutput,
            Some(ENOMEM) => return Self::OutOfMemory,
            Some(ELOOP) => return Self::SymLinkLoop,
            _ => {}
        }
        match error.kind() {
            io::ErrorKind::NotFound => Self::FileNotFound,
            io::ErrorKind::PermissionDenied => Self::AccessDenied,
            io::ErrorKind::NotADirectory => Self::NotDir,
            io::ErrorKind::InvalidFilename => Self::NameTooLong,
            io::ErrorKind::InvalidInput => Self::BadPathName,
            _ => Self::Unexpected,
        }
    }
}

impl From<io::Error> for PathError {
    fn from(error: io::Error) -> Self {
        Self::from(&error)
    }
}

impl From<&io::Error> for PathError {
    fn from(error: &io::Error) -> Self {
        match error.raw_os_error() {
            Some(EPERM) => return Self::PermissionDenied,
            Some(EIO) => return Self::InputOutput,
            Some(ENXIO | ENODEV) => return Self::NoDevice,
            Some(ENFILE) => return Self::SystemFdQuotaExceeded,
            Some(EMFILE) => return Self::ProcessFdQuotaExceeded,
            Some(ELOOP) => return Self::SymLinkLoop,
            _ => {}
        }
        match error.kind() {
            io::ErrorKind::NotFound => Self::FileNotFound,
            io::ErrorKind::PermissionDenied => Self::AccessDenied,
            io::ErrorKind::NotADirectory => Self::NotDir,
            io::ErrorKind::IsADirectory => Self::IsDir,
            io::ErrorKind::InvalidFilename => Self::NameTooLong,
            io::ErrorKind::InvalidInput => Self::BadPathName,
            io::ErrorKind::AlreadyExists => Self::PathAlreadyExists,
            io::ErrorKind::StorageFull => Self::NoSpaceLeft,
            io::ErrorKind::ReadOnlyFilesystem => Self::ReadOnlyFileSystem,
            io::ErrorKind::ResourceBusy => Self::DeviceBusy,
            io::ErrorKind::ExecutableFileBusy => Self::FileBusy,
            io::ErrorKind::FileTooLarge => Self::FileTooBig,
            io::ErrorKind::WouldBlock => Self::WouldBlock,
            io::ErrorKind::OutOfMemory => Self::SystemResources,
            _ => Self::Unexpected,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_operating_system_errors_to_upstream_error_names() {
        let missing = io::Error::from(io::ErrorKind::NotFound);
        assert_eq!(PathError::from(missing).to_string(), "FileNotFound");
        assert_eq!(
            PathError::from(io::Error::from_raw_os_error(EPERM)),
            PathError::PermissionDenied
        );
        assert_eq!(
            PathError::from(io::Error::from_raw_os_error(13)),
            PathError::AccessDenied
        );
        assert!(PathError::AccessDenied.is_access_denied());
        assert_eq!(
            PathError::from(io::Error::from_raw_os_error(ENOMEM)),
            PathError::SystemResources
        );
        assert!(PathError::PermissionDenied.is_access_denied());
        assert!(!PathError::FileNotFound.is_access_denied());
    }

    #[test]
    fn realpath_failures_follow_upstream_realpath_alloc() {
        for (error, expected) in [
            (io::ErrorKind::NotFound.into(), PathError::FileNotFound),
            (io::ErrorKind::NotADirectory.into(), PathError::NotDir),
            (
                io::ErrorKind::PermissionDenied.into(),
                PathError::AccessDenied,
            ),
            (
                io::ErrorKind::InvalidFilename.into(),
                PathError::NameTooLong,
            ),
            (io::ErrorKind::InvalidInput.into(), PathError::BadPathName),
            (io::Error::from_raw_os_error(ELOOP), PathError::SymLinkLoop),
            (
                io::Error::from_raw_os_error(EPERM),
                PathError::PermissionDenied,
            ),
            (io::Error::from_raw_os_error(EIO), PathError::InputOutput),
            (io::Error::from_raw_os_error(ENOMEM), PathError::OutOfMemory),
            (io::Error::from_raw_os_error(ENXIO), PathError::Unexpected),
            (io::Error::from_raw_os_error(EMFILE), PathError::Unexpected),
        ] {
            let error: io::Error = error;
            assert_eq!(PathError::from_realpath(&error), expected, "{error}");
        }
    }
}
