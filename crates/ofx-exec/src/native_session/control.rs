use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::terminal_contracts::{
    Dimensions, MAX_AUTHORITY_TEXT_BYTES, MAX_COMMAND_BYTES, MAX_SHELL_PATH_BYTES,
};

pub(crate) const LAUNCHER_MODE: &str = "--oh-fx-internal-terminal-launcher";
pub(crate) const CONTROL_MODE: &str = "--oh-fx-internal-terminal-control";
pub(crate) const NONCE_LEN: usize = 32;
pub(crate) const MARKER_FRAME_LEN: usize = NONCE_LEN + 1;
pub(crate) const MARKER_ACK: u8 = 1;
pub(crate) const LAUNCHER_RELEASE_BYTE: u8 = 1;
pub(crate) const COMMAND_RELEASE_BYTE: u8 = 2;
pub(crate) const CONTROL_FRAME_LEN: usize = 5;
pub(crate) const LAUNCHER_CONFIG_BYTES: usize =
    MAX_COMMAND_BYTES * 6 + MAX_AUTHORITY_TEXT_BYTES * 2 + MAX_SHELL_PATH_BYTES * 2 + 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ControlKind {
    Prepared = 1,
    ShellReady = 2,
    CommandStarted = 3,
    CommandExited = 4,
    CommandSignal = 5,
    StartupFailed = 6,
    InvalidTerm = 7,
}

pub(crate) fn write_control_frame(
    writer: &mut impl Write,
    kind: ControlKind,
    value: u32,
) -> io::Result<()> {
    let mut bytes = [0; CONTROL_FRAME_LEN];
    bytes[0] = kind as u8;
    bytes[1..].copy_from_slice(&value.to_le_bytes());
    writer.write_all(&bytes)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StartupFailure {
    ShellUnavailable = 1,
    ProfileFailed = 2,
    ControlFailed = 3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MarkerKind {
    ShellReady = 1,
    CommandStarted = 2,
}

impl MarkerKind {
    pub(crate) fn event(self) -> &'static str {
        match self {
            Self::ShellReady => "shell-ready",
            Self::CommandStarted => "command-started",
        }
    }

    pub(crate) fn from_event(event: &str) -> Option<Self> {
        [Self::ShellReady, Self::CommandStarted]
            .into_iter()
            .find(|kind| kind.event() == event)
    }

    pub(crate) fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::ShellReady),
            2 => Some(Self::CommandStarted),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LauncherConfig {
    pub(crate) argv: Vec<Vec<u8>>,
    pub(crate) cwd: Vec<u8>,
    pub(crate) dimensions: Dimensions,
    pub(crate) control_path: String,
    pub(crate) control_nonce: String,
    pub(crate) bootstrap_path: String,
    pub(crate) bootstrap: Vec<u8>,
    pub(crate) command_path: String,
    pub(crate) command: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("InvalidLauncherConfig")]
pub(crate) struct InvalidLauncherConfig;

impl LauncherConfig {
    pub(crate) fn read(reader: &mut impl Read) -> Result<Self, InvalidLauncherConfig> {
        let mut length = [0; 4];
        reader
            .read_exact(&mut length)
            .map_err(|_| InvalidLauncherConfig)?;
        let length =
            usize::try_from(u32::from_le_bytes(length)).map_err(|_| InvalidLauncherConfig)?;
        if length == 0 || length > LAUNCHER_CONFIG_BYTES {
            return Err(InvalidLauncherConfig);
        }
        let mut json = vec![0; length];
        reader
            .read_exact(&mut json)
            .map_err(|_| InvalidLauncherConfig)?;
        let config: Self = serde_json::from_slice(&json).map_err(|_| InvalidLauncherConfig)?;
        config.validate()?;
        Ok(config)
    }

    pub(crate) fn cwd(&self) -> &Path {
        Path::new(OsStr::from_bytes(&self.cwd))
    }

    fn validate(&self) -> Result<(), InvalidLauncherConfig> {
        let absolute = self.cwd().is_absolute()
            && [&self.control_path, &self.bootstrap_path, &self.command_path]
                .into_iter()
                .all(|path| Path::new(path).is_absolute());
        if self.argv.is_empty()
            || !absolute
            || self.control_nonce.len() != NONCE_LEN
            || !self.dimensions.is_valid()
        {
            return Err(InvalidLauncherConfig);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn framed(config: &LauncherConfig) -> Vec<u8> {
        let json = serde_json::to_vec(config).unwrap();
        let mut framed = u32::try_from(json.len()).unwrap().to_le_bytes().to_vec();
        framed.extend_from_slice(&json);
        framed
    }

    fn config() -> LauncherConfig {
        LauncherConfig {
            argv: vec![b"/bin/bash".to_vec(), b"-i".to_vec()],
            cwd: b"/tmp".to_vec(),
            dimensions: Dimensions {
                rows: 24,
                columns: 80,
            },
            control_path: "/tmp/control.sock".to_owned(),
            control_nonce: "a".repeat(NONCE_LEN),
            bootstrap_path: "/tmp/bootstrap".to_owned(),
            bootstrap: b"set +x\n".to_vec(),
            command_path: "/tmp/command".to_owned(),
            command: "printf ok".to_owned(),
        }
    }

    #[test]
    fn control_frames_are_a_kind_byte_and_a_little_endian_value() {
        let mut bytes = Vec::new();
        write_control_frame(&mut bytes, ControlKind::CommandExited, 0x0102_0304).unwrap();
        write_control_frame(&mut bytes, ControlKind::Prepared, 0).unwrap();
        assert_eq!(bytes, [4, 4, 3, 2, 1, 1, 0, 0, 0, 0]);
    }

    #[test]
    fn markers_name_their_events_and_bytes() {
        for kind in [MarkerKind::ShellReady, MarkerKind::CommandStarted] {
            assert_eq!(MarkerKind::from_event(kind.event()), Some(kind));
            assert_eq!(MarkerKind::from_byte(kind as u8), Some(kind));
        }
        assert_eq!(MarkerKind::from_event("command-exited"), None);
        assert_eq!(MarkerKind::from_byte(0), None);
        assert_eq!(MarkerKind::from_byte(3), None);
    }

    #[test]
    fn launcher_configs_read_their_length_prefixed_json_with_raw_bytes() {
        let raw = LauncherConfig {
            argv: vec![b"/opt/\xffshell/bash".to_vec(), b"-i".to_vec()],
            cwd: b"/tmp/\xfe".to_vec(),
            ..config()
        };
        assert_eq!(
            LauncherConfig::read(&mut framed(&raw).as_slice()),
            Ok(raw.clone())
        );
        assert_eq!(raw.cwd(), Path::new(OsStr::from_bytes(b"/tmp/\xfe")));
    }

    #[test]
    fn launcher_configs_reject_relative_paths_bad_nonces_and_empty_argv() {
        let invalid = [
            LauncherConfig {
                argv: Vec::new(),
                ..config()
            },
            LauncherConfig {
                cwd: b"tmp".to_vec(),
                ..config()
            },
            LauncherConfig {
                control_path: "control.sock".to_owned(),
                ..config()
            },
            LauncherConfig {
                bootstrap_path: "bootstrap".to_owned(),
                ..config()
            },
            LauncherConfig {
                command_path: "command".to_owned(),
                ..config()
            },
            LauncherConfig {
                control_nonce: "a".repeat(NONCE_LEN - 1),
                ..config()
            },
            LauncherConfig {
                dimensions: Dimensions {
                    rows: 0,
                    columns: 80,
                },
                ..config()
            },
        ];
        for config in invalid {
            assert_eq!(
                LauncherConfig::read(&mut framed(&config).as_slice()),
                Err(InvalidLauncherConfig),
                "{config:?}"
            );
        }
    }

    #[test]
    fn launcher_configs_are_bounded() {
        let mut framed = u32::try_from(LAUNCHER_CONFIG_BYTES + 1)
            .unwrap()
            .to_le_bytes()
            .to_vec();
        framed.extend_from_slice(b"{}");
        for input in [
            framed.as_slice(),
            &[0_u8, 0, 0, 0],
            &[2_u8, 0, 0, 0, b'{'],
            &[1_u8, 0],
        ] {
            assert_eq!(
                LauncherConfig::read(&mut &input[..]),
                Err(InvalidLauncherConfig)
            );
        }
    }
}
