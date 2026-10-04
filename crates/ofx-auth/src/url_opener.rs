use std::process::{Command, Stdio};
use std::thread;

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
    let Some(launcher) = Platform::current().launcher() else {
        return false;
    };
    let child = Command::new(launcher)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match child {
        Ok(mut child) => {
            thread::spawn(move || child.wait());
            true
        }
        Err(_) => false,
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
