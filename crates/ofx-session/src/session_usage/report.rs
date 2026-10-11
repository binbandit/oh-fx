use ofx_contract::{
    SessionActivity, SessionModelSource, SessionTotals, SessionUsageSource, UsageCompleteness,
    UsageReport, UsageReportError, build_session_report,
};

use super::{Availability, ModelAggregate, UsageSnapshot};
use crate::session_log::now_ms;

pub fn fresh_session_report() -> Result<UsageReport, UsageReportError> {
    UsageSnapshot::fresh().report(now_ms())
}

impl UsageSnapshot {
    pub(crate) fn report(&self, snapshot_time_ms: i64) -> Result<UsageReport, UsageReportError> {
        let snapshot_time_ms = snapshot_time_ms.max(0);
        let wall_duration_ms = i64::try_from(self.wall_duration_ms).unwrap_or(snapshot_time_ms);
        let models: Vec<SessionModelSource<'_>> =
            self.models.iter().map(ModelAggregate::source).collect();
        build_session_report(&SessionUsageSource {
            snapshot_time_ms,
            session_started_at_ms: snapshot_time_ms.saturating_sub(wall_duration_ms).max(0),
            completeness: self.billing.completeness(),
            totals: SessionTotals {
                total_cost: self.total_cost,
                input_tokens: self.input_tokens,
                output_tokens: self.output_tokens,
                cache_read_tokens: self.cache_read_tokens,
                cache_write_tokens: self.cache_write_tokens,
                reasoning_tokens: self.reasoning_tokens,
                request_count: self.request_count,
            },
            models: &models,
            activity: SessionActivity {
                api_duration_complete: self.api_duration_complete,
                wall_duration_complete: self.wall_duration_complete,
                code_complete: self.code_complete,
                api_duration_ms: self.api_duration_ms,
                wall_duration_ms: self.wall_duration_ms,
                lines_added: self.lines_added,
                lines_removed: self.lines_removed,
            },
        })
    }
}

impl ModelAggregate {
    fn source(&self) -> SessionModelSource<'_> {
        SessionModelSource {
            model: &self.model,
            totals: SessionTotals {
                total_cost: self.total_cost,
                input_tokens: self.input_tokens,
                output_tokens: self.output_tokens,
                cache_read_tokens: self.cache_read_tokens,
                cache_write_tokens: self.cache_write_tokens,
                reasoning_tokens: self.reasoning_tokens,
                request_count: self.request_count,
            },
        }
    }
}

impl Availability {
    const fn completeness(self) -> UsageCompleteness {
        match self {
            Self::Complete => UsageCompleteness::Complete,
            Self::Pending => UsageCompleteness::Pending,
            Self::Incomplete => UsageCompleteness::Incomplete,
            Self::Legacy => UsageCompleteness::Legacy,
        }
    }
}

#[cfg(test)]
mod tests;
