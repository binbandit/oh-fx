use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use ofx_config::{ProfilePaths, Settings};
use ofx_upgrade::{UpgradeControl, UpgradeError, UpgradeLock, UpgradeOutcome, UpgradeProgress};
use serde_json::json;

const CLEAR_LINE: &str = "\r\x1b[K";
const FOUND_HOLD: Duration = Duration::from_millis(150);

pub(crate) fn run_in_background() -> ExitCode {
    if !automatic_upgrades_enabled() {
        return ExitCode::SUCCESS;
    }
    if let Some(lock) = UpgradeLock::try_acquire(&state_directory()) {
        let _ = block_on_upgrade(false, &lock);
    }
    ExitCode::SUCCESS
}

pub(crate) fn run(json: bool) -> ExitCode {
    let outcome = UpgradeLock::acquire(&state_directory())
        .map_err(|_| UpgradeError::ReplaceFailed)
        .and_then(|lock| block_on_upgrade(!json, &lock));
    let report = if json {
        json_report(&outcome)
    } else {
        text_report(&outcome)
    };
    if crate::write_stdout(&format!("{report}\n")).is_err() {
        return crate::write_failed();
    }
    if outcome.is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn automatic_upgrades_enabled() -> bool {
    let workspace = env::current_dir()
        .and_then(fs::canonicalize)
        .unwrap_or_else(|_| PathBuf::from("/"));
    ProfilePaths::from_environment()
        .and_then(|paths| Settings::load(&paths, &workspace).ok())
        .is_none_or(|settings| settings.auto_upgrade_enabled())
}

fn state_directory() -> PathBuf {
    ProfilePaths::from_environment().map_or_else(env::temp_dir, |paths| paths.state)
}

fn block_on_upgrade(
    show_progress: bool,
    lock: &UpgradeLock,
) -> Result<UpgradeOutcome, UpgradeError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| UpgradeError::FetchFailed)?;
    runtime.block_on(upgrade(show_progress, lock))
}

async fn upgrade(show_progress: bool, lock: &UpgradeLock) -> Result<UpgradeOutcome, UpgradeError> {
    let client = ofx_http::build_connection_client(&ofx_http::ConnectionOptions {
        user_agent: ofx_app::user_agent(),
        follow_redirects: true,
        ..ofx_http::ConnectionOptions::default()
    })
    .map_err(|_| UpgradeError::FetchFailed)?;
    let mut progress = ProgressLine::new(show_progress, io::stderr());
    let control = UpgradeControl::new();
    let outcome =
        ofx_upgrade::upgrade(&client, lock, &control, |update| progress.show(update)).await;
    progress.clear();
    outcome
}

fn json_report(outcome: &Result<UpgradeOutcome, UpgradeError>) -> String {
    let (current, latest, status) = match outcome {
        Ok(UpgradeOutcome::Upgraded {
            current, latest, ..
        }) => (current, latest, "upgraded"),
        Ok(UpgradeOutcome::UpToDate { current, latest }) => (current, latest, "up_to_date"),
        Err(error) => return json!({"kind": "upgrade", "error": error.to_string()}).to_string(),
    };
    json!({
        "kind": "upgrade",
        "current": current.to_string(),
        "latest": latest.to_string(),
        "status": status,
    })
    .to_string()
}

fn text_report(outcome: &Result<UpgradeOutcome, UpgradeError>) -> String {
    match outcome {
        Ok(UpgradeOutcome::Upgraded {
            latest, notes_url, ..
        }) => format!("upgraded to v{latest}\nnotes: {notes_url}"),
        Ok(UpgradeOutcome::UpToDate { latest, .. }) => {
            format!("oh-fx is already up to date (v{latest})")
        }
        Err(error) => format!("error: {error}"),
    }
}

struct ProgressLine<W> {
    enabled: bool,
    total_known: bool,
    shown: Option<String>,
    output: W,
}

impl<W: Write> ProgressLine<W> {
    fn new(enabled: bool, output: W) -> Self {
        Self {
            enabled,
            total_known: false,
            shown: None,
            output,
        }
    }

    fn show(&mut self, progress: UpgradeProgress<'_>) {
        if !self.enabled {
            return;
        }
        let text = match progress {
            UpgradeProgress::Found { current, latest } => format!("oh-fx {current} -> {latest}"),
            UpgradeProgress::Downloading {
                received,
                total: Some(total),
            } if total > 0 => {
                self.total_known = true;
                format!("{}%", received.min(total) * 100 / total)
            }
            UpgradeProgress::Installing if self.total_known => "100%".to_owned(),
            UpgradeProgress::Downloading { .. } | UpgradeProgress::Installing => {
                "upgrading...".to_owned()
            }
        };
        if self.shown.as_ref() == Some(&text) {
            return;
        }
        self.draw(&text);
        self.shown = Some(text);
        if matches!(progress, UpgradeProgress::Found { .. }) {
            thread::sleep(FOUND_HOLD);
        }
    }

    fn clear(&mut self) {
        if self.shown.take().is_some() {
            self.draw("");
        }
    }

    fn draw(&mut self, text: &str) {
        let _ = write!(self.output, "{CLEAR_LINE}{text}");
        let _ = self.output.flush();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use semver::Version;

    use super::*;

    #[test]
    fn the_found_line_stays_up_before_the_download_replaces_it_as_upstream_does() {
        let (current, latest) = (Version::new(0, 1, 0), Version::new(0, 2, 0));
        let found = UpgradeProgress::Found {
            current: &current,
            latest: &latest,
        };
        let mut shown = ProgressLine::new(true, Vec::new());
        let started = Instant::now();
        shown.show(found);
        assert!(started.elapsed() >= FOUND_HOLD, "{:?}", started.elapsed());
        assert_eq!(shown.output, b"\r\x1b[Koh-fx 0.1.0 -> 0.2.0");
        let mut quiet = ProgressLine::new(false, Vec::new());
        let started = Instant::now();
        quiet.show(found);
        assert!(started.elapsed() < FOUND_HOLD);
        assert!(quiet.output.is_empty());
    }

    fn drawn(events: &[UpgradeProgress<'_>]) -> String {
        let mut line = ProgressLine::new(true, Vec::new());
        for event in events {
            line.show(*event);
        }
        String::from_utf8(line.output)
            .unwrap()
            .replace(CLEAR_LINE, "|")
    }

    #[test]
    fn download_progress_clamps_to_100_and_unknown_lengths_stay_plain_as_upstream_does() {
        assert_eq!(
            drawn(&[
                UpgradeProgress::Downloading {
                    received: 50,
                    total: Some(200),
                },
                UpgradeProgress::Downloading {
                    received: 300,
                    total: Some(200),
                },
                UpgradeProgress::Installing,
            ]),
            "|25%|100%"
        );
        assert_eq!(
            drawn(&[
                UpgradeProgress::Downloading {
                    received: 50,
                    total: None,
                },
                UpgradeProgress::Installing,
            ]),
            "|upgrading..."
        );
    }
}
