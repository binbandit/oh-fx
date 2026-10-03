use std::future::Future;
use std::sync::{Mutex, PoisonError};

use tokio_util::sync::CancellationToken;

use crate::error::UpgradeError;

#[derive(Debug, Default)]
pub struct UpgradeControl {
    stop: CancellationToken,
    install: Mutex<()>,
}

impl UpgradeControl {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn request_stop(&self) {
        self.stop.cancel();
    }

    pub fn stop_for_process_exit(&self) {
        self.request_stop();
        drop(self.install.lock().unwrap_or_else(PoisonError::into_inner));
    }

    pub fn stop_requested(&self) -> bool {
        self.stop.is_cancelled()
    }

    pub(crate) fn check(&self) -> Result<(), UpgradeError> {
        if self.stop_requested() {
            return Err(UpgradeError::Cancelled);
        }
        Ok(())
    }

    pub(crate) async fn unless_stopped<F: Future>(
        &self,
        transfer: F,
    ) -> Result<F::Output, UpgradeError> {
        self.stop
            .run_until_cancelled(transfer)
            .await
            .ok_or(UpgradeError::Cancelled)
    }

    pub(crate) fn install_unless_stopped<T>(
        &self,
        install: impl FnOnce() -> Result<T, UpgradeError>,
    ) -> Result<T, UpgradeError> {
        let _held = self.install.lock().unwrap_or_else(PoisonError::into_inner);
        self.check()?;
        install()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::Duration;

    use super::*;

    #[test]
    fn a_process_exit_stop_waits_out_an_install_already_under_way() {
        let control = Arc::new(UpgradeControl::new());
        let started = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let installer = {
            let (control, started, done) = (
                Arc::clone(&control),
                Arc::clone(&started),
                Arc::clone(&done),
            );
            thread::spawn(move || {
                control.install_unless_stopped(|| {
                    started.store(true, Ordering::Release);
                    thread::sleep(Duration::from_millis(50));
                    done.store(true, Ordering::Release);
                    Ok(())
                })
            })
        };
        while !started.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(1));
        }
        control.stop_for_process_exit();
        assert!(done.load(Ordering::Acquire));
        assert!(installer.join().unwrap().is_ok());
    }

    #[test]
    fn no_install_starts_after_a_process_exit_stop() {
        let control = UpgradeControl::new();
        control.stop_for_process_exit();
        assert!(control.stop_requested());
        let installed = control.install_unless_stopped(|| Ok(()));
        assert!(matches!(installed, Err(UpgradeError::Cancelled)));
        assert!(matches!(control.check(), Err(UpgradeError::Cancelled)));
    }

    #[tokio::test]
    async fn a_stop_ends_a_transfer_that_never_finishes() {
        let control = Arc::new(UpgradeControl::new());
        let stopper = Arc::clone(&control);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            stopper.request_stop();
        });
        let transfer = control.unless_stopped(std::future::pending::<()>());
        let outcome = tokio::time::timeout(Duration::from_secs(5), transfer)
            .await
            .expect("the stop ends the transfer");
        assert!(matches!(outcome, Err(UpgradeError::Cancelled)));
    }
}
