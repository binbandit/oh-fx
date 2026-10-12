use std::io::{self, ErrorKind};
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use ofx_trace::trace_log;

const LAUNCHER_WAIT: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const CORE: &str = "core";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Platform {
    MacOs,
    Linux,
    Other,
}

impl Platform {
    const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else {
            Self::Other
        }
    }

    const fn launcher(self) -> Option<&'static str> {
        match self {
            Self::MacOs => Some("open"),
            Self::Linux => Some("xdg-open"),
            Self::Other => None,
        }
    }
}

const NO_OPEN_BROWSER_VARIABLE: &str = "OH_FX_NO_OPEN_BROWSER";

pub fn browser_allowed() -> bool {
    std::env::var_os(NO_OPEN_BROWSER_VARIABLE).is_none()
}

pub fn open_url(url: &str) -> bool {
    let Some(child) = spawn_url(url) else {
        return false;
    };
    settle_later(child);
    true
}

pub fn open_url_bounded(url: &str) -> bool {
    let Some(child) = spawn_url(url) else {
        return false;
    };
    wait_for_launcher(child, LAUNCHER_WAIT)
}

fn spawn_url(url: &str) -> Option<Child> {
    let launcher = Platform::current().launcher()?;
    Command::new(launcher)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .inspect_err(|error| {
            trace_log!(CORE, "url opener launcher failed err={}", error_name(error));
        })
        .ok()
}

fn wait_for_launcher(mut child: Child, limit: Duration) -> bool {
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return settled(status),
            Ok(None) if started.elapsed() < limit => {
                thread::sleep(POLL_INTERVAL.min(limit.saturating_sub(started.elapsed())));
            }
            Ok(None) => {
                settle_later(child);
                return true;
            }
            Err(error) => {
                trace_log!(
                    CORE,
                    "url opener launcher failed err={}",
                    error_name(&error)
                );
                thread::spawn(move || child.wait());
                return false;
            }
        }
    }
}

fn settle_later(mut child: Child) {
    thread::spawn(move || {
        if let Ok(status) = child.wait() {
            settled(status);
        }
    });
}

fn settled(status: ExitStatus) -> bool {
    if let Some(code) = status.code() {
        if code != 0 {
            trace_log!(CORE, "url opener unsuccessful term=exited exit_code={code}");
        }
    } else if let Some(signal) = status.signal() {
        trace_log!(CORE, "url opener unsuccessful term=signal signal={signal}");
    } else if let Some(signal) = status.stopped_signal() {
        trace_log!(CORE, "url opener unsuccessful term=stopped signal={signal}");
    } else {
        trace_log!(
            CORE,
            "url opener unsuccessful term=unknown status={}",
            status.into_raw()
        );
    }
    status.success()
}

fn error_name(error: &io::Error) -> &'static str {
    match error.kind() {
        ErrorKind::NotFound => "FileNotFound",
        ErrorKind::PermissionDenied => "AccessDenied",
        _ => "Unexpected",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRACE_CHILD: &str = "OH_FX_URL_OPENER_TRACE_CHILD";
    const TRACE_TEST: &str = "url_opener::tests::launcher_failures_write_their_core_lines";

    #[test]
    fn launcher_failures_write_their_core_lines() {
        if let Some(root) = std::env::var_os(TRACE_CHILD) {
            ofx_trace::configure_from_env(std::path::Path::new(&root), None);
            assert!(!open_url_bounded(
                "https://example.test/authorize?state=stand-in"
            ));
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let tools = tempfile::tempdir().unwrap();
        for name in ["open", "xdg-open"] {
            let launcher = tools.path().join(name);
            std::fs::write(&launcher, "#!/bin/sh\nexit 3\n").unwrap();
            std::fs::set_permissions(
                &launcher,
                std::os::unix::fs::PermissionsExt::from_mode(0o755),
            )
            .unwrap();
        }
        let empty = tempfile::tempdir().unwrap();
        for (search, line) in [
            (
                tools.path(),
                " [core] url opener unsuccessful term=exited exit_code=3\n",
            ),
            (
                empty.path(),
                " [core] url opener launcher failed err=FileNotFound\n",
            ),
        ] {
            let log = root.path().join("trace.log");
            let status = Command::new(std::env::current_exe().unwrap())
                .args([TRACE_TEST, "--exact", "--test-threads=1"])
                .env_clear()
                .env("PATH", search)
                .env(TRACE_CHILD, root.path())
                .env("OH_FX_TRACE_LOG", &log)
                .stdout(Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "{status:?}");
            let written = std::fs::read_to_string(&log).unwrap_or_default();
            assert!(written.contains(line), "{line}{written}");
            assert!(!written.contains("stand-in"), "{written}");
        }
    }

    #[test]
    fn url_opener_selects_the_platform_launcher_argv() {
        assert_eq!(Platform::MacOs.launcher(), Some("open"));
        assert_eq!(Platform::Linux.launcher(), Some("xdg-open"));
    }

    #[test]
    fn url_opener_reports_unsupported_platforms_without_launching() {
        assert_eq!(Platform::Other.launcher(), None);
    }
}
