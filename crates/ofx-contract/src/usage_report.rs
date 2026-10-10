use std::cmp::Ordering;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fmt;

use crate::types::valid_gateway_generation_id;

const MAX_MODEL_BYTES: usize = 1024;
const MAX_MODELS: usize = 128;
const MS_PER_SECOND: i64 = 1000;
const MS_PER_HOUR: i64 = 60 * 60 * MS_PER_SECOND;
const MS_PER_DAY: i64 = 24 * MS_PER_HOUR;
const SECONDS_PER_DAY: i64 = MS_PER_DAY / MS_PER_SECOND;
const UNKNOWN_DATE: &str = "Unknown";
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageScope {
    Session,
    Hours24,
    Days7,
    Days30,
}

impl UsageScope {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Session => "Session",
            Self::Hours24 => "24 hours",
            Self::Days7 => "7 days",
            Self::Days30 => "30 days",
        }
    }

    pub const fn cli_value(self) -> Option<&'static str> {
        match self {
            Self::Session => None,
            Self::Hours24 => Some("24h"),
            Self::Days7 => Some("7d"),
            Self::Days30 => Some("30d"),
        }
    }

    pub fn parse_cli_value(value: &str) -> Option<Self> {
        [Self::Hours24, Self::Days7, Self::Days30]
            .into_iter()
            .find(|scope| scope.cli_value() == Some(value))
    }

    const fn duration_ms(self) -> Option<i64> {
        match self {
            Self::Session => None,
            Self::Hours24 => Some(MS_PER_DAY),
            Self::Days7 => Some(MS_PER_DAY * 7),
            Self::Days30 => Some(MS_PER_DAY * 30),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageCompleteness {
    Complete,
    Pending,
    Incomplete,
    Legacy,
}

impl UsageCompleteness {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Pending => "pending",
            Self::Incomplete => "incomplete",
            Self::Legacy => "legacy",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        [
            Self::Complete,
            Self::Pending,
            Self::Incomplete,
            Self::Legacy,
        ]
        .into_iter()
        .find(|completeness| completeness.name() == name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageCoverage {
    NotStarted,
    Partial,
    Full,
}

impl UsageCoverage {
    pub const fn name(self) -> &'static str {
        match self {
            Self::NotStarted => "not_started",
            Self::Partial => "partial",
            Self::Full => "full",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsageIncident {
    pub occurred_at_ms: i64,
    pub completeness: UsageCompleteness,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingMarker {
    pub id: String,
    pub observed_at_ms: i64,
}

impl PendingMarker {
    pub fn is_valid(&self) -> bool {
        valid_gateway_generation_id(&self.id) && self.observed_at_ms >= 0
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct GenerationFact {
    pub id: String,
    pub created_at_ms: i64,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: Option<u64>,
    pub billable_web_search_calls: u64,
    pub total_cost: f64,
}

impl GenerationFact {
    pub fn is_valid(&self) -> bool {
        valid_gateway_generation_id(&self.id)
            && self.created_at_ms >= 0
            && valid_model(&self.model)
            && self.total_cost.is_finite()
            && self.total_cost >= 0.0
            && self.cache_read_tokens <= self.input_tokens
            && self.cache_write_tokens <= self.input_tokens
            && self
                .reasoning_tokens
                .is_none_or(|reasoning| reasoning <= self.output_tokens)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UsageTotals {
    pub total_tokens: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: Option<u64>,
    pub request_count: Option<u64>,
    pub total_cost: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelUsage {
    pub model: String,
    pub totals: UsageTotals,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UsageReport {
    pub scope: UsageScope,
    pub snapshot_time_ms: i64,
    pub window_start_ms: i64,
    pub coverage_started_at_ms: Option<i64>,
    pub coverage: UsageCoverage,
    pub completeness: UsageCompleteness,
    pub totals: Option<UsageTotals>,
    pub models: Vec<ModelUsage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageReportError {
    InvalidGenerationFact,
    InvalidSnapshotTime,
    UsageCapacityExceeded,
    UsageOverflow,
}

impl UsageReportError {
    pub const fn name(self) -> &'static str {
        match self {
            Self::InvalidGenerationFact => "InvalidGenerationFact",
            Self::InvalidSnapshotTime => "InvalidSnapshotTime",
            Self::UsageCapacityExceeded => "UsageCapacityExceeded",
            Self::UsageOverflow => "UsageOverflow",
        }
    }
}

impl fmt::Display for UsageReportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

impl std::error::Error for UsageReportError {}

pub fn format_utc_date(timestamp_ms: i64) -> String {
    if timestamp_ms < 0 {
        return UNKNOWN_DATE.to_owned();
    }
    let days = timestamp_ms.div_euclid(MS_PER_SECOND) / SECONDS_PER_DAY;
    let (year, month, day) = civil_from_unix_days(days);
    let Some(name) = usize::try_from(month - 1)
        .ok()
        .and_then(|index| MONTHS.get(index))
    else {
        return UNKNOWN_DATE.to_owned();
    };
    format!("{name} {day}, {year}")
}

pub fn build_rolling_report(
    scope: UsageScope,
    snapshot_time_ms: i64,
    coverage_started_at_ms: Option<i64>,
    facts: &[GenerationFact],
    incidents: &[UsageIncident],
) -> Result<UsageReport, UsageReportError> {
    let duration_ms = scope
        .duration_ms()
        .ok_or(UsageReportError::InvalidSnapshotTime)?;
    if snapshot_time_ms < 0 {
        return Err(UsageReportError::InvalidSnapshotTime);
    }
    let window_start_ms = snapshot_time_ms
        .checked_sub(duration_ms)
        .ok_or(UsageReportError::InvalidSnapshotTime)?;
    let visible_coverage_started_at_ms = match coverage_started_at_ms {
        Some(started_at_ms) if started_at_ms < 0 => {
            return Err(UsageReportError::InvalidSnapshotTime);
        }
        Some(started_at_ms) if started_at_ms > snapshot_time_ms => None,
        started => started,
    };
    let window = window_start_ms..snapshot_time_ms;
    let mut completeness = UsageCompleteness::Complete;
    for incident in incidents {
        if !window.contains(&incident.occurred_at_ms) {
            continue;
        }
        completeness = match (incident.completeness, completeness) {
            (UsageCompleteness::Incomplete | UsageCompleteness::Legacy, _) => {
                UsageCompleteness::Incomplete
            }
            (UsageCompleteness::Pending, UsageCompleteness::Complete) => UsageCompleteness::Pending,
            (UsageCompleteness::Pending | UsageCompleteness::Complete, current) => current,
        };
    }
    let coverage = match visible_coverage_started_at_ms {
        Some(started_at_ms) if started_at_ms <= window_start_ms => UsageCoverage::Full,
        Some(_) => UsageCoverage::Partial,
        None => UsageCoverage::NotStarted,
    };
    let mut report = UsageReport {
        scope,
        snapshot_time_ms,
        window_start_ms,
        coverage_started_at_ms: None,
        coverage,
        completeness,
        totals: None,
        models: Vec::new(),
    };
    if coverage == UsageCoverage::NotStarted {
        return Ok(report);
    }

    let mut seen: HashMap<&str, &GenerationFact> = HashMap::new();
    let mut model_indexes: HashMap<&str, usize> = HashMap::new();
    let mut models: Vec<(&str, RunningTotals)> = Vec::new();
    let mut totals = RunningTotals::default();
    for fact in facts {
        if !fact.is_valid() {
            return Err(UsageReportError::InvalidGenerationFact);
        }
        if !window.contains(&fact.created_at_ms) {
            continue;
        }
        match seen.entry(&fact.id) {
            Entry::Occupied(first) => {
                if *first.get() != fact {
                    report.completeness = UsageCompleteness::Incomplete;
                }
                continue;
            }
            Entry::Vacant(slot) => {
                slot.insert(fact);
            }
        }
        totals.add(fact)?;
        let index = match model_indexes.entry(&fact.model) {
            Entry::Occupied(index) => *index.get(),
            Entry::Vacant(slot) => {
                if models.len() == MAX_MODELS {
                    return Err(UsageReportError::UsageCapacityExceeded);
                }
                models.push((&fact.model, RunningTotals::default()));
                *slot.insert(models.len() - 1)
            }
        };
        models[index].1.add(fact)?;
    }

    let mut models = models
        .into_iter()
        .map(|(model, totals)| {
            Ok(ModelUsage {
                model: model.to_owned(),
                totals: totals.freeze()?,
            })
        })
        .collect::<Result<Vec<_>, UsageReportError>>()?;
    models.sort_by(usage_first);
    report.coverage_started_at_ms = visible_coverage_started_at_ms;
    report.totals = Some(totals.freeze()?);
    report.models = models;
    Ok(report)
}

struct RunningTotals {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    reasoning_tokens: Option<u64>,
    request_count: u64,
    total_cost: f64,
}

impl Default for RunningTotals {
    fn default() -> Self {
        Self {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: Some(0),
            request_count: 0,
            total_cost: 0.0,
        }
    }
}

impl RunningTotals {
    fn add(&mut self, fact: &GenerationFact) -> Result<(), UsageReportError> {
        let overflow = || UsageReportError::UsageOverflow;
        self.input_tokens = self
            .input_tokens
            .checked_add(fact.input_tokens)
            .ok_or_else(overflow)?;
        self.output_tokens = self
            .output_tokens
            .checked_add(fact.output_tokens)
            .ok_or_else(overflow)?;
        self.cache_read_tokens = self
            .cache_read_tokens
            .checked_add(fact.cache_read_tokens)
            .ok_or_else(overflow)?;
        self.cache_write_tokens = self
            .cache_write_tokens
            .checked_add(fact.cache_write_tokens)
            .ok_or_else(overflow)?;
        self.request_count = self.request_count.checked_add(1).ok_or_else(overflow)?;
        self.reasoning_tokens = match (self.reasoning_tokens, fact.reasoning_tokens) {
            (Some(current), Some(reasoning)) => {
                Some(current.checked_add(reasoning).ok_or_else(overflow)?)
            }
            _ => None,
        };
        let total_cost = self.total_cost + fact.total_cost;
        if !total_cost.is_finite() {
            return Err(UsageReportError::UsageOverflow);
        }
        self.total_cost = total_cost;
        Ok(())
    }

    fn freeze(&self) -> Result<UsageTotals, UsageReportError> {
        Ok(UsageTotals {
            total_tokens: self
                .input_tokens
                .checked_add(self.output_tokens)
                .ok_or(UsageReportError::UsageOverflow)?,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cache_read_tokens: self.cache_read_tokens,
            cache_write_tokens: self.cache_write_tokens,
            reasoning_tokens: self.reasoning_tokens.filter(|_| self.request_count != 0),
            request_count: Some(self.request_count),
            total_cost: self.total_cost,
        })
    }
}

fn usage_first(first: &ModelUsage, second: &ModelUsage) -> Ordering {
    second
        .totals
        .total_tokens
        .cmp(&first.totals.total_tokens)
        .then_with(|| {
            second
                .totals
                .total_cost
                .partial_cmp(&first.totals.total_cost)
                .unwrap_or(Ordering::Equal)
        })
        .then_with(|| first.model.as_bytes().cmp(second.model.as_bytes()))
}

fn valid_model(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= MAX_MODEL_BYTES
        && model.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
}

fn civil_from_unix_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests;
