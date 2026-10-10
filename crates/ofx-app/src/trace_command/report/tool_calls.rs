use std::fmt::{self, Write as _};

use ofx_agent::{ToolCallMetric, ToolCallOutcome, ToolCallTrace};
use ofx_contract::is_provider_search_alias;
use ofx_text::mask_secrets;

use super::{SEARCH_NAME, Timestamp};

const TOOL_PROBLEMS: usize = 5;
const PREVIEW_BYTES: usize = 180;
const TRIMMED: [char; 4] = [' ', '\t', '\r', '\n'];
const LINE_END: [char; 3] = [' ', '\t', '\r'];

pub(super) fn write_problems(out: &mut String, trace: &ToolCallTrace) -> Result<usize, fmt::Error> {
    let mut count = 0;
    for call in trace
        .calls
        .iter()
        .rev()
        .filter(|call| call.outcome != ToolCallOutcome::Succeeded)
        .take(TOOL_PROBLEMS)
    {
        count += 1;
        out.push_str("- tool ");
        write_compact(out, call)?;
    }
    Ok(count)
}

pub(super) fn write_section(out: &mut String, trace: &ToolCallTrace) -> fmt::Result {
    let calls = &trace.calls;
    if calls.is_empty() {
        out.push_str("\n## Tool Calls\n(none recorded)\n");
        return Ok(());
    }
    out.push_str("\n## Tool Calls\n### Local\n");
    let count =
        |outcome: ToolCallOutcome| calls.iter().filter(|call| call.outcome == outcome).count();
    let total_ms: u64 = calls.iter().map(|call| u64::from(call.duration_ms)).sum();
    writeln!(
        out,
        "last={} succeeded={} rejected={} command_failed={} tool_failed={} runtime_failed={} total={total_ms}ms",
        calls.len(),
        count(ToolCallOutcome::Succeeded),
        count(ToolCallOutcome::Rejected),
        count(ToolCallOutcome::CommandFailed),
        count(ToolCallOutcome::ToolFailed),
        count(ToolCallOutcome::RuntimeFailed),
    )?;
    let lifetime = &trace.lifetime;
    writeln!(
        out,
        "session: calls={} succeeded={} rejected={} command_failed={} tool_failed={} runtime_failed={} total_time={}s",
        lifetime.total_calls,
        lifetime.count_for(ToolCallOutcome::Succeeded),
        lifetime.count_for(ToolCallOutcome::Rejected),
        lifetime.count_for(ToolCallOutcome::CommandFailed),
        lifetime.count_for(ToolCallOutcome::ToolFailed),
        lifetime.count_for(ToolCallOutcome::RuntimeFailed),
        lifetime.total_duration_ms / 1000,
    )?;
    let held = u64::try_from(calls.len()).unwrap_or(u64::MAX);
    if lifetime.total_calls > held {
        writeln!(
            out,
            "coverage: window holds only the last {} of {} tool calls; full results persist in the session directory",
            calls.len(),
            lifetime.total_calls
        )?;
    } else {
        out.push_str("coverage: complete (window holds every recorded tool call)\n");
    }
    if count(ToolCallOutcome::Succeeded) != calls.len() {
        out.push_str("non-successes first:\n");
        for call in calls
            .iter()
            .filter(|call| call.outcome != ToolCallOutcome::Succeeded)
        {
            write_compact(out, call)?;
            write_field(out, "args", &call.args, call.args_total_bytes)?;
            write_field(out, "result", &call.result, call.result_total_bytes)?;
        }
    }
    out.push_str("recent successes (compact):\n");
    for call in calls
        .iter()
        .filter(|call| call.outcome == ToolCallOutcome::Succeeded)
    {
        write_compact(out, call)?;
        write_field(out, "args", &call.args, call.args_total_bytes)?;
        write_result_preview(out, &call.result, call.result_total_bytes)?;
    }
    Ok(())
}

fn write_compact(out: &mut String, call: &ToolCallMetric) -> fmt::Result {
    let name = if is_provider_search_alias(&call.name) {
        SEARCH_NAME
    } else {
        &call.name
    };
    write!(
        out,
        "{} name={name} outcome={} duration={}ms",
        Timestamp(call.started_at_ms),
        call.outcome.name(),
        call.duration_ms
    )?;
    if call.subagent_id == 0 {
        out.push_str(" source=parent\n");
        Ok(())
    } else {
        writeln!(out, " source=subagent#{}", call.subagent_id)
    }
}

fn write_field(out: &mut String, label: &str, body: &str, total_bytes: u32) -> fmt::Result {
    if total_bytes == 0 {
        return Ok(());
    }
    let masked = mask_secrets(body.trim_matches(TRIMMED));
    let trimmed = masked.trim_matches(TRIMMED);
    if trimmed.is_empty() {
        return Ok(());
    }
    let more = u64::from(total_bytes).saturating_sub(u64::try_from(body.len()).unwrap_or(u64::MAX));
    if !trimmed.contains('\n') {
        write!(out, "  {label}: {trimmed}")?;
        if more > 0 {
            write!(out, " ... ({more} more bytes)")?;
        }
        out.push('\n');
        return Ok(());
    }
    writeln!(out, "  {label}:")?;
    for line in trimmed.split('\n') {
        writeln!(out, "    {}", line.trim_end_matches(LINE_END))?;
    }
    if more > 0 {
        writeln!(out, "    ... ({more} more bytes)")?;
    }
    Ok(())
}

fn write_result_preview(out: &mut String, body: &str, total_bytes: u32) -> fmt::Result {
    if total_bytes == 0 {
        return Ok(());
    }
    let masked = mask_secrets(body.trim_matches(TRIMMED));
    let trimmed = masked.trim_matches(TRIMMED);
    if trimmed.is_empty() {
        return Ok(());
    }
    let preview = first_useful_line(trimmed);
    out.push_str("  result_preview: ");
    if preview.len() > PREVIEW_BYTES {
        out.push_str(&preview[..preview.floor_char_boundary(PREVIEW_BYTES)]);
        out.push_str(" ...");
    } else {
        out.push_str(preview);
    }
    if u64::from(total_bytes) > u64::try_from(preview.len()).unwrap_or(u64::MAX) {
        write!(out, " ({total_bytes} bytes total)")?;
    }
    out.push('\n');
    Ok(())
}

fn first_useful_line(text: &str) -> &str {
    text.split('\n')
        .map(|line| line.trim_matches(LINE_END))
        .find(|line| !line.is_empty() && *line != ":")
        .unwrap_or_else(|| &text[..text.floor_char_boundary(PREVIEW_BYTES)])
}

#[cfg(test)]
mod tests;
