use std::fmt::{self, Write as _};

use super::{masked_prefix, tool_display_name};
use ofx_agent::InterruptedTurn;

const ARGUMENT_BYTES: usize = 1200;

pub(super) fn write_problems(
    out: &mut String,
    turn: Option<&InterruptedTurn>,
) -> Result<usize, fmt::Error> {
    let Some(turn) = turn else {
        return Ok(0);
    };
    out.push_str("- interrupted turn");
    if let Some(call) = &turn.in_flight {
        write!(out, " in_flight_tool={}", tool_display_name(&call.name))?;
    }
    if !turn.completed_tools.is_empty() {
        write!(out, " completed_tools={}", turn.completed_tools.len())?;
    }
    out.push('\n');
    Ok(1)
}

pub(super) fn write_section(out: &mut String, turn: Option<&InterruptedTurn>) -> fmt::Result {
    let Some(turn) = turn else {
        return Ok(());
    };
    out.push_str("\n## Interrupted Turn\nlast turn was interrupted by user\n");
    if let Some(call) = &turn.in_flight {
        writeln!(out, "  in_flight_tool: {}", tool_display_name(&call.name))?;
        if !call.arguments.is_empty() {
            let (shown, left_out) = masked_prefix(&call.arguments, ARGUMENT_BYTES);
            writeln!(out, "  in_flight_args:\n    {shown}")?;
            if left_out > 0 {
                writeln!(out, "    ... (truncated, {left_out} more bytes)")?;
            }
        }
    }
    if !turn.completed_tools.is_empty() {
        write!(out, "  completed_tools ({}):", turn.completed_tools.len())?;
        for name in &turn.completed_tools {
            write!(out, " {}", tool_display_name(name))?;
        }
        out.push('\n');
    }
    Ok(())
}

#[cfg(test)]
mod tests;
