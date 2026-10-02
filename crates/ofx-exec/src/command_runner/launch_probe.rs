use crate::directory_identity::DIRECTORY_CHANGED;

const MAX_NAME_BYTES: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProbeState {
    Matching,
    Passed,
    Failed(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LaunchProbe {
    expected: Vec<u8>,
    held: Vec<u8>,
    state: ProbeState,
}

impl LaunchProbe {
    pub(super) fn new(nonce: &str) -> Self {
        Self {
            expected: super::launch_failure_prefix(nonce),
            held: Vec::new(),
            state: ProbeState::Matching,
        }
    }

    pub(super) fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut emitted = Vec::new();
        for (index, &byte) in bytes.iter().enumerate() {
            match &mut self.state {
                ProbeState::Passed => {
                    emitted.extend_from_slice(&bytes[index..]);
                    break;
                }
                ProbeState::Failed(name) => {
                    if byte != b'\n' && name.len() < MAX_NAME_BYTES {
                        name.push(byte);
                    }
                }
                ProbeState::Matching if self.expected[self.held.len()] == byte => {
                    self.held.push(byte);
                    if self.held.len() == self.expected.len() {
                        self.state = ProbeState::Failed(Vec::new());
                    }
                }
                ProbeState::Matching => {
                    self.state = ProbeState::Passed;
                    emitted.append(&mut self.held);
                    emitted.extend_from_slice(&bytes[index..]);
                    break;
                }
            }
        }
        emitted
    }

    pub(super) fn flush(&mut self) -> Vec<u8> {
        if self.state != ProbeState::Matching {
            return Vec::new();
        }
        self.state = ProbeState::Passed;
        std::mem::take(&mut self.held)
    }

    pub(super) fn launch_failure(&self) -> Option<&'static str> {
        let ProbeState::Failed(name) = &self.state else {
            return None;
        };
        Some(
            LAUNCH_ERROR_NAMES
                .iter()
                .find(|known| known.as_bytes() == name.as_slice())
                .copied()
                .unwrap_or("CommandLaunchFailed"),
        )
    }
}

const LAUNCH_ERROR_NAMES: [&str; 16] = [
    "SystemResources",
    "AccessDenied",
    "PermissionDenied",
    "InvalidExe",
    "FileSystem",
    "IsDir",
    "FileNotFound",
    "NotDir",
    "FileBusy",
    "ProcessFdQuotaExceeded",
    "SystemFdQuotaExceeded",
    "OutOfMemory",
    "NameTooLong",
    "BrokenPipe",
    "Unexpected",
    DIRECTORY_CHANGED,
];

#[cfg(test)]
mod tests {
    use super::*;

    const NONCE: &str = "0123456789abcdef0123456789abcdef";

    fn marker(name: &str) -> Vec<u8> {
        let mut marker = super::super::launch_failure_prefix(NONCE);
        marker.extend_from_slice(name.as_bytes());
        marker.push(b'\n');
        marker
    }

    #[test]
    fn a_complete_marker_names_the_launch_failure_and_emits_nothing() {
        let mut probe = LaunchProbe::new(NONCE);
        let marker = marker("FileNotFound");
        let (head, tail) = marker.split_at(7);
        assert!(probe.feed(head).is_empty());
        assert!(probe.feed(tail).is_empty());
        assert_eq!(probe.launch_failure(), Some("FileNotFound"));
    }

    #[test]
    fn unknown_names_become_a_generic_launch_failure() {
        let mut probe = LaunchProbe::new(NONCE);
        assert!(probe.feed(&marker("Surprise")).is_empty());
        assert_eq!(probe.launch_failure(), Some("CommandLaunchFailed"));
    }

    #[test]
    fn ordinary_stderr_passes_through_including_a_partial_match() {
        let mut probe = LaunchProbe::new(NONCE);
        assert_eq!(probe.feed(b"warning: x\n"), b"warning: x\n");
        assert_eq!(probe.feed(b"\0OH"), b"\0OH");
        assert_eq!(probe.launch_failure(), None);

        let mut probe = LaunchProbe::new(NONCE);
        assert!(probe.feed(b"\0OH_FX").is_empty());
        assert_eq!(probe.feed(b"!rest"), b"\0OH_FX!rest");
        assert_eq!(probe.launch_failure(), None);

        let mut probe = LaunchProbe::new(NONCE);
        assert!(probe.feed(b"\0OH").is_empty());
        assert_eq!(probe.flush(), b"\0OH");
        assert!(probe.flush().is_empty());
        assert_eq!(probe.feed(b"later"), b"later");
    }

    #[test]
    fn a_marker_with_another_nonce_is_ordinary_output() {
        let mut probe = LaunchProbe::new(NONCE);
        let mut forged = super::super::launch_failure_prefix("ffffffffffffffffffffffffffffffff");
        forged.extend_from_slice(b"FileNotFound\n");
        assert_eq!(probe.feed(&forged), forged);
        assert_eq!(probe.launch_failure(), None);
    }
}
