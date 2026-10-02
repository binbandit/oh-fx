use std::env;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use ofx_config::ProfilePaths;
use ofx_upgrade::{UpgradeError, UpgradeLock, UpgradeOutcome, UpgradeProgress};
use serde_json::json;

const CLEAR_LINE: &str = "\r\x1b[K";

pub(crate) fn run_in_background() -> ExitCode {
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
    let mut progress = ProgressLine::new(show_progress);
    let outcome = ofx_upgrade::upgrade(&client, lock, |update| progress.show(update)).await;
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

struct ProgressLine {
    enabled: bool,
    drawn: bool,
    last_percent: Option<u64>,
}

impl ProgressLine {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            drawn: false,
            last_percent: None,
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
                let percent = received * 100 / total;
                if self.last_percent == Some(percent) {
                    return;
                }
                self.last_percent = Some(percent);
                format!("{percent}%")
            }
            UpgradeProgress::Downloading { .. } => "upgrading...".to_owned(),
            UpgradeProgress::Installing if self.last_percent == Some(100) => return,
            UpgradeProgress::Installing => "100%".to_owned(),
        };
        self.draw(&text);
    }

    fn clear(&mut self) {
        if self.drawn {
            self.draw("");
        }
    }

    fn draw(&mut self, text: &str) {
        let mut stderr = io::stderr().lock();
        let _ = write!(stderr, "{CLEAR_LINE}{text}");
        let _ = stderr.flush();
        self.drawn = true;
    }
}
