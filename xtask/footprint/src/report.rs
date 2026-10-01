use std::fmt::Write as _;

use crate::budgets::{Budgets, Severity};
use crate::metric::{Metric, Readings, Step, Unit};

pub(crate) struct Side<'a> {
    pub(crate) commit: &'a str,
    pub(crate) readings: Result<&'a Readings, &'a str>,
}

#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Ok,
    Warn(String),
    Fail(String),
    Explained(String),
}

pub(crate) fn trailer_reason(body: &str, trailer: &str) -> Option<String> {
    body.lines().find_map(|line| {
        let reason = line.trim().strip_prefix(trailer)?.strip_prefix(':')?.trim();
        (!reason.is_empty()).then(|| reason.to_owned())
    })
}

pub(crate) fn render(
    budgets: &Budgets,
    head: &Side<'_>,
    base: &Side<'_>,
    target: &str,
    trailer: Option<&str>,
) -> String {
    let mut text = String::from("## Footprint\n\n");
    let _ = writeln!(
        text,
        "Report only: nothing here fails the build yet. Release `oh-fx` for {target} at head `{}` against base `{}`, both built with the head's toolchain.\n",
        short(head.commit),
        short(base.commit)
    );
    let head_readings = match head.readings {
        Ok(readings) => readings,
        Err(error) => {
            let _ = writeln!(text, "The head could not be measured: {error}");
            return text;
        }
    };
    if let Err(error) = base.readings {
        let _ = writeln!(
            text,
            "The base could not be measured, so changes are not shown: {error}\n"
        );
    }
    text.push_str(
        "| Metric | Base | Head | Change | Budget | Verdict |\n|---|---:|---:|---:|---|---|\n",
    );
    for metric in Metric::ALL {
        let head_value = head_readings.get(&metric);
        let base_value = base
            .readings
            .ok()
            .and_then(|readings| readings.get(&metric))
            .and_then(|reading| reading.as_ref().ok().copied());
        let (head_cell, verdict) = match head_value {
            Some(Ok(value)) => (
                format_value(metric.unit(), *value),
                evaluate(budgets, metric, base_value, *value, trailer),
            ),
            Some(Err(error)) => ("unavailable".to_owned(), Verdict::Fail(error.clone())),
            None => (
                "unavailable".to_owned(),
                Verdict::Fail("not measured".to_owned()),
            ),
        };
        let change = match (base_value, head_value) {
            (Some(base_value), Some(Ok(head_value))) => {
                format_change(metric.unit(), base_value, *head_value)
            }
            _ => String::new(),
        };
        let _ = writeln!(
            text,
            "| {} | {} | {} | {} | {} | {} |",
            metric.label(),
            base_value.map_or_else(String::new, |value| format_value(metric.unit(), value)),
            head_cell,
            change,
            budget_text(budgets, metric),
            verdict_text(&verdict)
        );
    }
    if let Some(reason) = trailer {
        let _ = writeln!(
            text,
            "\nThe description explains growth past the pull request steps: {}: {reason}",
            budgets.pull_request.trailer
        );
    }
    text.push_str(&reasons(budgets));
    text
}

fn evaluate(
    budgets: &Budgets,
    metric: Metric,
    base: Option<u64>,
    head: u64,
    trailer: Option<&str>,
) -> Verdict {
    let mut failures = Vec::new();
    let mut warnings = Vec::new();
    if let Some(ceiling) = budgets
        .ceiling_for(metric)
        .filter(|ceiling| head > *ceiling)
    {
        failures.push(format!(
            "above upstream {} ({})",
            budgets.ceiling.upstream_commit,
            format_value(metric.unit(), ceiling)
        ));
    }
    if let Some(limit) = budgets.limits.get(&metric).filter(|limit| head > limit.max) {
        let finding = format!("above {}", format_value(metric.unit(), limit.max));
        match limit.severity {
            Severity::Fail => failures.push(finding),
            Severity::Warn => warnings.push(finding),
        }
    }
    let mut steps = Vec::new();
    if base.is_none() && metric.step() != Step::Untracked {
        warnings.push("no base to compare".to_owned());
    }
    if let Some(base) = base.filter(|base| head > *base) {
        let growth = head - base;
        let rules = &budgets.pull_request;
        let percent_over =
            |limit: u64| u128::from(growth) * 100 >= u128::from(base) * u128::from(limit);
        match metric.step() {
            Step::Size => {
                if growth >= rules.size_fail_bytes || percent_over(rules.size_fail_percent) {
                    steps.push(format!("grew {}", format_value(Unit::Bytes, growth)));
                } else if growth > rules.size_warn_bytes {
                    warnings.push(format!("grew {}", format_value(Unit::Bytes, growth)));
                }
            }
            Step::Instructions if percent_over(rules.instructions_fail_percent) => {
                steps.push(format!("grew {}", format_percent(base, head)));
            }
            Step::StreamInstructions if percent_over(rules.stream_instructions_fail_percent) => {
                steps.push(format!("grew {}", format_percent(base, head)));
            }
            _ => {}
        }
    }
    if !failures.is_empty() {
        failures.extend(steps);
        Verdict::Fail(failures.join("; "))
    } else if !steps.is_empty() {
        match trailer {
            Some(_) => Verdict::Explained(steps.join("; ")),
            None => Verdict::Fail(steps.join("; ")),
        }
    } else if !warnings.is_empty() {
        Verdict::Warn(warnings.join("; "))
    } else {
        Verdict::Ok
    }
}

fn budget_text(budgets: &Budgets, metric: Metric) -> String {
    let mut parts = Vec::new();
    if let Some(ceiling) = budgets.ceiling_for(metric) {
        parts.push(format!(
            "≤ {} (upstream {})",
            format_value(metric.unit(), ceiling),
            budgets.ceiling.upstream_commit
        ));
    }
    if let Some(limit) = budgets.limits.get(&metric) {
        let severity = match limit.severity {
            Severity::Fail => "",
            Severity::Warn => " (warn)",
        };
        parts.push(format!(
            "≤ {}{severity}",
            format_value(metric.unit(), limit.max)
        ));
    }
    let rules = &budgets.pull_request;
    match metric.step() {
        Step::Size => parts.push(format!(
            "growth < {} and < {}%",
            format_value(Unit::Bytes, rules.size_fail_bytes),
            rules.size_fail_percent
        )),
        Step::Instructions => parts.push(format!("growth < {}%", rules.instructions_fail_percent)),
        Step::StreamInstructions => parts.push(format!(
            "growth < {}%",
            rules.stream_instructions_fail_percent
        )),
        Step::Untracked => {}
    }
    parts.join("; ")
}

fn verdict_text(verdict: &Verdict) -> String {
    match verdict {
        Verdict::Ok => "ok".to_owned(),
        Verdict::Warn(reason) => format!("warn: {}", cell(reason)),
        Verdict::Fail(reason) => format!("**would fail**: {}", cell(reason)),
        Verdict::Explained(reason) => format!("explained: {}", cell(reason)),
    }
}

fn reasons(budgets: &Budgets) -> String {
    let mut text = String::from("\n<details><summary>Why these budgets</summary>\n\n");
    let _ = writeln!(
        text,
        "- **Pull request steps:** {}",
        budgets.pull_request.reason
    );
    let _ = writeln!(text, "- **Upstream ceiling:** {}", budgets.ceiling.reason);
    for (metric, limit) in &budgets.limits {
        let _ = writeln!(text, "- **`{}`:** {}", metric.id(), limit.reason);
    }
    text.push_str("\n</details>\n");
    text
}

fn cell(text: &str) -> String {
    text.replace('|', "\\|").replace('\n', " ")
}

fn short(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

fn format_value(unit: Unit, value: u64) -> String {
    let number = group_digits(value);
    match unit {
        Unit::Bytes => format!("{number} B"),
        Unit::Kibibytes => format!("{number} KiB"),
        Unit::Count => number,
    }
}

fn format_change(unit: Unit, base: u64, head: u64) -> String {
    let sign = if head >= base { '+' } else { '-' };
    let difference = head.abs_diff(base);
    let amount = format_value(unit, difference);
    if base == 0 {
        format!("{sign}{amount}")
    } else {
        format!("{sign}{amount} ({})", format_percent(base, head))
    }
}

fn format_percent(base: u64, head: u64) -> String {
    let sign = if head >= base { '+' } else { '-' };
    let tenths = u128::from(head.abs_diff(base)) * 1000 / u128::from(base.max(1));
    format!("{sign}{}.{}%", tenths / 10, tenths % 10)
}

fn group_digits(value: u64) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::budgets::{Ceiling, Limit, PullRequest};

    fn budgets() -> Budgets {
        let mut limits = BTreeMap::new();
        limits.insert(
            Metric::AskInstructions,
            Limit {
                max: 1_500_000,
                severity: Severity::Fail,
                reason: "upstream runs more".to_owned(),
            },
        );
        limits.insert(
            Metric::AskPeakRss,
            Limit {
                max: 4_096,
                severity: Severity::Warn,
                reason: "frugal".to_owned(),
            },
        );
        Budgets {
            pull_request: PullRequest {
                trailer: "Footprint-Budget".to_owned(),
                size_fail_bytes: 65_536,
                size_fail_percent: 2,
                size_warn_bytes: 8_192,
                instructions_fail_percent: 5,
                stream_instructions_fail_percent: 3,
                reason: "steps".to_owned(),
            },
            ceiling: Ceiling {
                upstream_commit: "34f1ed1".to_owned(),
                stripped_bytes: 13_315_096,
                archive_bytes: 5_835_267,
                reason: "upstream".to_owned(),
            },
            limits,
        }
    }

    #[test]
    fn size_growth_warns_then_fails_unless_explained() {
        let budgets = budgets();
        let size = Metric::StrippedBytes;
        let base = 4_000_000;
        assert_eq!(
            evaluate(&budgets, size, Some(base), base + 8_192, None),
            Verdict::Ok
        );
        assert_eq!(
            evaluate(&budgets, size, Some(base), base + 9_000, None),
            Verdict::Warn("grew 9,000 B".to_owned())
        );
        assert_eq!(
            evaluate(&budgets, size, Some(base), base + 65_536, None),
            Verdict::Fail("grew 65,536 B".to_owned())
        );
        assert_eq!(
            evaluate(&budgets, size, Some(base), base + 65_536, Some("new TUI")),
            Verdict::Explained("grew 65,536 B".to_owned())
        );
        assert_eq!(
            evaluate(&budgets, size, Some(base), base - 100_000, None),
            Verdict::Ok
        );
    }

    #[test]
    fn percent_steps_catch_small_binaries() {
        let budgets = budgets();
        assert_eq!(
            evaluate(
                &budgets,
                Metric::StrippedBytes,
                Some(1_000_000),
                1_020_000,
                None
            ),
            Verdict::Fail("grew 20,000 B".to_owned())
        );
    }

    #[test]
    fn the_upstream_ceiling_cannot_be_explained_away() {
        let budgets = budgets();
        assert_eq!(
            evaluate(
                &budgets,
                Metric::StrippedBytes,
                None,
                13_315_097,
                Some("needed")
            ),
            Verdict::Fail("above upstream 34f1ed1 (13,315,096 B)".to_owned())
        );
    }

    #[test]
    fn instruction_steps_and_limits() {
        let budgets = budgets();
        let ask = Metric::AskInstructions;
        assert_eq!(
            evaluate(&budgets, ask, Some(1_000_000), 1_049_999, None),
            Verdict::Ok
        );
        assert_eq!(
            evaluate(&budgets, ask, Some(1_000_000), 1_050_000, None),
            Verdict::Fail("grew +5.0%".to_owned())
        );
        assert_eq!(
            evaluate(&budgets, ask, Some(38_000_000), 38_000_000, None),
            Verdict::Fail("above 1,500,000".to_owned())
        );
        assert_eq!(
            evaluate(&budgets, Metric::AskPeakRss, Some(3_000), 5_000, None),
            Verdict::Warn("above 4,096 KiB".to_owned())
        );
    }

    #[test]
    fn reads_the_trailer_from_the_description() {
        let body = "Adds the TUI.\r\n\r\nFootprint-Budget: the frame renderer is new\r\n";
        assert_eq!(
            trailer_reason(body, "Footprint-Budget").as_deref(),
            Some("the frame renderer is new")
        );
        assert_eq!(
            trailer_reason("Footprint-Budget:   \n", "Footprint-Budget"),
            None
        );
        assert_eq!(trailer_reason("No reason here.", "Footprint-Budget"), None);
    }

    #[test]
    fn formats_numbers_changes_and_percentages() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(4_174_736), "4,174,736");
        assert_eq!(
            format_change(Unit::Bytes, 4_174_736, 4_068_312),
            "-106,424 B (-2.5%)"
        );
        assert_eq!(format_change(Unit::Count, 0, 518), "+518");
        assert_eq!(format_percent(31_518, 32_061), "+1.7%");
    }

    #[test]
    fn renders_a_table_with_unmeasured_bases() {
        let budgets = budgets();
        let mut head = Readings::new();
        head.insert(Metric::StrippedBytes, Ok(4_174_736));
        head.insert(
            Metric::AskInstructions,
            Err("valgrind is missing".to_owned()),
        );
        let text = render(
            &budgets,
            &Side {
                commit: "0123456789abcdef",
                readings: Ok(&head),
            },
            &Side {
                commit: "fedcba9876543210",
                readings: Err("cargo build failed"),
            },
            "x86_64-unknown-linux-musl",
            None,
        );
        assert!(text.contains("head `0123456789ab` against base `fedcba987654`"));
        assert!(text.contains("The base could not be measured"));
        assert!(text.contains("| Stripped binary |  | 4,174,736 B |  | ≤ 13,315,096 B (upstream 34f1ed1); growth < 65,536 B and < 2% | warn: no base to compare |"));
        assert!(text.contains(
            "| unavailable |  | ≤ 1,500,000; growth < 5% | **would fail**: valgrind is missing |"
        ));
        assert!(text.contains("- **`ask_instructions`:** upstream runs more"));
    }
}
