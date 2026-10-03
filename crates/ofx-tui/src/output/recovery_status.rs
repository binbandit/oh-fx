use ofx_contract::{ModelRecoveryAction, RouteRecoveryKind, RouteRecoveryStatus};

use super::activity_status::static_status_rows;
use crate::row_text::Row;
use crate::theme::Theme;

const RECOVERED_VISIBLE_MS: i64 = 1_500;
const SECOND_MS: i64 = 1_000;
const ESC_TO_PAUSE: &str = " · esc to pause";
const CONTINUE_LATER: &str = " · send a new message when you're ready";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecoveryStatus {
    status: RouteRecoveryStatus,
    deadline_ms: Option<i64>,
    expires_ms: Option<i64>,
    shown_seconds: u64,
}

impl RecoveryStatus {
    pub(crate) fn new(status: RouteRecoveryStatus, now_ms: i64) -> Self {
        let deadline_ms = status
            .retry_wait
            .map(|wait| now_ms.saturating_add(i64::try_from(wait.as_millis()).unwrap_or(i64::MAX)));
        let expires_ms = status.is_recovered().then(|| now_ms + RECOVERED_VISIBLE_MS);
        let mut recovery = Self {
            shown_seconds: status.delay_seconds,
            status,
            deadline_ms,
            expires_ms,
        };
        recovery.refresh(now_ms);
        recovery
    }

    pub(crate) fn is_terminal(&self) -> bool {
        self.status.is_terminal()
    }

    pub(crate) fn expired(&self, now_ms: i64) -> bool {
        self.expires_ms.is_some_and(|expiry| now_ms >= expiry)
    }

    pub(crate) fn refresh(&mut self, now_ms: i64) -> bool {
        let Some(deadline_ms) = self.deadline_ms else {
            return false;
        };
        let seconds = remaining_seconds(deadline_ms, now_ms);
        let changed = seconds != self.shown_seconds;
        self.shown_seconds = seconds;
        changed
    }

    pub(crate) fn next_change_ms(&self, now_ms: i64) -> Option<i64> {
        let countdown = self
            .deadline_ms
            .filter(|deadline_ms| now_ms < *deadline_ms)
            .map(|deadline_ms| {
                let seconds = remaining_seconds(deadline_ms, now_ms);
                deadline_ms - (i64::try_from(seconds).unwrap_or(i64::MAX) - 1) * SECOND_MS
            });
        countdown.into_iter().chain(self.expires_ms).min()
    }

    pub(crate) fn rows(&self, theme: &Theme, cols: usize) -> Vec<Row> {
        let paint = if self.status.is_recovered() {
            theme.green
        } else if self.status.is_terminal() {
            theme.red
        } else {
            theme.warning
        };
        static_status_rows(&self.label(), paint, cols)
    }

    fn label(&self) -> String {
        if self.status.is_recovered() {
            return match self.status.succeeded_attempt {
                0 => "✓ recovered".to_owned(),
                attempt => format!("✓ recovered · attempt {attempt}"),
            };
        }
        if self.status.is_paused() {
            return format!("{}{CONTINUE_LATER}", self.status.label());
        }
        let mut projected = self.status.clone();
        projected.delay_seconds = self.shown_seconds;
        let label = projected.label();
        if self.is_connectivity_wait() {
            format!("{label}{ESC_TO_PAUSE}")
        } else {
            label
        }
    }

    pub(crate) fn is_connectivity_wait(&self) -> bool {
        self.status.kind == RouteRecoveryKind::AutoRetry
            && self.status.action == Some(ModelRecoveryAction::WaitingForConnectivity)
    }
}

fn remaining_seconds(deadline_ms: i64, now_ms: i64) -> u64 {
    if now_ms >= deadline_ms {
        return 0;
    }
    u64::try_from((deadline_ms - now_ms - 1) / SECOND_MS + 1).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remaining_seconds_round_up_and_reach_zero_at_the_deadline() {
        assert_eq!(remaining_seconds(4_000, 0), 4);
        assert_eq!(remaining_seconds(4_000, 1_000), 3);
        assert_eq!(remaining_seconds(4_000, 2_001), 2);
        assert_eq!(remaining_seconds(4_000, 3_750), 1);
        assert_eq!(remaining_seconds(4_000, 4_000), 0);
        assert_eq!(remaining_seconds(4_000, 9_000), 0);
        assert_eq!(remaining_seconds(250, 0), 1);
    }
}
