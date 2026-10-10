use std::fmt::{self, Write as _};

use ofx_trace::{NetworkCall, NetworkCallKind, NetworkTrace};

use super::{LOG_VARIABLE, Timestamp};

const NETWORK_PROBLEMS: usize = 3;

pub(super) fn write_problems(out: &mut String, trace: &NetworkTrace) -> Result<usize, fmt::Error> {
    let mut count = 0;
    for call in trace
        .calls
        .iter()
        .rev()
        .filter(|call| call.is_error())
        .take(NETWORK_PROBLEMS)
    {
        count += 1;
        out.push_str("- network ");
        write_compact(out, call)?;
    }
    Ok(count)
}

pub(super) fn write_section(out: &mut String, trace: &NetworkTrace) -> fmt::Result {
    let calls = &trace.calls;
    if calls.is_empty() {
        out.push_str("\n## Network Calls\n(none recorded)\n");
        return Ok(());
    }
    let errors = calls.iter().filter(|call| call.is_error()).count();
    let durations = calls.iter().map(|call| call.duration_ms);
    let total_ms: u64 = durations.clone().map(u64::from).sum();
    let held = u64::try_from(calls.len()).unwrap_or(u64::MAX);
    writeln!(
        out,
        "\n## Network Calls\nlast={} ok={} errors={errors} avg={}ms min={}ms max={}ms",
        calls.len(),
        calls.len() - errors,
        total_ms / held,
        durations.clone().min().unwrap_or_default(),
        durations.max().unwrap_or_default(),
    )?;
    write_session_totals(out, trace)?;
    for call in calls {
        write_compact(out, call)?;
    }
    Ok(())
}

fn write_session_totals(out: &mut String, trace: &NetworkTrace) -> fmt::Result {
    let lifetime = &trace.lifetime;
    writeln!(
        out,
        "session: calls={} ok={} errors={} total_time={}s",
        lifetime.total_calls,
        lifetime.ok_calls,
        lifetime.error_calls,
        lifetime.total_duration_ms / 1000
    )?;
    let window = &trace.calls;
    let held = u64::try_from(window.len()).unwrap_or(u64::MAX);
    match window.first() {
        Some(oldest) if lifetime.total_calls > held => writeln!(
            out,
            "coverage: window holds only the last {} of {} calls, oldest retained {}; run with {LOG_VARIABLE} for a complete transport record",
            window.len(),
            lifetime.total_calls,
            Timestamp(oldest.started_at_ms)
        )?,
        _ => out.push_str("coverage: complete (window holds every recorded call)\n"),
    }
    if trace.turns.is_empty() {
        return Ok(());
    }
    if lifetime.evicted_turns > 0 {
        writeln!(
            out,
            "turns: most recent {} shown, {} older evicted",
            trace.turns.len(),
            lifetime.evicted_turns
        )?;
    } else {
        out.push_str("turns:\n");
    }
    for rollup in &trace.turns {
        write!(
            out,
            "  turn {}: calls={} errors={} total_time={}s",
            rollup.turn_id,
            rollup.calls,
            rollup.error_calls,
            rollup.total_duration_ms / 1000
        )?;
        if rollup.subagent_calls > 0 {
            write!(out, " subagent_calls={}", rollup.subagent_calls)?;
        }
        if rollup.first_started_at_ms > 0 {
            write!(out, " started {}", Timestamp(rollup.first_started_at_ms))?;
        }
        out.push('\n');
    }
    Ok(())
}

fn write_compact(out: &mut String, call: &NetworkCall) -> fmt::Result {
    write!(
        out,
        "{} model={}",
        Timestamp(call.started_at_ms),
        call.model
    )?;
    if call.kind != NetworkCallKind::Gateway {
        write!(out, " kind={}", call.kind.name())?;
    }
    if call.subagent_id == 0 {
        out.push_str(" source=parent");
    } else {
        write!(out, " source=subagent#{}", call.subagent_id)?;
    }
    if !call.error.is_empty() {
        write!(out, " err={}", call.error)?;
    } else if call.status != 0 {
        write!(out, " status={}", call.status)?;
    }
    write!(
        out,
        " duration={}ms bytes={}",
        call.duration_ms, call.response_bytes
    )?;
    if call.input_tokens > 0 || call.output_tokens > 0 {
        write!(out, " tokens={}->{}", call.input_tokens, call.output_tokens)?;
    }
    if !call.stop_reason.is_empty() {
        write!(out, " stop_reason={}", call.stop_reason)?;
    }
    if call.turn_id != 0 {
        write!(out, " turn={}", call.turn_id)?;
    }
    if call.step_id != 0 {
        write!(out, " step={}", call.step_id)?;
    }
    out.push('\n');
    Ok(())
}

#[cfg(test)]
mod tests;
