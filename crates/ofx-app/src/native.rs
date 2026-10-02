use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use ofx_tui::Clipboard;
use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::io::Errno;

const COPY_LIMIT: Duration = Duration::from_secs(5);
const EXIT_POLL: Duration = Duration::from_millis(10);

pub(crate) struct NativeClipboard;

impl Clipboard for NativeClipboard {
    fn copy(&self, text: &str) -> bool {
        clipboard_command(std::env::consts::OS)
            .is_some_and(|argv| copy_through(argv, text.as_bytes(), COPY_LIMIT))
    }
}

fn clipboard_command(os: &str) -> Option<&'static [&'static str]> {
    match os {
        "macos" => Some(&["pbcopy"]),
        "linux" => Some(&["xclip", "-selection", "clipboard"]),
        _ => None,
    }
}

fn copy_through(argv: &[&str], text: &[u8], limit: Duration) -> bool {
    let Some((program, arguments)) = argv.split_first() else {
        return false;
    };
    let Ok(mut child) = Command::new(program)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + limit;
    let written = child
        .stdin
        .take()
        .is_some_and(|stdin| write_before(&stdin, text, deadline));
    let status = if written {
        exit_before(&mut child, deadline)
    } else {
        None
    };
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    status.is_some_and(|status| status.success())
}

fn write_before(stdin: &ChildStdin, mut text: &[u8], deadline: Instant) -> bool {
    if rustix::io::ioctl_fionbio(stdin, true).is_err() {
        return false;
    }
    while !text.is_empty() {
        match rustix::io::write(stdin, text) {
            Ok(0) => return false,
            Ok(written) => text = &text[written..],
            Err(Errno::INTR) => {}
            Err(Errno::AGAIN) => {
                if !writable_before(stdin, deadline) {
                    return false;
                }
            }
            Err(_) => return false,
        }
    }
    true
}

fn writable_before(stdin: &ChildStdin, deadline: Instant) -> bool {
    let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
        return false;
    };
    let Ok(timeout) = Timespec::try_from(remaining) else {
        return false;
    };
    let mut fds = [PollFd::new(stdin, PollFlags::OUT)];
    matches!(
        rustix::event::poll(&mut fds, Some(&timeout)),
        Ok(1..) | Err(Errno::INTR)
    )
}

fn exit_before(child: &mut Child, deadline: Instant) -> Option<ExitStatus> {
    loop {
        if let Some(status) = child.try_wait().ok()? {
            return Some(status);
        }
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())?;
        thread::sleep(remaining.min(EXIT_POLL));
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    const HANG_LIMIT: Duration = Duration::from_secs(1);

    fn large_text() -> String {
        "clipboard ✓\n".repeat(8 * 1024 * 1024 / "clipboard ✓\n".len())
    }

    fn still_running(pid: &str) -> bool {
        Command::new("kill")
            .args(["-0", pid])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    }

    #[test]
    fn native_clipboard_selects_the_platform_command() {
        assert_eq!(clipboard_command("macos"), Some(&["pbcopy"][..]));
        assert_eq!(
            clipboard_command("linux"),
            Some(&["xclip", "-selection", "clipboard"][..])
        );
        assert_eq!(clipboard_command("windows"), None);
        assert_eq!(clipboard_command("wasi"), None);
    }

    #[test]
    fn native_clipboard_accepts_only_a_successful_exit() {
        let directory = tempfile::tempdir().unwrap();
        let cases = [
            ("cat >/dev/null", COPY_LIMIT, true),
            ("cat >/dev/null; exit 1", COPY_LIMIT, false),
            ("cat >/dev/null; kill -TERM $$", COPY_LIMIT, false),
            ("cat >/dev/null; kill -STOP $$", HANG_LIMIT, false),
        ];
        for (index, (script, limit, copied)) in cases.into_iter().enumerate() {
            let pid_file = directory.path().join(index.to_string());
            let script = format!("echo $$ > \"$0\"; {script}");
            assert_eq!(
                copy_through(
                    &["sh", "-c", &script, pid_file.to_str().unwrap()],
                    b"text",
                    limit
                ),
                copied,
                "{script}"
            );
            let pid = fs::read_to_string(&pid_file).unwrap();
            assert!(!still_running(pid.trim()), "{script}");
        }
    }

    #[test]
    fn the_text_reaches_the_command_only_on_its_input() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input");
        let arguments = directory.path().join("arguments");
        let text = "$(touch injected) `touch injected`; touch injected\n-selection primary \u{1b}]52;c;aGk=\u{7}\0";
        assert!(copy_through(
            &[
                "sh",
                "-c",
                "cd \"$(dirname \"$0\")\" && echo \"$#\" > \"$1\" && cat > \"$0\"",
                input.to_str().unwrap(),
                arguments.to_str().unwrap(),
            ],
            text.as_bytes(),
            COPY_LIMIT
        ));
        assert_eq!(fs::read(&input).unwrap(), text.as_bytes());
        assert_eq!(fs::read_to_string(&arguments).unwrap(), "1\n");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);
    }

    #[test]
    fn the_command_reads_the_whole_text_on_its_input() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("clipboard");
        let target_arg = target.to_str().unwrap();
        let text = large_text();
        assert!(copy_through(
            &["sh", "-c", "cat > \"$0\"", target_arg],
            text.as_bytes(),
            COPY_LIMIT
        ));
        assert_eq!(fs::read_to_string(&target).unwrap(), text);
    }

    #[test]
    fn a_missing_command_or_one_that_exits_without_reading_fails() {
        assert!(!copy_through(&[], b"text", COPY_LIMIT));
        assert!(!copy_through(
            &["/nonexistent/oh-fx-clipboard"],
            b"text",
            COPY_LIMIT
        ));
        assert!(!copy_through(
            &["oh-fx-clipboard-missing-from-path"],
            b"text",
            COPY_LIMIT
        ));
        assert!(!copy_through(
            &["true"],
            large_text().as_bytes(),
            COPY_LIMIT
        ));
    }

    #[test]
    fn a_command_that_leaves_a_server_running_still_finishes_the_copy() {
        let directory = tempfile::tempdir().unwrap();
        let server_pid_file = directory.path().join("server");
        assert!(copy_through(
            &[
                "sh",
                "-c",
                "cat >/dev/null; sleep 30 & echo $! > \"$0\"",
                server_pid_file.to_str().unwrap()
            ],
            b"text",
            COPY_LIMIT
        ));
        let server = fs::read_to_string(&server_pid_file).unwrap();
        assert!(still_running(server.trim()));
        assert!(
            Command::new("kill")
                .arg(server.trim())
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn a_command_that_hangs_is_killed_at_the_deadline() {
        let directory = tempfile::tempdir().unwrap();
        for text in [String::from("text"), large_text()] {
            let pid_file = directory.path().join(format!("pid-{}", text.len()));
            let started = Instant::now();
            assert!(!copy_through(
                &[
                    "sh",
                    "-c",
                    "echo $$ > \"$0\"; exec sleep 30",
                    pid_file.to_str().unwrap()
                ],
                text.as_bytes(),
                HANG_LIMIT
            ));
            assert!(started.elapsed() < COPY_LIMIT);
            let pid = fs::read_to_string(&pid_file).unwrap();
            assert!(!still_running(pid.trim()));
        }
    }
}
