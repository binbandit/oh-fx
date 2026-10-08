use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const LAUNCHER_WAIT: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

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
    let Some(mut child) = spawn_url(url) else {
        return false;
    };
    thread::spawn(move || child.wait());
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
        .ok()
}

fn wait_for_launcher(mut child: Child, limit: Duration) -> bool {
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if started.elapsed() < limit => {
                thread::sleep(POLL_INTERVAL.min(limit.saturating_sub(started.elapsed())));
            }
            result => {
                let opened = result.is_ok();
                thread::spawn(move || child.wait());
                return opened;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
