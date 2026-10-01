use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

use crate::metric::Metric;
use crate::repository;

#[derive(Debug)]
pub(crate) struct Budgets {
    pub(crate) pull_request: PullRequest,
    pub(crate) ceiling: Ceiling,
    pub(crate) limits: BTreeMap<Metric, Limit>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    pull_request: PullRequest,
    ceiling: Ceiling,
    limits: BTreeMap<String, Limit>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullRequest {
    pub(crate) trailer: String,
    pub(crate) size_fail_bytes: u64,
    pub(crate) size_fail_percent: u64,
    pub(crate) size_warn_bytes: u64,
    pub(crate) instructions_fail_percent: u64,
    pub(crate) stream_instructions_fail_percent: u64,
    pub(crate) reason: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Ceiling {
    pub(crate) upstream_commit: String,
    pub(crate) stripped_bytes: u64,
    pub(crate) archive_bytes: u64,
    pub(crate) reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Severity {
    Fail,
    Warn,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Limit {
    pub(crate) max: u64,
    pub(crate) severity: Severity,
    pub(crate) reason: String,
}

impl Budgets {
    pub(crate) fn ceiling_for(&self, metric: Metric) -> Option<u64> {
        match metric {
            Metric::StrippedBytes => Some(self.ceiling.stripped_bytes),
            Metric::ArchiveBytes => Some(self.ceiling.archive_bytes),
            _ => None,
        }
    }
}

pub(crate) fn load(path: &Path) -> Result<Budgets, String> {
    parse(&repository::read(path)?).map_err(|error| format!("{}: {error}", path.display()))
}

fn parse(text: &str) -> Result<Budgets, String> {
    let file: File = toml::from_str(text).map_err(|error| error.to_string())?;
    require_reason("pull_request", &file.pull_request.reason)?;
    require_reason("ceiling", &file.ceiling.reason)?;
    if file.pull_request.trailer.trim().is_empty() {
        return Err("pull_request.trailer must name the description line".to_owned());
    }
    let mut limits = BTreeMap::new();
    for (id, limit) in file.limits {
        let metric =
            Metric::from_id(&id).ok_or_else(|| format!("limits.{id} names no known metric"))?;
        require_reason(&format!("limits.{id}"), &limit.reason)?;
        limits.insert(metric, limit);
    }
    Ok(Budgets {
        pull_request: file.pull_request,
        ceiling: file.ceiling,
        limits,
    })
}

fn require_reason(table: &str, reason: &str) -> Result<(), String> {
    if reason.trim().is_empty() {
        Err(format!("{table} needs a reason"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
[pull_request]
trailer = "Footprint-Budget"
size_fail_bytes = 65536
size_fail_percent = 2
size_warn_bytes = 8192
instructions_fail_percent = 5
stream_instructions_fail_percent = 3
reason = "steps"

[ceiling]
upstream_commit = "34f1ed1"
stripped_bytes = 13315096
archive_bytes = 5835267
reason = "upstream"

[limits.ask_instructions]
max = 1500000
severity = "fail"
reason = "upstream runs more"
"#;

    #[test]
    fn parses_steps_ceiling_and_limits() {
        let budgets = parse(VALID).expect("valid budgets");
        assert_eq!(budgets.pull_request.size_fail_bytes, 65_536);
        assert_eq!(budgets.ceiling_for(Metric::StrippedBytes), Some(13_315_096));
        assert_eq!(budgets.ceiling_for(Metric::AskInstructions), None);
        let limit = &budgets.limits[&Metric::AskInstructions];
        assert_eq!((limit.max, limit.severity), (1_500_000, Severity::Fail));
    }

    #[test]
    fn every_budget_needs_a_reason() {
        let missing = VALID.replace("reason = \"upstream runs more\"", "reason = \" \"");
        assert_eq!(
            parse(&missing).expect_err("blank reason"),
            "limits.ask_instructions needs a reason"
        );
        let absent = VALID.replace("reason = \"upstream\"\n", "");
        assert!(parse(&absent).is_err());
    }

    #[test]
    fn rejects_unknown_metrics_and_fields() {
        let metric = VALID.replace("limits.ask_instructions", "limits.ask_speed");
        assert_eq!(
            parse(&metric).expect_err("unknown metric"),
            "limits.ask_speed names no known metric"
        );
        let field = VALID.replace("severity = \"fail\"", "severity = \"fail\"\nslack = 1");
        assert!(parse(&field).is_err());
    }

    #[test]
    fn the_committed_budgets_load() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../budgets.toml");
        let budgets = load(&path).expect("budgets.toml loads");
        assert_eq!(budgets.pull_request.trailer, "Footprint-Budget");
    }
}
