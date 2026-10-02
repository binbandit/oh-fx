use ofx_contract::CompactionEnd;

use super::activity_status::{TokenProgress, activity_row, static_status_rows};
use crate::row_text::{Paint, Row};
use crate::theme::Theme;

const TRANSIENT_FEEDBACK_MS: i64 = 1500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
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
}

impl CompactionStatus {
    pub(crate) fn preparing(now_ms: i64) -> Self {
        Self {
            phase: Phase::Preparing,
            started_ms: now_ms,
            expires_ms: None,
        }
    }

    pub(crate) fn ended(end: CompactionEnd, now_ms: i64) -> Self {
        let transient = matches!(end, CompactionEnd::NothingToCompact | CompactionEnd::Busy);
        Self {
            phase: Phase::Ended(end),
            started_ms: now_ms,
            expires_ms: transient.then_some(now_ms.saturating_add(TRANSIENT_FEEDBACK_MS)),
        }
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
        self.running().then_some(self.started_ms)
    }

    pub(crate) fn expires_ms(&self) -> Option<i64> {
        self.expires_ms
    }

    pub(crate) fn expired(&self, now_ms: i64) -> bool {
        self.expires_ms.is_some_and(|expiry| now_ms >= expiry)
    }

    pub(crate) fn rows(&self, theme: &Theme, now_ms: i64, cols: usize) -> Vec<Row> {
        let label = match self.phase {
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
            self.started_ms,
            now_ms,
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
