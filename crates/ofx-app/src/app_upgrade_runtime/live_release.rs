use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use ofx_contract::BoxFuture;
use ofx_upgrade::{UpgradeControl, UpgradeError, UpgradeLock, UpgradeOutcome, UpgradeProgress};

use super::session_upgrader::{CheckOutcome, ReleaseCheck};

pub(crate) struct LiveRelease {
    client: reqwest::Client,
    state: PathBuf,
    executable: PathBuf,
    identity: Option<(u64, u64)>,
}

impl LiveRelease {
    pub(crate) fn installed(state: PathBuf) -> Option<Self> {
        Self::at(state, ofx_upgrade::installed_executable().ok()?)
    }

    fn at(state: PathBuf, executable: PathBuf) -> Option<Self> {
        let client = ofx_http::build_connection_client(&ofx_http::ConnectionOptions {
            user_agent: crate::user_agent(),
            follow_redirects: true,
            ..ofx_http::ConnectionOptions::default()
        })
        .ok()?;
        Some(Self {
            client,
            state,
            identity: identity(&executable),
            executable,
        })
    }
}

impl ReleaseCheck for LiveRelease {
    fn check<'a>(
        &'a mut self,
        control: &'a UpgradeControl,
        found: &'a mut (dyn FnMut(&str) + Send),
    ) -> BoxFuture<'a, CheckOutcome> {
        Box::pin(async move {
            if control.stop_requested() {
                return CheckOutcome::Stopped;
            }
            if identity(&self.executable) != self.identity {
                return CheckOutcome::Installed;
            }
            let Some(lock) = UpgradeLock::try_acquire(&self.state) else {
                return CheckOutcome::Current;
            };
            let mut announced = false;
            let outcome = ofx_upgrade::upgrade(&self.client, &lock, control, |progress| {
                if let UpgradeProgress::Found { latest, .. } = progress {
                    announced = true;
                    found(&latest.to_string());
                }
            })
            .await;
            match outcome {
                Ok(UpgradeOutcome::Upgraded { .. }) => CheckOutcome::Installed,
                Err(UpgradeError::Cancelled) => CheckOutcome::Stopped,
                Err(_) if announced => CheckOutcome::Failed,
                Ok(UpgradeOutcome::UpToDate { .. }) | Err(_) => CheckOutcome::Current,
            }
        })
    }
}

fn identity(path: &Path) -> Option<(u64, u64)> {
    fs::metadata(path)
        .ok()
        .map(|metadata| (metadata.dev(), metadata.ino()))
}

#[cfg(test)]
mod tests;
