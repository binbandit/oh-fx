use std::fmt::Write as _;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;
use std::process::{Child, ChildStderr, ChildStdin, Command, Stdio};

use rustix::fs::{Mode, OFlags};
use rustix::termios::{Winsize, tcsetwinsize};

use super::control::{
    COMMAND_RELEASE_BYTE, CONTROL_FRAME_LEN, ControlKind, LAUNCHER_CONFIG_BYTES, LAUNCHER_MODE,
    LAUNCHER_RELEASE_BYTE, LauncherConfig,
};
use crate::command_runner::error_name;
use crate::shell_resolver::{
    ResolveError, build_bootstrap, build_source_command, interactive_invocation,
};
use crate::terminal_contracts::Dimensions;

const RANDOM_BYTES: usize = 16;
const PRIVATE_PATH_PREFIX: &str = "/tmp/oh-fx-terminal-";
const PTY_MULTIPLEXER: &str = "/dev/ptmx";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ControlFrame {
    pub(crate) kind: ControlKind,
    pub(crate) value: u32,
}

pub(crate) fn read_control_frame(reader: &mut impl Read) -> io::Result<ControlFrame> {
    let mut bytes = [0; CONTROL_FRAME_LEN];
    reader.read_exact(&mut bytes)?;
    let [kind, value @ ..] = bytes;
    let kind = match kind {
        1 => ControlKind::Prepared,
        2 => ControlKind::ShellReady,
        3 => ControlKind::CommandStarted,
        4 => ControlKind::CommandExited,
        5 => ControlKind::CommandSignal,
        6 => ControlKind::StartupFailed,
        7 => ControlKind::InvalidTerm,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "InvalidControlFrame",
            ));
        }
    };
    Ok(ControlFrame {
        kind,
        value: u32::from_le_bytes(value),
    })
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TerminalLaunch<'a> {
    pub(crate) executable: &'a Path,
    pub(crate) shell: &'a Path,
    pub(crate) clean_start: bool,
    pub(crate) cwd: &'a Path,
    pub(crate) command: &'a str,
    pub(crate) dimensions: Dimensions,
}

#[derive(Debug)]
pub(crate) struct LaunchedTerminal {
    pub(crate) master: OwnedFd,
    pub(crate) control: ChildStderr,
    pub(crate) liveness: Liveness,
    pub(crate) launcher: Child,
}

#[derive(Debug)]
pub(crate) struct Liveness(ChildStdin);

impl Liveness {
    pub(crate) fn release_launcher(&mut self) -> io::Result<()> {
        self.0.write_all(&[LAUNCHER_RELEASE_BYTE])
    }

    pub(crate) fn release_command(&mut self) -> io::Result<()> {
        self.0.write_all(&[COMMAND_RELEASE_BYTE])
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum LaunchError {
    #[error("{0}")]
    Resolve(#[from] ResolveError),
    #[error("PtyUnavailable")]
    PtyUnavailable,
    #[error("ResizeFailed")]
    ResizeFailed,
    #[error("LauncherConfigTooLarge")]
    LauncherConfigTooLarge,
    #[error("LauncherNotPrepared")]
    LauncherNotPrepared,
    #[error("{0}")]
    Failed(&'static str),
}

impl From<io::Error> for LaunchError {
    fn from(error: io::Error) -> Self {
        Self::Failed(error_name(&error))
    }
}

pub(crate) fn launch_terminal(
    request: &TerminalLaunch<'_>,
) -> Result<LaunchedTerminal, LaunchError> {
    let config = encode_config(&launcher_config(request)?)?;
    let (master, slave) = open_pty()?;
    tcsetwinsize(&slave, winsize(request.dimensions)).map_err(|_| LaunchError::ResizeFailed)?;
    let mut launcher = Command::new(request.executable)
        .arg(LAUNCHER_MODE)
        .stdin(Stdio::piped())
        .stdout(Stdio::from(slave))
        .stderr(Stdio::piped())
        .spawn()?;
    let (Some(mut liveness), Some(mut control)) = (launcher.stdin.take(), launcher.stderr.take())
    else {
        abandon(&mut launcher);
        return Err(LaunchError::LauncherNotPrepared);
    };
    if let Err(error) = liveness.write_all(&config) {
        abandon(&mut launcher);
        return Err(error.into());
    }
    if !read_control_frame(&mut control).is_ok_and(|frame| frame.kind == ControlKind::Prepared) {
        abandon(&mut launcher);
        return Err(LaunchError::LauncherNotPrepared);
    }
    Ok(LaunchedTerminal {
        master,
        control,
        liveness: Liveness(liveness),
        launcher,
    })
}

fn launcher_config(request: &TerminalLaunch<'_>) -> Result<LauncherConfig, LaunchError> {
    let mut argv = interactive_invocation(request.shell, request.clean_start)?;
    let control_nonce = random_hex()?;
    let suffix = random_hex()?;
    let private_path = |extension: &str| format!("{PRIVATE_PATH_PREFIX}{suffix}.{extension}");
    let control_path = private_path("sock");
    let bootstrap_path = private_path("bootstrap");
    let command_path = private_path("command");
    let bootstrap = build_bootstrap(
        request.executable.as_os_str().as_bytes(),
        &control_path,
        &control_nonce,
        &command_path,
    );
    argv.push("-c".into());
    let mut argv: Vec<Vec<u8>> = argv.into_iter().map(OsStringExt::into_vec).collect();
    argv.push(build_source_command(&bootstrap_path));
    Ok(LauncherConfig {
        argv,
        cwd: request.cwd.as_os_str().as_bytes().to_vec(),
        dimensions: request.dimensions,
        control_path,
        control_nonce,
        bootstrap_path,
        bootstrap,
        command_path,
        command: request.command.to_owned(),
    })
}

fn encode_config(config: &LauncherConfig) -> Result<Vec<u8>, LaunchError> {
    let json = serde_json::to_vec(config).map_err(|_| LaunchError::LauncherConfigTooLarge)?;
    if json.len() > LAUNCHER_CONFIG_BYTES {
        return Err(LaunchError::LauncherConfigTooLarge);
    }
    let length = u32::try_from(json.len()).map_err(|_| LaunchError::LauncherConfigTooLarge)?;
    let mut framed = length.to_le_bytes().to_vec();
    framed.extend_from_slice(&json);
    Ok(framed)
}

fn random_hex() -> Result<String, LaunchError> {
    let mut bytes = [0_u8; RANDOM_BYTES];
    getrandom::fill(&mut bytes).map_err(|_| LaunchError::Failed("SystemResources"))?;
    Ok(bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    }))
}

fn open_pty() -> Result<(OwnedFd, OwnedFd), LaunchError> {
    let flags = OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC;
    let master = rustix::fs::open(PTY_MULTIPLEXER, flags | OFlags::NONBLOCK, Mode::empty())
        .map_err(|_| LaunchError::PtyUnavailable)?;
    rustix::pty::grantpt(&master).map_err(|_| LaunchError::PtyUnavailable)?;
    rustix::pty::unlockpt(&master).map_err(|_| LaunchError::PtyUnavailable)?;
    let name =
        rustix::pty::ptsname(&master, Vec::new()).map_err(|_| LaunchError::PtyUnavailable)?;
    let slave = rustix::fs::open(name.as_c_str(), flags, Mode::empty())
        .map_err(|error| LaunchError::from(io::Error::from(error)))?;
    Ok((master, slave))
}

fn winsize(dimensions: Dimensions) -> Winsize {
    Winsize {
        ws_row: dimensions.rows,
        ws_col: dimensions.columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

fn abandon(launcher: &mut Child) {
    let _ = launcher.kill();
    let _ = launcher.wait();
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use rustix::io::FdFlags;

    use super::super::control::write_control_frame;
    use super::*;

    fn request<'a>(shell: &'a Path, command: &'a str) -> TerminalLaunch<'a> {
        TerminalLaunch {
            executable: Path::new("/opt/oh fx/bin/oh-fx"),
            shell,
            clean_start: true,
            cwd: Path::new("/workspace"),
            command,
            dimensions: Dimensions {
                rows: 24,
                columns: 80,
            },
        }
    }

    #[test]
    fn control_frames_read_back_as_written_and_reject_unknown_kinds() {
        let mut bytes = Vec::new();
        for kind in [
            ControlKind::Prepared,
            ControlKind::ShellReady,
            ControlKind::CommandStarted,
            ControlKind::CommandExited,
            ControlKind::CommandSignal,
            ControlKind::StartupFailed,
            ControlKind::InvalidTerm,
        ] {
            write_control_frame(&mut bytes, kind, 0x0102_0304).unwrap();
            assert_eq!(
                read_control_frame(&mut bytes.as_slice()).unwrap(),
                ControlFrame {
                    kind,
                    value: 0x0102_0304
                }
            );
            bytes.clear();
        }
        for unknown in [0_u8, 8, 255] {
            let error = read_control_frame(&mut [unknown, 0, 0, 0, 0].as_slice()).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        }
        let short = read_control_frame(&mut [1_u8, 0].as_slice()).unwrap_err();
        assert_eq!(short.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn the_launcher_config_names_private_paths_and_sources_the_bootstrap() {
        let config = launcher_config(&request(Path::new("/bin/bash"), "printf ok")).unwrap();
        let suffix = config
            .control_path
            .strip_prefix(PRIVATE_PATH_PREFIX)
            .and_then(|rest| rest.strip_suffix(".sock"))
            .unwrap();
        assert_eq!(suffix.len(), RANDOM_BYTES * 2);
        assert!(suffix.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(
            config.bootstrap_path,
            format!("{PRIVATE_PATH_PREFIX}{suffix}.bootstrap")
        );
        assert_eq!(
            config.command_path,
            format!("{PRIVATE_PATH_PREFIX}{suffix}.command")
        );
        assert_eq!(config.control_nonce.len(), RANDOM_BYTES * 2);
        assert_ne!(config.control_nonce, suffix);
        let source = format!(". '{}'\n", config.bootstrap_path);
        assert_eq!(
            config.argv,
            [
                b"/bin/bash".as_slice(),
                b"--noprofile",
                b"--norc",
                b"-i",
                b"-c",
                source.as_bytes()
            ]
        );
        assert_eq!(
            config.bootstrap,
            build_bootstrap(
                b"/opt/oh fx/bin/oh-fx",
                &config.control_path,
                &config.control_nonce,
                &config.command_path
            )
        );
        assert_eq!(config.command, "printf ok");
        assert_eq!(config.cwd, b"/workspace");
    }

    #[test]
    fn launcher_configs_keep_paths_that_are_not_utf8() {
        let executable = Path::new(OsStr::from_bytes(b"/opt/\xffbin/oh-fx"));
        let cwd = Path::new(OsStr::from_bytes(b"/work/\xfe"));
        let config = launcher_config(&TerminalLaunch {
            executable,
            cwd,
            ..request(Path::new("/bin/zsh"), "true")
        })
        .unwrap();
        assert_eq!(config.cwd, b"/work/\xfe");
        let quoted = b"'/opt/\xffbin/oh-fx'";
        assert!(
            config
                .bootstrap
                .windows(quoted.len())
                .any(|window| window == quoted)
        );
        let encoded = encode_config(&config).unwrap();
        assert_eq!(
            LauncherConfig::read(&mut encoded.as_slice()).unwrap(),
            config
        );
    }

    #[test]
    fn launches_refuse_unsupported_shells_and_oversized_commands_before_any_process() {
        for (shell, error) in [
            (
                "bash",
                LaunchError::Resolve(ResolveError::RelativeShellPath),
            ),
            (
                "/bin/fish",
                LaunchError::Resolve(ResolveError::UnsupportedShell),
            ),
        ] {
            assert_eq!(
                launch_terminal(&request(Path::new(shell), "true")).unwrap_err(),
                error
            );
        }
        let oversized = "\u{1}".repeat(LAUNCHER_CONFIG_BYTES / 6 + 1);
        assert_eq!(
            launch_terminal(&request(Path::new("/bin/bash"), &oversized)).unwrap_err(),
            LaunchError::LauncherConfigTooLarge
        );
    }

    #[test]
    fn the_pty_master_is_nonblocking_and_close_on_exec() {
        let (master, slave) = open_pty().unwrap();
        assert!(
            rustix::fs::fcntl_getfl(&master)
                .unwrap()
                .contains(OFlags::NONBLOCK)
        );
        for fd in [&master, &slave] {
            assert!(
                rustix::io::fcntl_getfd(fd)
                    .unwrap()
                    .contains(FdFlags::CLOEXEC)
            );
        }
    }
}
