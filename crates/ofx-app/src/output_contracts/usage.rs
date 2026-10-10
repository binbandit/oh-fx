use std::fmt::Write as _;

use ofx_cli::OutputFormat;
use ofx_contract::{UsageCompleteness, UsageCoverage, UsageReport, UsageTotals, format_utc_date};
use ofx_text::fixed_decimal;

use super::status::safe;

const SPEND_DECIMALS: usize = 4;
const SESSION_PERIOD: &str = "session";

pub struct UsageSnapshot<'a> {
    pub report: &'a UsageReport,
}

impl UsageSnapshot<'_> {
    pub fn render(&self, format: OutputFormat) -> String {
        match format {
            OutputFormat::Text => self.render_text(),
            OutputFormat::Json => {
                let mut line = self.render_json();
                line.push('\n');
                line
            }
        }
    }

    fn render_text(&self) -> String {
        let report = self.report;
        let mut out = String::new();
        let _ = writeln!(out, "Usage ({})", report.scope.label());
        match (report.coverage, report.coverage_started_at_ms) {
            (UsageCoverage::NotStarted, _) => out.push_str("Tracking has not started.\n"),
            (UsageCoverage::Partial, Some(started_at_ms)) => {
                let _ = writeln!(
                    out,
                    "Tracking since {} (partial window).",
                    format_utc_date(started_at_ms)
                );
            }
            (UsageCoverage::Partial | UsageCoverage::Full, _) => {}
        }
        out.push_str(match report.completeness {
            UsageCompleteness::Complete => "",
            UsageCompleteness::Pending => "Known totals exclude pending Gateway reconciliation.\n",
            UsageCompleteness::Incomplete => "Known totals may be incomplete.\n",
            UsageCompleteness::Legacy => "This session predates complete usage tracking.\n",
        });
        let Some(totals) = report.totals else {
            return out;
        };
        let _ = writeln!(out, "Total tokens  {}", totals.total_tokens);
        let _ = writeln!(out, "Input         {}", totals.input_tokens);
        let _ = writeln!(out, "Output        {}", totals.output_tokens);
        let _ = writeln!(
            out,
            "Cache         {} read · {} write",
            totals.cache_read_tokens, totals.cache_write_tokens
        );
        if let Some(reasoning) = totals.reasoning_tokens {
            let _ = writeln!(out, "Reasoning     {reasoning}");
        }
        if let Some(requests) = totals.request_count {
            let _ = writeln!(out, "Requests      {requests}");
        }
        let _ = writeln!(out, "Spend         ${}", spend(totals.total_cost));
        if !report.models.is_empty() {
            out.push_str("\nBy model\n");
            for model in &report.models {
                let _ = writeln!(
                    out,
                    "- {}  {} tokens  ${}",
                    safe(&model.model),
                    model.totals.total_tokens,
                    spend(model.totals.total_cost)
                );
            }
        }
        out
    }

    fn render_json(&self) -> String {
        let report = self.report;
        let mut out = String::from("{\"kind\":\"usage\",\"schema_version\":1,\"period\":");
        push_json_string(&mut out, report.scope.cli_value().unwrap_or(SESSION_PERIOD));
        let _ = write!(
            out,
            ",\"snapshot_time_ms\":{},\"window_start_ms\":{},\"coverage\":{{\"status\":",
            report.snapshot_time_ms, report.window_start_ms
        );
        push_json_string(&mut out, report.coverage.name());
        out.push_str(",\"started_at_ms\":");
        push_optional(&mut out, report.coverage_started_at_ms);
        let _ = write!(
            out,
            ",\"full_window\":{}}},\"completeness\":",
            report.coverage == UsageCoverage::Full
        );
        push_json_string(&mut out, report.completeness.name());
        out.push_str(",\"totals\":");
        match &report.totals {
            Some(totals) => push_totals(&mut out, totals),
            None => out.push_str("null"),
        }
        out.push_str(",\"models\":[");
        for (index, model) in report.models.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push_str("{\"model\":");
            push_json_string(&mut out, &model.model);
            out.push_str(",\"totals\":");
            push_totals(&mut out, &model.totals);
            out.push('}');
        }
        out.push_str("]}");
        out
    }
}

fn push_totals(out: &mut String, totals: &UsageTotals) {
    let _ = write!(
        out,
        "{{\"total_tokens\":{},\"input_tokens\":{},\"output_tokens\":{},\"cache_read_tokens\":{},\"cache_write_tokens\":{},\"reasoning_tokens\":",
        totals.total_tokens,
        totals.input_tokens,
        totals.output_tokens,
        totals.cache_read_tokens,
        totals.cache_write_tokens
    );
    push_optional(out, totals.reasoning_tokens);
    out.push_str(",\"request_count\":");
    push_optional(out, totals.request_count);
    let _ = write!(out, ",\"spend\":{}}}", totals.total_cost);
}

fn push_optional<T: std::fmt::Display>(out: &mut String, value: Option<T>) {
    match value {
        Some(value) => {
            let _ = write!(out, "{value}");
        }
        None => out.push_str("null"),
    }
}

fn push_json_string(out: &mut String, text: &str) {
    out.push_str(&serde_json::Value::from(text).to_string());
}

fn spend(cost: f64) -> String {
    fixed_decimal(cost, SPEND_DECIMALS)
}

#[cfg(test)]
mod tests;
