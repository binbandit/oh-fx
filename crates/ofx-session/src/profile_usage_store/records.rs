use std::fmt::Write as _;

use ofx_contract::{GenerationFact, PendingMarker, UsageCompleteness, UsageIncident};

use super::{RecordIndex, UsageStoreError, Variants};
use crate::generation_fact_codec;
use crate::json_fields::push_string;

const MAX_FILE_INCIDENTS: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum ProfileEvent<'a> {
    Generation(&'a GenerationFact),
    Pending(&'a PendingMarker),
    Incident(&'a UsageIncident),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AppendOutcome {
    Appended,
    Duplicate,
    Conflict,
}

pub(super) struct AppendDecision {
    pub(super) outcome: AppendOutcome,
    pub(super) write: bool,
}

impl ProfileEvent<'_> {
    pub(super) fn validate(self) -> Result<(), UsageStoreError> {
        match self {
            Self::Generation(fact) if !fact.is_valid() => Err(UsageStoreError::InvalidFact),
            Self::Pending(marker) if !marker.is_valid() => Err(UsageStoreError::InvalidPending),
            Self::Incident(incident) if !valid_incident(incident) => {
                Err(UsageStoreError::InvalidIncident)
            }
            _ => Ok(()),
        }
    }

    pub(super) fn line(self) -> String {
        let mut out = String::new();
        match self {
            Self::Generation(fact) => {
                out.push_str("{\"schema_version\":1,\"kind\":\"generation\",\"fact\":");
                generation_fact_codec::write(&mut out, fact);
                out.push_str("}\n");
            }
            Self::Pending(marker) => {
                out.push_str("{\"schema_version\":1,\"kind\":\"pending\",\"id\":");
                push_string(&mut out, &marker.id);
                let _ = writeln!(out, ",\"observed_at_ms\":{}}}", marker.observed_at_ms);
            }
            Self::Incident(incident) => push_incident(&mut out, incident),
        }
        out
    }
}

pub(super) fn push_coverage(out: &mut String, started_at_ms: i64) {
    let _ = writeln!(
        out,
        "{{\"schema_version\":1,\"kind\":\"coverage\",\"started_at_ms\":{started_at_ms}}}"
    );
}

pub(super) fn push_incident(out: &mut String, incident: &UsageIncident) {
    let _ = write!(
        out,
        "{{\"schema_version\":1,\"kind\":\"incident\",\"occurred_at_ms\":{},\"completeness\":",
        incident.occurred_at_ms
    );
    push_string(out, incident.completeness.name());
    out.push_str("}\n");
}

fn valid_incident(incident: &UsageIncident) -> bool {
    incident.occurred_at_ms >= 0
        && matches!(
            incident.completeness,
            UsageCompleteness::Pending | UsageCompleteness::Incomplete
        )
}

impl RecordIndex {
    pub(super) fn classify(&self, event: ProfileEvent<'_>) -> AppendDecision {
        match event {
            ProfileEvent::Generation(fact) => classify_variant(
                &self.loaded.facts,
                self.fact_variants.get(&fact.id).copied(),
                fact,
            ),
            ProfileEvent::Pending(marker) => classify_variant(
                &self.loaded.pending,
                self.pending_variants.get(&marker.id).copied(),
                marker,
            ),
            ProfileEvent::Incident(incident) => {
                let known = self.loaded.incidents.len() >= MAX_FILE_INCIDENTS
                    || self.loaded.incidents.contains(incident);
                AppendDecision {
                    outcome: if known {
                        AppendOutcome::Duplicate
                    } else {
                        AppendOutcome::Appended
                    },
                    write: !known,
                }
            }
        }
    }
}

fn classify_variant<T: PartialEq>(
    records: &[T],
    variants: Option<Variants>,
    record: &T,
) -> AppendDecision {
    let Some(variants) = variants else {
        return AppendDecision {
            outcome: AppendOutcome::Appended,
            write: true,
        };
    };
    let seen = records[variants.first] == *record
        || variants
            .second
            .is_some_and(|second| records[second] == *record);
    AppendDecision {
        outcome: if seen {
            AppendOutcome::Duplicate
        } else {
            AppendOutcome::Conflict
        },
        write: !seen && variants.second.is_none(),
    }
}
