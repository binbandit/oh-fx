use super::{
    MAX_MODELS, ModelUsage, UsageCompleteness, UsageCoverage, UsageReport, UsageReportError,
    UsageScope, UsageTotals, usage_first, valid_model,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionActivity {
    pub api_duration_complete: bool,
    pub wall_duration_complete: bool,
    pub code_complete: bool,
    pub api_duration_ms: u64,
    pub wall_duration_ms: u64,
    pub lines_added: u64,
    pub lines_removed: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SessionTotals {
    pub total_cost: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: Option<u64>,
    pub request_count: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SessionModelSource<'a> {
    pub model: &'a str,
    pub totals: SessionTotals,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SessionUsageSource<'a> {
    pub snapshot_time_ms: i64,
    pub session_started_at_ms: i64,
    pub completeness: UsageCompleteness,
    pub totals: SessionTotals,
    pub models: &'a [SessionModelSource<'a>],
    pub activity: SessionActivity,
}

impl SessionTotals {
    fn consistent(&self) -> bool {
        self.total_cost.is_finite()
            && self.total_cost >= 0.0
            && self.cache_read_tokens <= self.input_tokens
            && self.cache_write_tokens <= self.input_tokens
    }

    fn reasoning_fits(&self) -> bool {
        self.reasoning_tokens
            .is_none_or(|reasoning| reasoning <= self.output_tokens)
    }

    fn frozen(&self) -> Result<UsageTotals, UsageReportError> {
        Ok(UsageTotals {
            total_tokens: self
                .input_tokens
                .checked_add(self.output_tokens)
                .ok_or(UsageReportError::UsageOverflow)?,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cache_read_tokens: self.cache_read_tokens,
            cache_write_tokens: self.cache_write_tokens,
            reasoning_tokens: self.reasoning_tokens,
            request_count: self.request_count,
            total_cost: self.total_cost,
        })
    }
}

pub fn build_session_report(
    source: &SessionUsageSource<'_>,
) -> Result<UsageReport, UsageReportError> {
    let started_at_ms = source.session_started_at_ms;
    if source.snapshot_time_ms < 0
        || started_at_ms < 0
        || started_at_ms > source.snapshot_time_ms
        || !source.totals.consistent()
    {
        return Err(UsageReportError::InvalidSnapshotTime);
    }
    if !source.totals.reasoning_fits() {
        return Err(UsageReportError::InvalidGenerationFact);
    }
    let mut report = UsageReport {
        scope: UsageScope::Session,
        snapshot_time_ms: source.snapshot_time_ms,
        window_start_ms: started_at_ms,
        coverage_started_at_ms: Some(started_at_ms),
        coverage: UsageCoverage::Full,
        completeness: source.completeness,
        totals: None,
        models: Vec::new(),
        session_activity: Some(source.activity),
    };
    if source.completeness == UsageCompleteness::Legacy {
        return Ok(report);
    }
    if source.models.len() > MAX_MODELS {
        return Err(UsageReportError::UsageCapacityExceeded);
    }
    let totals = source.totals.frozen()?;
    let mut models = Vec::with_capacity(source.models.len());
    for model in source.models {
        if !valid_model(model.model) || !model.totals.consistent() || !model.totals.reasoning_fits()
        {
            return Err(UsageReportError::InvalidGenerationFact);
        }
        models.push(ModelUsage {
            model: model.model.to_owned(),
            totals: model.totals.frozen()?,
        });
    }
    models.sort_by(usage_first);
    report.totals = Some(totals);
    report.models = models;
    Ok(report)
}

#[cfg(test)]
mod tests;
