use ofx_contract::{CompactionEnd, TurnId};

use super::activity_status::{ActivityClock, TokenProgress, activity_row, static_status_rows};
use crate::row_text::{Paint, Row};
use crate::theme::Theme;

const TRANSIENT_FEEDBACK_MS: i64 = 1500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Requested,
    Preparing,
    Summarizing,
    Stopping,
    Ended(CompactionEnd),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tone {
    Neutral,
    Warning,
    Danger,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CompactionStatus {
    phase: Phase,
    started_ms: i64,
    expires_ms: Option<i64>,
    turn: Option<TurnId>,
}

impl CompactionStatus {
    pub(crate) fn requested(now_ms: i64) -> Self {
        Self {
            phase: Phase::Requested,
            started_ms: now_ms,
            expires_ms: None,
            turn: None,
        }
    }

    pub(crate) fn preparing(previous: Option<Self>, now_ms: i64) -> Self {
        match previous {
            Some(status) if status.phase == Phase::Stopping => status,
            _ => Self {
                phase: Phase::Preparing,
                started_ms: now_ms,
                expires_ms: None,
                turn: None,
            },
        }
    }

    pub(crate) fn turn_preparing(turn: TurnId, now_ms: i64) -> Self {
        Self {
            turn: Some(turn),
            ..Self::preparing(None, now_ms)
        }
    }

    pub(crate) fn ended(end: CompactionEnd, now_ms: i64) -> Self {
        let transient = matches!(end, CompactionEnd::NothingToCompact | CompactionEnd::Busy);
        Self {
            phase: Phase::Ended(end),
            started_ms: now_ms,
            expires_ms: transient.then_some(now_ms.saturating_add(TRANSIENT_FEEDBACK_MS)),
            turn: None,
        }
    }

    pub(crate) fn turn(&self) -> Option<TurnId> {
        self.turn
    }

    pub(crate) fn summarizing(&mut self) {
        if self.phase == Phase::Preparing {
            self.phase = Phase::Summarizing;
        }
    }

    pub(crate) fn stopping(&mut self) {
        if self.running() {
            self.phase = Phase::Stopping;
        }
    }

    pub(crate) fn running(&self) -> bool {
        !matches!(self.phase, Phase::Ended(_))
    }

    pub(crate) fn clock_ms(&self) -> Option<i64> {
        (self.running() && self.phase != Phase::Requested && self.turn.is_none())
            .then_some(self.started_ms)
    }

    pub(crate) fn expires_ms(&self) -> Option<i64> {
        self.expires_ms
    }

    pub(crate) fn expired(&self, now_ms: i64) -> bool {
        self.expires_ms.is_some_and(|expiry| now_ms >= expiry)
    }

    pub(crate) fn rows(&self, theme: &Theme, clock: ActivityClock, cols: usize) -> Vec<Row> {
        let label = match self.phase {
            Phase::Requested => return Vec::new(),
            Phase::Preparing => "Preparing compaction",
            Phase::Summarizing => "Compacting",
            Phase::Stopping => "Stopping compaction",
            Phase::Ended(end) => {
                let (label, tone) = feedback(end);
                return static_status_rows(label, tone_paint(theme, tone), cols);
            }
        };
        vec![activity_row(
            theme,
            label,
            clock,
            TokenProgress::default(),
            cols,
        )]
    }
}

fn feedback(end: CompactionEnd) -> (&'static str, Tone) {
    match end {
        CompactionEnd::NothingToCompact => ("No context to compact.", Tone::Neutral),
        CompactionEnd::Busy => (
            "Wait for the active work to finish before compacting context.",
            Tone::Warning,
        ),
        CompactionEnd::Cancelled => (
            "Compaction cancelled. Try /compact again when ready.",
            Tone::Neutral,
        ),
        CompactionEnd::Failed => ("Compaction failed. Try /compact again.", Tone::Danger),
        CompactionEnd::AuthenticationRejected => (
            "Compaction was not started. Check authentication and try /compact again.",
            Tone::Danger,
        ),
        CompactionEnd::ContextTooLarge => (
            "Context is too large to compact. Choose a model with a larger context window.",
            Tone::Danger,
        ),
    }
}

fn tone_paint(theme: &Theme, tone: Tone) -> Paint {
    match tone {
        Tone::Neutral => theme.dim,
        Tone::Warning => theme.warning,
        Tone::Danger => theme.red,
    }
}
